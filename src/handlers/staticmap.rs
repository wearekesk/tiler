//! The `/staticmap` handler: renders a composed PNG/SVG/JPEG map (like the
//! Google Static Maps API) from the server-configured PMTiles archive.
//!
//! `GET /staticmap?size=WxH
//!      [&center=LAT,LON&zoom=Z][&scale=1|2|4][&format=png|svg|jpeg]
//!      [&markers=[color:C|label:L|size:S|]LAT,LON|LAT,LON..]* (repeatable)
//!      [&path=[color:C|weight:W|fillcolor:C|]LAT,LON|LAT,LON..|enc:POLYLINE]* (repeatable)
//!      [&style=feature:F|color:C|weight:W]* (repeatable)
//!      [&visible=LAT,LON|LAT,LON..]* (repeatable)`
//!
//! The tile source is fixed by the `PMTILES_URL` variable. If `center`+`zoom`
//! are omitted, the viewport is auto-fit to all `visible`/`markers`/`path`
//! points (like Google's `visible` parameter).

use geo::algorithm::line_measures::{Haversine, Length};
use geo_types::LineString;
use poem::http::{HeaderValue, StatusCode};
use poem::{handler, Request, Response};

use crate::config;
use crate::geo_util::{self, Viewport};
use crate::params::{self, QueryMap};
use crate::render::{self, render_svg};
use crate::tiles;

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

/// Caps how many renders rasterize at once. Each can allocate a pixmap up to
/// `MAX_OUTPUT_DIM`² RGBA (~256 MiB), so without a gate a burst of large
/// `size`×`scale` requests could exhaust memory even though each is individually
/// capped. Rasterization is CPU-bound, so bounding concurrency to the core count
/// also keeps the blocking pool from thrashing. Requests past the limit wait for
/// a permit (they've only parsed cheap params at this point, so queueing is
/// cheap) rather than being rejected.
fn render_gate() -> &'static tokio::sync::Semaphore {
    use std::sync::OnceLock;
    static GATE: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    GATE.get_or_init(|| {
        let permits = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .max(2);
        tokio::sync::Semaphore::new(permits)
    })
}

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

fn bad_request(msg: impl std::fmt::Display) -> poem::Error {
    poem::Error::from_string(msg.to_string(), StatusCode::BAD_REQUEST)
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
pub async fn static_map(req: &Request) -> poem::Result<Response> {
    let query = req.uri().query().unwrap_or("");
    let qm = QueryMap::parse(query);

    // The tile source is fixed by the server (`PMTILES_URL`); clients cannot
    // choose it, which removes any SSRF / local-file-read surface.
    let pmtiles = config::pmtiles_url()
        .map(|s| s.to_string())
        .ok_or_else(|| internal_error("PMTILES_URL is not configured"))?;

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
        f @ ("png" | "svg" | "jpeg" | "jpg") => f.to_string(),
        other => {
            return Err(bad_request(format!(
                "invalid format '{other}': expected 'png', 'svg', or 'jpeg'"
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
            match (center, zoom) {
                // `zoom` given but no `center`: keep the requested zoom, just
                // auto-center on the points instead of also auto-fitting zoom.
                (None, Some(z)) => {
                    let (lat, lon) = geo_util::center_of(&fit_points);
                    Viewport::new(lat, lon, z, width, height)
                }
                // `center` given but no `zoom`: keep the requested center and
                // only auto-fit the zoom so all points are visible.
                (Some((lat, lon)), None) => {
                    Viewport::fit_at_center(lat, lon, &fit_points, width, height)
                }
                _ => Viewport::fit(&fit_points, width, height),
            }
        }
    };

    let source = tiles::get_source(&pmtiles)
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
    // round-trips would dominate latency. Each task returns `(tile, had_error)`.
    // `Ok(None)` (a tile that simply isn't in the archive) is normal and not an
    // error; a fetch/decode failure is. We still render what we got, but a
    // degraded render must not be cached long-term (see the cache logic below).
    let mut set = tokio::task::JoinSet::new();
    for (z, x, y) in covering {
        let source = source.clone();
        set.spawn(async move {
            match source.get_tile(z, x, y).await {
                Ok(Some(bytes)) => match tiles::decode_tile(bytes.to_vec()) {
                    Ok(layers) => (Some(((z, x, y), layers)), false),
                    Err(e) => {
                        tracing::warn!(z, x, y, error = %e, "failed to decode tile");
                        (None, true)
                    }
                },
                Ok(None) => (None, false),
                Err(e) => {
                    tracing::warn!(z, x, y, error = %e, "failed to fetch tile");
                    (None, true)
                }
            }
        });
    }

    let mut tiles = Vec::new();
    let mut had_tile_error = false;
    while let Some(res) = set.join_next().await {
        match res {
            Ok((Some(tile), err)) => {
                tiles.push(tile);
                had_tile_error |= err;
            }
            Ok((None, err)) => had_tile_error |= err,
            // A panicked/cancelled fetch task also counts as a degraded render.
            Err(_) => had_tile_error = true,
        }
    }
    // Restore a deterministic order (JoinSet completes out of order) so tiles
    // composite identically across requests.
    tiles.sort_by_key(|((z, x, y), _)| (*z, *x, *y));

    // Compute the longest route's length (by measured distance) before `paths`
    // is moved into the render task below.
    let longest_meters = paths
        .iter()
        .filter(|p| p.points.len() >= 2)
        .map(|p| {
            let line: LineString<f64> = p.points.iter().map(|(lat, lon)| (*lon, *lat)).collect();
            Haversine.length(&line)
        })
        .fold(f64::NEG_INFINITY, f64::max);

    // Bound the number of concurrent renders so a burst of large requests can't
    // exhaust memory (see `render_gate`). Held across the whole render below.
    let _permit = render_gate()
        .acquire()
        .await
        .map_err(|e| internal_error(format!("render gate closed: {e}")))?;

    // Rendering the SVG and rasterizing it are both CPU-bound (many tiles, up to
    // an 8192x8192 pixmap). Do all of it on the blocking pool so the async
    // worker thread stays free, and so the large SVG string never crosses a
    // thread boundary.
    let (bytes, content_type) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let svg = render_svg(
            &viewport,
            &tiles,
            &overrides,
            &marker_groups,
            &paths,
            out_width,
            out_height,
        );
        let out: (Vec<u8>, &'static str) = match format.as_str() {
            "svg" => (svg.into_bytes(), "image/svg+xml"),
            // Quality 100: maximum fidelity (map imagery has sharp edges/text).
            "jpeg" | "jpg" => (
                render::svg_to_jpeg(&svg, out_width, out_height, 100)?,
                "image/jpeg",
            ),
            _ => (
                render::svg_to_png(&svg, out_width, out_height)?,
                "image/png",
            ),
        };
        Ok(out)
    })
    .await
    .map_err(|e| internal_error(format!("render task panicked: {e}")))?
    .map_err(|e| internal_error(format!("failed to render image: {e}")))?;

    let mut response = Response::builder().content_type(content_type).body(bytes);

    {
        let headers = response.headers_mut();
        if had_tile_error {
            // A tile fetch/decode failed, so this render may be incomplete —
            // don't let it be cached long-term with a durable ETag.
            headers.insert("cache-control", "no-store".parse().unwrap());
        } else {
            headers.insert("cache-control", CACHE_CONTROL.parse().unwrap());
            headers.insert("etag", etag.parse().unwrap());
        }
    }

    if longest_meters.is_finite() {
        // `f64 as u64` saturates (>= 0, NaN -> 0), and `HeaderValue::from` for an
        // integer is infallible — no string round-trip needed.
        let meters = longest_meters.round() as u64;
        response
            .headers_mut()
            .insert("x-route-length-meters", HeaderValue::from(meters));
    }

    Ok(response)
}
