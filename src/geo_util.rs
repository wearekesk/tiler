//! Web Mercator (EPSG:3857) math used to place PMTiles tiles and lat/lon
//! points onto the output image canvas.

const TILE_SIZE: f64 = 256.0;

pub fn tile_size_at_zoom(zoom: u8) -> f64 {
    TILE_SIZE * 2f64.powi(zoom as i32)
}

/// Longitude (degrees) to a zoom-independent fraction in `[0, 1)`.
pub fn lon_to_frac(lon: f64) -> f64 {
    (lon + 180.0) / 360.0
}

/// The maximum absolute latitude representable in Web Mercator; beyond this the
/// projection diverges (division by zero / `ln(0)`).
pub const MAX_MERCATOR_LAT: f64 = 85.051_128_78;

/// Latitude (degrees) to a zoom-independent Web Mercator fraction in `[0, 1)`.
/// Latitude is clamped to the Web Mercator limit so `±90` (or anything beyond)
/// can't produce `NaN`/`Infinity` in the projection math.
pub fn lat_to_frac(lat: f64) -> f64 {
    let lat = lat.clamp(-MAX_MERCATOR_LAT, MAX_MERCATOR_LAT);
    let lat_rad = lat.to_radians();
    0.5 - ((1.0 + lat_rad.sin()) / (1.0 - lat_rad.sin())).ln() / (4.0 * std::f64::consts::PI)
}

/// Inverse of [`lat_to_frac`].
pub fn frac_to_lat(frac: f64) -> f64 {
    let n = std::f64::consts::PI * (1.0 - 2.0 * frac);
    n.sinh().atan().to_degrees()
}

/// Longitude (degrees) to world pixel X at the given zoom level.
pub fn lon_to_x(lon: f64, zoom: u8) -> f64 {
    lon_to_frac(lon) * tile_size_at_zoom(zoom)
}

/// Latitude (degrees) to world pixel Y at the given zoom level.
pub fn lat_to_y(lat: f64, zoom: u8) -> f64 {
    lat_to_frac(lat) * tile_size_at_zoom(zoom)
}

/// The Web-Mercator midpoint (lat, lon) of a set of points — the center of
/// their bounding box in projected space. Returns `(0, 0)` for an empty slice.
pub fn center_of(points: &[(f64, f64)]) -> (f64, f64) {
    if points.is_empty() {
        return (0.0, 0.0);
    }
    let min_lat = points.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
    let max_lat = points.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
    let center_lat = frac_to_lat((lat_to_frac(min_lat) + lat_to_frac(max_lat)) / 2.0);

    // Circular mean for longitude, so a cluster straddling the antimeridian
    // (e.g. -179° and 179°) centers at ±180°, not 0°.
    let (mut xs, mut ys) = (0.0f64, 0.0f64);
    for &(_, lon) in points {
        let r = lon.to_radians();
        xs += r.cos();
        ys += r.sin();
    }
    let center_lon = ys.atan2(xs).to_degrees();
    (center_lat, center_lon)
}

/// Describes the requested map viewport: center, zoom, and pixel size.
/// Provides projection helpers from lat/lon to image-space pixels, and
/// computes which XYZ tiles are needed to cover the viewport.
pub struct Viewport {
    pub zoom: u8,
    pub width: u32,
    pub height: u32,
    pub top_left_x: f64,
    pub top_left_y: f64,
}

impl Viewport {
    pub fn new(center_lat: f64, center_lon: f64, zoom: u8, width: u32, height: u32) -> Self {
        let cx = lon_to_x(center_lon, zoom);
        let cy = lat_to_y(center_lat, zoom);
        Viewport {
            zoom,
            width,
            height,
            top_left_x: cx - width as f64 / 2.0,
            top_left_y: cy - height as f64 / 2.0,
        }
    }

    /// Computes a viewport whose zoom and center are chosen so that every
    /// point in `points` is visible within `width`x`height`, with a small
    /// pixel margin so markers/strokes near the edge aren't clipped.
    /// Mirrors the Google Static Maps `visible` parameter's auto-fit
    /// behavior. Falls back to zoom 1 if `points` is empty.
    pub fn fit(points: &[(f64, f64)], width: u32, height: u32) -> Self {
        if points.is_empty() {
            return Viewport::new(0.0, 0.0, 1, width, height);
        }
        // Center on the points (circular-mean longitude), then reuse the
        // antimeridian-safe zoom fit. This keeps a single span calculation and
        // avoids the raw max-min longitude span, which is wrong across ±180°.
        let (center_lat, center_lon) = center_of(points);
        Viewport::fit_at_center(center_lat, center_lon, points, width, height)
    }

    /// Keeps a fixed `center` and picks the largest zoom at which every point
    /// still fits (with a small margin). Used when the caller supplies `center`
    /// but not `zoom`: unlike [`Viewport::fit`], the center is respected.
    pub fn fit_at_center(
        center_lat: f64,
        center_lon: f64,
        points: &[(f64, f64)],
        width: u32,
        height: u32,
    ) -> Self {
        const MAX_ZOOM: u8 = 17;
        const PADDING_PX: f64 = 32.0;

        if points.is_empty() {
            return Viewport::new(center_lat, center_lon, 1, width, height);
        }

        let cfx = lon_to_frac(center_lon);
        let cfy = lat_to_frac(center_lat);
        // Half-span = farthest point from the center in each axis, so the span
        // that must fit is symmetric about the fixed center.
        let mut hx = 0.0f64;
        let mut hy = 0.0f64;
        for &(lat, lon) in points {
            // Use the shorter distance around the world so points just across
            // the antimeridian from a near-±180° center don't look ~1 world away.
            let raw = (lon_to_frac(lon) - cfx).abs();
            hx = hx.max(raw.min(1.0 - raw));
            hy = hy.max((lat_to_frac(lat) - cfy).abs());
        }

        let mut zoom = MAX_ZOOM;
        while zoom > 0 {
            let scale = tile_size_at_zoom(zoom);
            let w = 2.0 * hx * scale + PADDING_PX * 2.0;
            let h = 2.0 * hy * scale + PADDING_PX * 2.0;
            if w <= width as f64 && h <= height as f64 {
                break;
            }
            zoom -= 1;
        }

        Viewport::new(center_lat, center_lon, zoom, width, height)
    }

    /// Project a lat/lon into image-space pixel coordinates (origin top-left).
    pub fn project(&self, lat: f64, lon: f64) -> (f64, f64) {
        let x = lon_to_x(lon, self.zoom) - self.top_left_x;
        let y = lat_to_y(lat, self.zoom) - self.top_left_y;
        (x, y)
    }

    /// The set of (z, x, y) XYZ tile coordinates that cover the viewport.
    pub fn covering_tiles(&self) -> Vec<(u8, u32, u32)> {
        let tiles_per_axis: i64 = 1i64 << self.zoom;
        let x0 = (self.top_left_x / TILE_SIZE).floor() as i64;
        let x1 = ((self.top_left_x + self.width as f64) / TILE_SIZE).floor() as i64;
        let y0 = (self.top_left_y / TILE_SIZE).floor() as i64;
        let y1 = ((self.top_left_y + self.height as f64) / TILE_SIZE).floor() as i64;

        // Cap the number of columns at one full world width: when the viewport
        // is wider than the world (low zoom + large width), the raw x range
        // would otherwise wrap around and yield the same tile more than once,
        // spawning redundant fetch/decode work.
        let cols = (x1 - x0 + 1).min(tiles_per_axis);

        let mut tiles = Vec::new();
        for ty in y0..=y1 {
            if ty < 0 || ty >= tiles_per_axis {
                continue;
            }
            for i in 0..cols {
                let tx = x0 + i;
                // Wrap tile X around the antimeridian.
                let wrapped = tx.rem_euclid(tiles_per_axis);
                tiles.push((self.zoom, wrapped as u32, ty as u32));
            }
        }
        tiles
    }

    /// World-pixel origin (top-left corner) of a given XYZ tile.
    pub fn tile_origin_px(&self, x: u32, y: u32) -> (f64, f64) {
        (x as f64 * TILE_SIZE, y as f64 * TILE_SIZE)
    }
}
