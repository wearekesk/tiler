//! A raw PMTiles tile server, mounted at `/tiles`. Ported from the Cloudflare
//! Worker version: it serves `z/x/y` tiles and a `.json` TileJSON for archives
//! selected by name.
//!
//! `GET /tiles/{name}/{z}/{x}/{y}.{ext}` — a single tile.
//! `GET /tiles/{name}.json` — the archive's TileJSON.
//!
//! `name` may contain `/` (nested archives). It is resolved to a PMTiles source
//! via a matching `PMTILES_ALIASES` entry; a name with no alias is not found.
//! Path traversal (`..`) and other unsafe names are rejected.

use poem::http::{header, Method, StatusCode};
use poem::{handler, Body, Request, Response};

use crate::config;
use crate::tiles::{get_source, tile_content_type, tile_extension};
use pmtiles::TileType;

/// Resolves an archive `name` to a PMTiles source via `PMTILES_ALIASES`, or
/// `None` if no alias matches. Aliases are the only way to serve an archive, so
/// an unknown name is simply not found (rather than mapped to some file path).
fn resolve_source(name: &str) -> Option<&'static str> {
    config::pmtiles_aliases().get(name).map(String::as_str)
}

/// Rejects names that could escape the intended archive space (path traversal)
/// or contain unexpected characters.
fn name_is_safe(name: &str) -> bool {
    !name.is_empty()
        && name
            .split('/')
            .all(|seg| !seg.is_empty() && seg != ".." && seg != ".")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
}

struct Parsed {
    name: String,
    /// `Some((z, x, y))` for a tile request; `None` for a `.json` request.
    tile: Option<(u8, u32, u32)>,
    ext: String,
}

/// Parses the path after `/tiles`, e.g. `/name/z/x/y.ext` or `/name.json`.
fn parse_tile_path(path: &str) -> Option<Parsed> {
    let path = path.strip_prefix('/').unwrap_or(path);
    if path.is_empty() {
        return None;
    }

    if let Some(name) = path.strip_suffix(".json") {
        return name_is_safe(name).then(|| Parsed {
            name: name.to_string(),
            tile: None,
            ext: "json".to_string(),
        });
    }

    // `name.../z/x/y.ext`: the last three `/`-segments are z, x, y.ext.
    let segs: Vec<&str> = path.split('/').collect();
    if segs.len() < 4 {
        return None;
    }
    let (y_str, ext) = segs.last()?.rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }
    let z: u8 = segs[segs.len() - 3].parse().ok()?;
    let x: u32 = segs[segs.len() - 2].parse().ok()?;
    let y: u32 = y_str.parse().ok()?;
    let name = segs[..segs.len() - 3].join("/");
    name_is_safe(&name).then(|| Parsed {
        name,
        tile: Some((z, x, y)),
        ext: ext.to_string(),
    })
}

/// The tile type a requested extension implies, if any.
fn ext_to_type(ext: &str) -> Option<TileType> {
    match ext {
        "mvt" | "pbf" => Some(TileType::Mvt),
        "png" => Some(TileType::Png),
        "jpg" | "jpeg" => Some(TileType::Jpeg),
        "webp" => Some(TileType::Webp),
        _ => None,
    }
}

fn text(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .content_type("text/plain; charset=utf-8")
        .body(msg.to_string())
}

/// Finalizes a response with the tile cache-control header. (CORS is handled by
/// the `Cors` middleware at the app level.)
fn finish(mut resp: Response) -> Response {
    // A misconfigured (e.g. non-ASCII) `TILES_CACHE_CONTROL` must not crash the
    // server; fall back to a safe default header value instead of panicking.
    let cache_control = config::tiles_cache_control()
        .parse()
        .unwrap_or_else(|_| header::HeaderValue::from_static("public, max-age=86400"));
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, cache_control);
    resp
}

#[handler]
pub async fn serve(req: &Request) -> Response {
    if req.method() == Method::POST {
        return text(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
    }

    // The route is mounted at `/tiles`, so strip that prefix to get the archive
    // path. The wildcard capture ("/tiles/*") leaves the remainder in the path.
    let path = req.uri().path();
    let rest = path.strip_prefix("/tiles").unwrap_or("");

    let Some(parsed) = parse_tile_path(rest) else {
        return text(StatusCode::NOT_FOUND, "Invalid URL");
    };

    // No alias for this name: nothing to serve. This is an ordinary "not found",
    // not an error worth logging. Return it *without* `finish` so the long-lived
    // tile `Cache-Control` isn't applied — an operator adding the alias and
    // restarting must not be defeated by a 404 cached for a day.
    let Some(resolved) = resolve_source(&parsed.name) else {
        return text(StatusCode::NOT_FOUND, "Unknown archive");
    };
    let source = match get_source(resolved).await {
        Ok(s) => s,
        // Opening can fail for reasons that are *not* "the name is wrong":
        // a TLS/network error reaching a remote archive, an upstream 5xx, or a
        // timeout. The client still gets a generic 404, but we log the real
        // cause (with the resolved source) so operators can tell these apart
        // instead of guessing at an opaque "Archive not found". No `finish`
        // here either: a transient failure must not be cached for a day.
        Err(e) => {
            tracing::warn!(
                source = %resolved,
                error = %format!("{e:#}"),
                "failed to open PMTiles source"
            );
            return text(StatusCode::NOT_FOUND, "Archive not found");
        }
    };
    let (tile_type, min_zoom, max_zoom) = source.header_info();

    // TileJSON.
    let Some((z, x, y)) = parsed.tile else {
        // The TileJSON `tiles` URL must be absolute. Without a configured
        // `PUBLIC_HOSTNAME` or a `Host` header we'd emit `https:///tiles/...`,
        // which is unusable; reject rather than hand back a malformed template.
        let Some(host) = config::public_hostname().map(str::to_string).or_else(|| {
            req.headers()
                .get(header::HOST)
                .and_then(|h| h.to_str().ok())
                .filter(|h| !h.is_empty())
                .map(str::to_string)
        }) else {
            return finish(text(
                StatusCode::BAD_REQUEST,
                "cannot build TileJSON: no Host header and PUBLIC_HOSTNAME is not set",
            ));
        };
        let ext = {
            let e = tile_extension(tile_type);
            if e.is_empty() {
                "mvt"
            } else {
                e
            }
        };
        // Build an absolute tiles URL. If `PUBLIC_HOSTNAME` already carries a
        // scheme, honor it verbatim; otherwise pick one from `X-Forwarded-Proto`
        // (set by a TLS-terminating proxy), falling back to `http` for loopback
        // and `https` elsewhere so local dev over plain HTTP still works.
        let base = if host.contains("://") {
            format!("{host}/tiles/{}", parsed.name)
        } else {
            let scheme = req
                .headers()
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.split(',').next().unwrap_or(s).trim())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    if host.starts_with("localhost")
                        || host.starts_with("127.0.0.1")
                        || host.starts_with("[::1]")
                    {
                        "http"
                    } else {
                        "https"
                    }
                });
            format!("{scheme}://{host}/tiles/{}", parsed.name)
        };
        let tile_url = format!("{base}/{{z}}/{{x}}/{{y}}.{ext}");
        return match source.tilejson_string(tile_url).await {
            Ok(json) => finish(
                Response::builder()
                    .content_type("application/json")
                    .body(json),
            ),
            Err(e) => text(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to build tilejson: {e}"),
            ),
        };
    };

    // A tile request outside the archive's zoom range: 404.
    if z < min_zoom || z > max_zoom {
        return finish(text(StatusCode::NOT_FOUND, "Tile not found"));
    }

    // Reject a mismatched extension for a known-type archive (e.g. `.png` from
    // an MVT archive).
    if ext_to_type(&parsed.ext) != Some(tile_type) && !tile_extension(tile_type).is_empty() {
        return finish(text(
            StatusCode::BAD_REQUEST,
            &format!(
                "requested .{} but archive has type .{}",
                parsed.ext,
                tile_extension(tile_type)
            ),
        ));
    }

    match source.get_tile(z, x, y).await {
        Ok(Some(bytes)) => finish(
            Response::builder()
                .content_type(tile_content_type(tile_type))
                .body(Body::from_bytes(bytes)),
        ),
        // In range but no tile stored here: empty 204.
        Ok(None) => finish(Response::builder().status(StatusCode::NO_CONTENT).finish()),
        Err(e) => text(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to read tile: {e}"),
        ),
    }
}
