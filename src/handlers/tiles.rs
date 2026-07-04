//! A raw PMTiles tile server, mounted at `/tiles`. Ported from the Cloudflare
//! Worker version: it serves `z/x/y` tiles and a `.json` TileJSON for archives
//! selected by name.
//!
//! `GET /tiles/{name}/{z}/{x}/{y}.{ext}` — a single tile.
//! `GET /tiles/{name}.json` — the archive's TileJSON.
//!
//! `name` may contain `/` (nested archives). It is resolved to a PMTiles source
//! via the `PMTILES_PATH` template (`{name}` is substituted; default
//! `{name}.pmtiles`). Path traversal (`..`) and other unsafe names are rejected.

use poem::http::{header, Method, StatusCode};
use poem::{handler, Body, Request, Response};

use crate::config;
use crate::tiles::{get_source, tile_content_type, tile_extension};
use pmtiles::TileType;

/// Resolves an archive `name` to a PMTiles source string using `PMTILES_PATH`.
fn resolve_source(name: &str) -> String {
    match config::pmtiles_path() {
        Some(tmpl) => tmpl.replace("{name}", name),
        None => format!("{name}.pmtiles"),
    }
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
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        config::tiles_cache_control().parse().unwrap(),
    );
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

    let source = match get_source(&resolve_source(&parsed.name)).await {
        Ok(s) => s,
        Err(_) => return finish(text(StatusCode::NOT_FOUND, "Archive not found")),
    };
    let (tile_type, min_zoom, max_zoom) = source.header_info();

    // TileJSON.
    let Some((z, x, y)) = parsed.tile else {
        let host = config::public_hostname()
            .map(str::to_string)
            .or_else(|| {
                req.headers()
                    .get(header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let ext = {
            let e = tile_extension(tile_type);
            if e.is_empty() {
                "mvt"
            } else {
                e
            }
        };
        let tile_url = format!(
            "https://{host}/tiles/{}/{{z}}/{{x}}/{{y}}.{ext}",
            parsed.name
        );
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
