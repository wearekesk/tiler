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

use geo::algorithm::line_measures::{Haversine, Length};
use geo_types::LineString;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use poem::http::StatusCode;
use poem::listener::TcpListener;
use poem::middleware::Tracing;
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

/// A weak-ish content ETag derived from the request's query string, which
/// uniquely determines the rendered output.
fn etag_for_query(query: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    query.hash(&mut h);
    format!("\"{:016x}\"", h.finish())
}

fn bad_request(msg: impl std::fmt::Display) -> poem::Error {
    poem::Error::from_string(msg.to_string(), StatusCode::BAD_REQUEST)
}

fn internal_error(msg: impl std::fmt::Display) -> poem::Error {
    poem::Error::from_string(msg.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
}

fn parse_latlon(s: &str) -> poem::Result<(f64, f64)> {
    let mut parts = s.split(',');
    let lat = parts
        .next()
        .ok_or_else(|| bad_request("missing latitude"))?
        .trim()
        .parse::<f64>()
        .map_err(|e| bad_request(format!("invalid latitude: {e}")))?;
    let lon = parts
        .next()
        .ok_or_else(|| bad_request("missing longitude"))?
        .trim()
        .parse::<f64>()
        .map_err(|e| bad_request(format!("invalid longitude: {e}")))?;
    Ok((lat, lon))
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
    let etag = etag_for_query(query);

    // Short-circuit unchanged renders: if the client already holds this exact
    // output, skip all fetching/rasterization and return 304.
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

    let qm = QueryMap::parse(query);

    let pmtiles = qm
        .get_one("pmtiles")
        .map(|s| s.to_string())
        .or_else(|| std::env::var("PMTILES_URL").ok())
        .ok_or_else(|| {
            bad_request("missing 'pmtiles' parameter (and no PMTILES_URL env var set)")
        })?;

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

    let format = qm.get_one("format").unwrap_or("png").to_string();

    let center: Option<(f64, f64)> = qm.get_one("center").map(parse_latlon).transpose()?;
    let zoom: Option<u8> = match qm.get_one("zoom") {
        Some(s) => Some(
            s.parse()
                .map_err(|e| bad_request(format!("invalid zoom: {e}")))?,
        ),
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

    let source = TileSource::open(&pmtiles)
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

    let mut tiles = Vec::new();
    for (z, x, y) in covering {
        match source.get_tile(z, x, y).await {
            Ok(Some(bytes)) => {
                if let Ok(layers) = tiles::decode_tile(bytes.to_vec()) {
                    tiles.push(((z, x, y), layers));
                }
            }
            Ok(None) => {}
            Err(e) => {
                // Skip tiles that fail to fetch rather than failing the whole request.
                tracing::warn!(z, x, y, error = %e, "failed to fetch tile");
            }
        }
    }

    let out_width = width * scale;
    let out_height = height * scale;
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
        let png = render::svg_to_png(&svg, out_width, out_height)
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
            let meters = Haversine.length(&line);
            response.headers_mut().insert(
                "x-route-length-meters",
                meters.round().to_string().parse().unwrap(),
            );
        }
    }

    Ok(response)
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    dotenv::dotenv().ok();
    // Kept alive for the process lifetime so buffered spans keep flushing.
    let _telemetry = init_telemetry();

    let app = Route::new()
        .at("/staticmap", get(static_map))
        .with(Tracing);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    tracing::info!(port, "{SERVICE_NAME} listening on http://0.0.0.0:{port}/staticmap");
    Server::new(TcpListener::bind(format!("0.0.0.0:{port}")))
        .run(app)
        .await
}
