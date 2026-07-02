//! A "static maps" HTTP endpoint (similar in spirit to the Google Maps
//! Static API), backed by a PMTiles vector tile archive.
//!
//! `GET /staticmap?size=WxH
//!      [&pmtiles=PATH_OR_URL]
//!      [&center=LAT,LON&zoom=Z][&scale=1|2|4][&format=png|svg]
//!      [&markers=[color:C|label:L|size:S|]LAT,LON|LAT,LON..]* (repeatable)
//!      [&path=[color:C|weight:W|fillcolor:C|]LAT,LON|LAT,LON..|enc:POLYLINE]* (repeatable)
//!      [&style=feature:F|color:C|weight:W]* (repeatable)
//!      [&visible=LAT,LON|LAT,LON..]* (repeatable)`
//!
//! `pmtiles` may be omitted if a `PMTILES_URL` variable is set (via the
//! environment, or a `.env` file loaded at startup with the `dotenv` crate).
//!
//! If `center`+`zoom` are omitted, the viewport is automatically fit to
//! contain all `visible`, `markers`, and `path` points (like Google's
//! `visible` parameter).

mod geo_util;
mod params;
mod render;
mod style;
mod tiles;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use geo::algorithm::line_measures::{Haversine, Length};
use geo_types::LineString;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use poem::http::{HeaderValue, StatusCode};
use poem::listener::TcpListener;
use poem::middleware::{CatchPanic, Tracing};
use poem::{get, handler, EndpointExt, Request, Response, Route, Server};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

use geo_util::Viewport;
use params::QueryMap;
use render::render_svg;
use tiles::TileSource;

const SERVICE_NAME: &str = "kesk-tiler";

/// Initializes `tracing` (structured logs + spans). When
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set, spans are also exported to that
/// OpenTelemetry collector over OTLP/gRPC. The log level is controlled by
/// `RUST_LOG` (default `info`). Returns the tracer provider (if OTLP is
/// enabled) so the caller can keep it alive for the process lifetime.
fn init_telemetry() -> Option<SdkTracerProvider> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let provider = if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        match opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build()
        {
            Ok(exporter) => Some(
                SdkTracerProvider::builder()
                    .with_batch_exporter(exporter)
                    .with_resource(Resource::builder().with_service_name(SERVICE_NAME).build())
                    .build(),
            ),
            Err(e) => {
                eprintln!("OTLP exporter init failed, continuing without export: {e}");
                None
            }
        }
    } else {
        None
    };

    let otel_layer = provider.as_ref().map(|p| {
        opentelemetry::global::set_tracer_provider(p.clone());
        tracing_opentelemetry::layer().with_tracer(p.tracer(SERVICE_NAME))
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(otel_layer)
        .init();

    provider
}

/// Aggressive caching: a render is fully determined by its query string (and
/// the immutable PMTiles source), so responses are safe to cache for a long
/// time. `immutable` tells browsers never to revalidate within the window.
const CACHE_CONTROL: &str = "public, max-age=86400, s-maxage=604800, immutable";

/// Upper bound on each *scaled* output dimension (px). Caps pixmap allocation
/// so a large `size` x `scale` can't exhaust memory (8192x8192 RGBA ~= 256 MiB).
const MAX_OUTPUT_DIM: u32 = 8192;

/// Upper bound on the `zoom` parameter. The tile math shifts by `zoom`, so an
/// unbounded value overflows and panics; no real tileset needs this many.
const MAX_ZOOM: u8 = 24;

/// Content ETag for a request. Folds in the query string, the resolved tile
/// source, and the renderer version so a `304 Not Modified` can't serve stale
/// output after the source or the code changes.
fn etag_for_request(query: &str, source: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    query.hash(&mut h);
    source.hash(&mut h);
    env!("CARGO_PKG_VERSION").hash(&mut h);
    format!("\"{:016x}\"", h.finish())
}

/// Reads a boolean-ish environment flag (`1`/`true`/`yes`, case-insensitive).
fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
        })
        .unwrap_or(false)
}

/// Process-wide cache of opened tile sources, keyed by source string.
/// Opening a PMTiles archive reads its header + root directory (a network
/// round-trip for remote URLs), and the reader is thread-safe and reusable
/// with its own internal caches — so open each source once and share it.
type SourceCache = tokio::sync::Mutex<HashMap<String, Arc<TileSource>>>;

/// Cap on the number of distinct sources kept open. With `PMTILES_ALLOW_PARAM`
/// enabled a client could otherwise request unbounded unique sources and
/// exhaust memory; past the cap we evict an existing entry (in-flight requests
/// keep their own `Arc`, so eviction is safe).
const MAX_CACHED_SOURCES: usize = 64;

async fn get_source(pmtiles: &str) -> anyhow::Result<Arc<TileSource>> {
    static SOURCES: OnceLock<SourceCache> = OnceLock::new();
    let cache = SOURCES.get_or_init(Default::default);

    if let Some(existing) = cache.lock().await.get(pmtiles) {
        return Ok(existing.clone());
    }
    // Open outside the lock so a slow open doesn't block other sources; if a
    // concurrent request opened the same source first, keep that one.
    let opened = Arc::new(TileSource::open(pmtiles).await?);
    let mut map = cache.lock().await;
    if !map.contains_key(pmtiles) && map.len() >= MAX_CACHED_SOURCES {
        if let Some(evict) = map.keys().next().cloned() {
            map.remove(&evict);
        }
    }
    Ok(map.entry(pmtiles.to_string()).or_insert(opened).clone())
}

fn bad_request(msg: impl std::fmt::Display) -> poem::Error {
    poem::Error::from_string(msg.to_string(), StatusCode::BAD_REQUEST)
}

fn forbidden(msg: impl std::fmt::Display) -> poem::Error {
    poem::Error::from_string(msg.to_string(), StatusCode::FORBIDDEN)
}

fn internal_error(msg: impl std::fmt::Display) -> poem::Error {
    poem::Error::from_string(msg.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
}

fn parse_size(s: &str) -> poem::Result<(u32, u32)> {
    let mut parts = s.split('x');
    let w = parts
        .next()
        .ok_or_else(|| bad_request("missing width"))?
        .trim()
        .parse::<u32>()
        .map_err(|e| bad_request(format!("invalid width: {e}")))?;
    let h = parts
        .next()
        .ok_or_else(|| bad_request("missing height"))?
        .trim()
        .parse::<u32>()
        .map_err(|e| bad_request(format!("invalid height: {e}")))?;
    Ok((w, h))
}

#[handler]
async fn static_map(req: &Request) -> poem::Result<Response> {
    let query = req.uri().query().unwrap_or("");
    let qm = QueryMap::parse(query);

    // Resolve the tile source. The `pmtiles` query parameter can name an
    // arbitrary path or URL, which would let an unauthenticated client read
    // local files or reach internal services (SSRF). It is therefore only
    // honored when `PMTILES_ALLOW_PARAM` is explicitly enabled; otherwise the
    // server-configured `PMTILES_URL` is the only source.
    let pmtiles = match qm.get_one("pmtiles") {
        Some(_) if !env_flag("PMTILES_ALLOW_PARAM") => {
            return Err(forbidden(
                "the 'pmtiles' parameter is disabled; set PMTILES_ALLOW_PARAM=1 to allow it",
            ));
        }
        Some(p) => p.to_string(),
        None => std::env::var("PMTILES_URL").ok().ok_or_else(|| {
            bad_request("missing 'pmtiles' parameter (and no PMTILES_URL env var set)")
        })?,
    };

    // Short-circuit unchanged renders: if the client already holds this exact
    // output, skip all fetching/rasterization and return 304.
    let etag = etag_for_request(query, &pmtiles);
    if req
        .headers()
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        == Some(etag.as_str())
    {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header("etag", etag)
            .header("cache-control", CACHE_CONTROL)
            .finish());
    }

    let (width, height) = parse_size(
        qm.get_one("size")
            .ok_or_else(|| bad_request("missing 'size' parameter"))?,
    )?;
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err(bad_request("size must be between 1x1 and 4096x4096"));
    }

    let scale: u32 = match qm.get_one("scale") {
        Some(s) => s
            .parse()
            .map_err(|e| bad_request(format!("invalid scale: {e}")))?,
        None => 1,
    };
    if ![1, 2, 4].contains(&scale) {
        return Err(bad_request("scale must be 1, 2, or 4"));
    }

    // Guard against oversized allocations: an unguarded `size=4096x4096&scale=4`
    // would demand a 16384x16384 pixmap (~1 GiB) and could OOM the process.
    let out_width = width * scale;
    let out_height = height * scale;
    if out_width > MAX_OUTPUT_DIM || out_height > MAX_OUTPUT_DIM {
        return Err(bad_request(format!(
            "scaled output {out_width}x{out_height} exceeds {MAX_OUTPUT_DIM}x{MAX_OUTPUT_DIM}; reduce size or scale"
        )));
    }

    let format = match qm.get_one("format").unwrap_or("png") {
        f @ ("png" | "svg") => f.to_string(),
        other => {
            return Err(bad_request(format!(
                "invalid format '{other}': expected 'png' or 'svg'"
            )))
        }
    };

    let center: Option<(f64, f64)> = qm.get_one("center").map(params::parse_latlon).transpose()?;
    let zoom: Option<u8> = match qm.get_one("zoom") {
        Some(s) => {
            let z: u8 = s
                .parse()
                .map_err(|e| bad_request(format!("invalid zoom: {e}")))?;
            // Bound the zoom: the tile math shifts by `zoom`, so an unbounded
            // value would overflow `1 << zoom` and panic (DoS). No real tileset
            // goes anywhere near this.
            if z > MAX_ZOOM {
                return Err(bad_request(format!(
                    "zoom must be between 0 and {MAX_ZOOM}"
                )));
            }
            Some(z)
        }
        None => None,
    };

    let marker_groups = params::parse_marker_groups(qm.get_all("markers"))?;
    let paths = params::parse_path_specs(qm.get_all("path"))?;
    let overrides = params::parse_style_overrides(qm.get_all("style"))?;
    let visible_points = params::parse_visible(qm.get_all("visible"))?;

    let viewport = match (center, zoom) {
        (Some((lat, lon)), Some(z)) => Viewport::new(lat, lon, z, width, height),
        _ => {
            let mut fit_points = visible_points;
            if let Some(c) = center {
                fit_points.push(c);
            }
            for g in &marker_groups {
                fit_points.extend(g.points.iter().copied());
            }
            for p in &paths {
                fit_points.extend(p.points.iter().copied());
            }
            if fit_points.is_empty() {
                return Err(bad_request(
                    "provide either 'center'+'zoom', or 'visible'/'markers'/'path' points to auto-fit the viewport",
                ));
            }
            Viewport::fit(&fit_points, width, height)
        }
    };

    let source = get_source(&pmtiles)
        .await
        .map_err(|e| internal_error(format!("failed to open pmtiles source: {e}")))?;

    let covering = viewport.covering_tiles();
    tracing::debug!(
        zoom = viewport.zoom,
        tiles = covering.len(),
        width,
        height,
        "rendering static map"
    );

    // Fetch tiles concurrently: with a remote PMTiles URL, sequential
    // round-trips would dominate latency. A tile that fails to fetch is skipped
    // rather than failing the whole request.
    let mut set = tokio::task::JoinSet::new();
    for (z, x, y) in covering {
        let source = source.clone();
        set.spawn(async move {
            match source.get_tile(z, x, y).await {
                Ok(Some(bytes)) => tiles::decode_tile(bytes.to_vec())
                    .ok()
                    .map(|layers| ((z, x, y), layers)),
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(z, x, y, error = %e, "failed to fetch tile");
                    None
                }
            }
        });
    }

    let mut tiles = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(Some(tile)) = res {
            tiles.push(tile);
        }
    }
    // Restore a deterministic order (JoinSet completes out of order) so tiles
    // composite identically across requests.
    tiles.sort_by_key(|((z, x, y), _)| (*z, *x, *y));

    let svg = render_svg(
        &viewport,
        &tiles,
        &overrides,
        &marker_groups,
        &paths,
        out_width,
        out_height,
    );

    let mut response = if format == "svg" {
        Response::builder().content_type("image/svg+xml").body(svg)
    } else {
        // Rasterization is CPU-bound (SVG parse + render up to 8192x8192); run
        // it on the blocking pool so it doesn't stall the async worker thread
        // and starve other in-flight requests.
        let png =
            tokio::task::spawn_blocking(move || render::svg_to_png(&svg, out_width, out_height))
                .await
                .map_err(|e| internal_error(format!("rasterization task panicked: {e}")))?
                .map_err(|e| internal_error(format!("failed to rasterize png: {e}")))?;
        Response::builder().content_type("image/png").body(png)
    };

    {
        let headers = response.headers_mut();
        headers.insert("cache-control", CACHE_CONTROL.parse().unwrap());
        headers.insert("etag", etag.parse().unwrap());
    }

    let longest_path = paths.iter().max_by_key(|p| p.points.len());
    if let Some(p) = longest_path {
        if p.points.len() >= 2 {
            let line: LineString<f64> = p.points.iter().map(|(lat, lon)| (*lon, *lat)).collect();
            // `f64 as u64` saturates (>= 0, NaN -> 0), and `HeaderValue::from`
            // for an integer is infallible — no string round-trip needed.
            let meters = Haversine.length(&line).round() as u64;
            response
                .headers_mut()
                .insert("x-route-length-meters", HeaderValue::from(meters));
        }
    }

    Ok(response)
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    dotenv::dotenv().ok();
    // Kept alive for the process lifetime so buffered spans keep flushing.
    let _telemetry = init_telemetry();

    // Load the system font database now so the first render doesn't pay for it.
    render::warm_fontdb();

    // `CatchPanic` (innermost) turns any handler panic into a 500 instead of
    // killing the worker task; `Tracing` (outermost) still logs the response.
    let app = Route::new()
        .at("/staticmap", get(static_map))
        .with(CatchPanic::new())
        .with(Tracing);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    tracing::info!(
        port,
        "{SERVICE_NAME} listening on http://0.0.0.0:{port}/staticmap"
    );
    Server::new(TcpListener::bind(format!("0.0.0.0:{port}")))
        .run(app)
        .await
}
