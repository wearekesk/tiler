//! Builds an SVG document from decoded MVT tile geometries, marker pins, and
//! optional route polylines, and rasterizes it to PNG. Route polylines are
//! stroked using lyon's tessellator and emitted as a set of filled triangles;
//! this keeps the rendering path simple at the cost of faint anti-aliasing
//! seams between adjacent triangles, which is an acceptable trade-off for this
//! endpoint.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use geo_types::{Geometry, LineString, Polygon};
use lyon::math::point;
use lyon::path::Path as LyonPath;
use lyon::tessellation::{
    BuffersBuilder, StrokeOptions, StrokeTessellator, StrokeVertex, VertexBuffers,
};
use mvt_reader::feature::Value;
use resvg::usvg::fontdb::Database;

use crate::geo_util::Viewport;
use crate::params::{MarkerGroup, PathSpec, StyleOverride};
use crate::style::{style_for_layer, PALETTE, Z_LEVELS};
use crate::tiles::DecodedLayer;

/// A single fetched-and-decoded XYZ tile, paired with its coordinate.
pub type DecodedTile = ((u8, u32, u32), Vec<DecodedLayer>);

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[allow(clippy::too_many_arguments)]
pub fn render_svg(
    viewport: &Viewport,
    tiles: &[DecodedTile],
    overrides: &HashMap<String, StyleOverride>,
    marker_groups: &[MarkerGroup],
    paths: &[PathSpec],
    out_width: u32,
    out_height: u32,
) -> String {
    let mut labels = String::new();
    // Place points are duplicated into every tile whose buffer covers them, so
    // the same label would otherwise be collected once per tile (a few pixels
    // apart, appearing as doubled/bold text). Deduplicate by name *and* a coarse
    // output-space grid cell, so cross-tile copies collapse without suppressing
    // genuinely distinct places that happen to share a name.
    let mut seen_labels: HashSet<(String, i64, i64)> = HashSet::new();
    // Candidate labels are collected first, then placed after all tiles are
    // processed so we can declutter (drop overlapping ones) by priority.
    let mut label_candidates: Vec<LabelCandidate> = Vec::new();

    // Render layer-major (painter's algorithm) across *all* tiles: everything
    // is drawn into a per-z-level bucket, then the buckets are concatenated
    // bottom-to-top. This is essential for seamless tiling - drawing
    // tile-by-tile instead lets each tile's full-tile `earth`/background paint
    // over the previous tile's foreground along the shared edge, leaving a
    // visible seam. See `LayerStyle::z`.
    let mut levels: [String; Z_LEVELS] = Default::default();

    for ((_z, x, y), layers) in tiles {
        let (origin_x, origin_y) = viewport.tile_origin_px(*x, *y);
        let offset_x = origin_x - viewport.top_left_x;
        let offset_y = origin_y - viewport.top_left_y;

        for layer in layers {
            let style = style_for_layer(&layer.name, overrides);
            let bucket = &mut levels[style.z as usize];

            let scale = 256.0 / layer.extent as f64;
            // Layers that carry named features worth labeling: settlements
            // (`places`), and water/physical features (oceans, seas, lakes,
            // rivers). POIs are intentionally excluded to avoid clutter.
            let labelable = {
                let n = layer.name.to_lowercase();
                n.contains("place") || n.contains("water") || n.contains("physical")
            };

            // Colors can originate from user-supplied `style` overrides, so
            // escape them (once per layer) before they reach the SVG to avoid
            // XML injection. Path data is numeric and needs no escaping.
            let fill = style.fill.as_deref().map(xml_escape);
            let stroke = style.stroke.as_deref().map(xml_escape);

            for feature in &layer.features {
                // Collect a label for named features (points, or lines labeled
                // at their midpoint). This is in addition to drawing the
                // geometry — a river is both stroked and labeled.
                if labelable {
                    if let Some(c) =
                        label_for_feature(feature, viewport.zoom, scale, offset_x, offset_y)
                    {
                        // ~128px grid: near-identical positions (the same place
                        // seen from adjacent tiles) collapse; far-apart places
                        // with the same name do not.
                        let cell = ((c.x / 128.0).floor() as i64, (c.y / 128.0).floor() as i64);
                        if seen_labels.insert((c.name.clone(), cell.0, cell.1)) {
                            label_candidates.push(c);
                        }
                    }
                }

                if let Some(path_d) = geometry_to_path(&feature.geometry, scale, offset_x, offset_y)
                {
                    let dash = style
                        .dash
                        .map(|d| format!(" stroke-dasharray=\"{d}\""))
                        .unwrap_or_default();

                    if let Some(fill) = fill.as_deref() {
                        // Seal each fill with a stroke of its own color so
                        // independently anti-aliased edges of adjacent same-color
                        // fills overlap rather than leaving a faint hairline. The
                        // distinct visible border (e.g. buildings) is drawn as a
                        // second pass on top so the seal doesn't fatten it.
                        let seal_width = 1.0f32.max(style.stroke_width);
                        bucket.push_str(&format!(
                            "<path d=\"{path_d}\" fill=\"{fill}\" stroke=\"{fill}\" stroke-width=\"{seal_width}\" />\n",
                        ));
                        if let Some(border) = stroke.as_deref() {
                            bucket.push_str(&format!(
                                "<path d=\"{path_d}\" fill=\"none\" stroke=\"{border}\" stroke-width=\"{}\"{dash} />\n",
                                style.stroke_width
                            ));
                        }
                    } else if let Some(stroke) = stroke.as_deref() {
                        bucket.push_str(&format!(
                            "<path d=\"{path_d}\" fill=\"none\" stroke=\"{stroke}\" stroke-width=\"{}\"{dash} />\n",
                            style.stroke_width
                        ));
                    }
                }
            }
        }
    }

    // Declutter labels: place the most important first (lowest priority value
    // = highest rank), and skip any whose box overlaps an already-placed label.
    // This is what keeps low zooms readable instead of a wall of overlapping
    // town names.
    label_candidates.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.name.cmp(&b.name)));
    let mut placed: Vec<[f64; 4]> = Vec::new();
    for c in &label_candidates {
        // Rough text box: ~6.6px per char at font-size 12, plus vertical slack.
        let half_w = (c.name.chars().count() as f64 * 3.3).max(6.0) + 2.0;
        let half_h = 8.0;
        let bx = [c.x - half_w, c.y - half_h, c.x + half_w, c.y + half_h];
        let clashes = placed
            .iter()
            .any(|p| bx[0] < p[2] && bx[2] > p[0] && bx[1] < p[3] && bx[3] > p[1]);
        if clashes {
            continue;
        }
        placed.push(bx);
        labels.push_str(&c.svg);
    }

    let mut body = String::new();
    for level in &levels {
        body.push_str(level);
    }

    for p in paths {
        if p.points.len() >= 2 {
            body.push_str(&render_path(viewport, p));
        }
    }

    for group in marker_groups {
        body.push_str(&render_marker_group(viewport, group));
    }

    body.push_str(&labels);

    // Attribution in the bottom-right corner, with a white halo for legibility
    // over any background. `&#169;` / `&#220;` are the XML numeric entities for
    // the copyright sign and `Ü` (`&copy;` is HTML-only, invalid in SVG/XML).
    let w = viewport.width;
    let h = viewport.height;
    let notice = format!("&#169; Kesk Systems O&#220; {}", current_year());
    let attribution = format!(
        "<text x=\"{x:.1}\" y=\"{y:.1}\" font-size=\"10\" font-family=\"sans-serif\" text-anchor=\"end\" fill=\"none\" stroke=\"#ffffff\" stroke-width=\"2.5\" paint-order=\"stroke\">{notice}</text>\n\
         <text x=\"{x:.1}\" y=\"{y:.1}\" font-size=\"10\" font-family=\"sans-serif\" text-anchor=\"end\" fill=\"#5f5f5f\">{notice}</text>\n",
        x = w as f64 - 5.0,
        y = h as f64 - 5.0,
    );
    body.push_str(&attribution);

    let background = PALETTE.background;
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{out_width}\" height=\"{out_height}\" viewBox=\"0 0 {w} {h}\">\n<rect width=\"{w}\" height=\"{h}\" fill=\"{background}\" />\n{body}</svg>",
    )
}

/// The current UTC year (Gregorian), for the attribution notice.
fn current_year() -> i32 {
    use chrono::Datelike;
    chrono::Utc::now().year()
}

fn tp(x: f32, y: f32, scale: f64, ox: f64, oy: f64) -> (f64, f64) {
    (x as f64 * scale + ox, y as f64 * scale + oy)
}

fn geometry_to_path(geom: &Geometry<f32>, scale: f64, ox: f64, oy: f64) -> Option<String> {
    let mut d = String::new();
    match geom {
        Geometry::LineString(ls) => append_line(&mut d, ls, scale, ox, oy),
        Geometry::Polygon(poly) => append_polygon(&mut d, poly, scale, ox, oy),
        Geometry::MultiPolygon(mp) => {
            for poly in &mp.0 {
                append_polygon(&mut d, poly, scale, ox, oy);
            }
        }
        Geometry::MultiLineString(mls) => {
            for ls in &mls.0 {
                append_line(&mut d, ls, scale, ox, oy);
            }
        }
        _ => return None,
    }
    if d.is_empty() {
        None
    } else {
        Some(d)
    }
}

fn append_line(d: &mut String, ls: &LineString<f32>, scale: f64, ox: f64, oy: f64) {
    for (i, c) in ls.coords().enumerate() {
        let (x, y) = tp(c.x, c.y, scale, ox, oy);
        if i == 0 {
            d.push_str(&format!("M {x:.2} {y:.2} "));
        } else {
            d.push_str(&format!("L {x:.2} {y:.2} "));
        }
    }
}

fn append_polygon(d: &mut String, poly: &Polygon<f32>, scale: f64, ox: f64, oy: f64) {
    append_ring(d, poly.exterior(), scale, ox, oy);
    for interior in poly.interiors() {
        append_ring(d, interior, scale, ox, oy);
    }
}

fn append_ring(d: &mut String, ring: &LineString<f32>, scale: f64, ox: f64, oy: f64) {
    for (i, c) in ring.coords().enumerate() {
        let (x, y) = tp(c.x, c.y, scale, ox, oy);
        if i == 0 {
            d.push_str(&format!("M {x:.2} {y:.2} "));
        } else {
            d.push_str(&format!("L {x:.2} {y:.2} "));
        }
    }
    d.push_str("Z ");
}

/// A placed-or-not label with the data needed to declutter it.
struct LabelCandidate {
    name: String,
    x: f64,
    y: f64,
    /// Lower is more important (from the tile's `sort_key`, else `-population`).
    priority: i64,
    svg: String,
}

/// Interprets a numeric MVT property value as `f64`, if it is numeric.
fn value_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f as f64),
        Value::Double(d) => Some(*d),
        Value::Int(i) | Value::SInt(i) => Some(*i as f64),
        Value::UInt(u) => Some(*u as f64),
        _ => None,
    }
}

/// Renders a point-layer feature's `name` property as a halo'd text label,
/// if present. Returns the place name and its output-space position alongside
/// the SVG so the caller can deduplicate labels that appear in more than one
/// tile without collapsing distinct places that share a name.
///
/// Labels are filtered by the feature's `min_zoom` hint (present in Protomaps/
/// OpenMapTiles `places`): a place is only labeled once the map has zoomed in
/// far enough for it, so low zooms show only major places instead of every
/// hamlet the (overzoomed) tile happens to contain. Points are labeled at their
/// position and lines at their midpoint vertex (e.g. rivers); polygon labels are
/// not placed.
fn label_for_feature(
    feature: &mvt_reader::feature::Feature<f32>,
    zoom: u8,
    scale: f64,
    ox: f64,
    oy: f64,
) -> Option<LabelCandidate> {
    let props = feature.properties.as_ref()?;

    // Skip places whose minimum display zoom hasn't been reached yet.
    if let Some(min_zoom) = props
        .get("min_zoom")
        .or_else(|| props.get("pmap:min_zoom"))
        .and_then(value_as_f64)
    {
        if (zoom as f64) < min_zoom {
            return None;
        }
    }

    let name = props
        .get("name")
        .or_else(|| props.get("name:en"))
        .and_then(|v| match v {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })?;

    // Anchor: a point's position, or the midpoint vertex of a line (so rivers
    // etc. get a label roughly along their course).
    let midpoint = |ls: &LineString<f32>| -> Option<(f64, f64)> {
        let coords: Vec<_> = ls.coords().collect();
        coords
            .get(coords.len() / 2)
            .map(|c| tp(c.x, c.y, scale, ox, oy))
    };
    let (x, y) = match &feature.geometry {
        Geometry::Point(p) => tp(p.x(), p.y(), scale, ox, oy),
        Geometry::MultiPoint(mp) => {
            let p = mp.0.first()?;
            tp(p.x(), p.y(), scale, ox, oy)
        }
        Geometry::LineString(ls) => midpoint(ls)?,
        Geometry::MultiLineString(mls) => midpoint(mls.0.first()?)?,
        _ => return None,
    };

    // Ranking for decluttering: the tile's `sort_key` (lower = more important)
    // if present, else higher population wins (negated so lower = better).
    let priority = props
        .get("sort_key")
        .and_then(value_as_f64)
        .or_else(|| props.get("population").and_then(value_as_f64).map(|p| -p))
        .map(|v| v as i64)
        .unwrap_or(0);

    let escaped = xml_escape(&name);
    let svg = format!(
        "<text x=\"{x:.1}\" y=\"{y:.1}\" font-size=\"12\" font-family=\"sans-serif\" font-weight=\"600\" text-anchor=\"middle\" fill=\"none\" stroke=\"#ffffff\" stroke-width=\"3\" paint-order=\"stroke\">{escaped}</text>\n\
         <text x=\"{x:.1}\" y=\"{y:.1}\" font-size=\"12\" font-family=\"sans-serif\" font-weight=\"600\" text-anchor=\"middle\" fill=\"#333333\">{escaped}</text>\n"
    );
    Some(LabelCandidate {
        name,
        x,
        y,
        priority,
        svg,
    })
}

fn render_marker_group(viewport: &Viewport, group: &MarkerGroup) -> String {
    let mut out = String::new();
    let r = group.size.radius();
    // Color and label come from user query parameters; escape before emitting.
    let color = xml_escape(&group.color);
    let label = group.label.map(|c| xml_escape(&c.to_string()));
    for (lat, lon) in &group.points {
        let (mx, my) = viewport.project(*lat, *lon);
        out.push_str(&format!(
            "<circle cx=\"{mx:.2}\" cy=\"{my:.2}\" r=\"{r}\" fill=\"{color}\" stroke=\"#ffffff\" stroke-width=\"2\" />\n",
        ));
        if let Some(label) = &label {
            out.push_str(&format!(
                "<text x=\"{mx:.2}\" y=\"{my:.2}\" font-size=\"{:.1}\" font-family=\"sans-serif\" font-weight=\"700\" text-anchor=\"middle\" dominant-baseline=\"central\" fill=\"#ffffff\">{label}</text>\n",
                r * 1.1
            ));
        }
    }
    out
}

fn render_path(viewport: &Viewport, spec: &PathSpec) -> String {
    let mut out = String::new();
    // Colors come from user query parameters; escape before emitting to SVG.
    let color = xml_escape(&spec.color);

    // Optional polygon fill: render the raw point list as a closed ring
    // first (beneath the stroked line), akin to Google's `fillcolor`.
    if let Some(fillcolor) = &spec.fillcolor {
        let fillcolor = xml_escape(fillcolor);
        let mut d = String::new();
        for (i, (lat, lon)) in spec.points.iter().enumerate() {
            let (x, y) = viewport.project(*lat, *lon);
            if i == 0 {
                d.push_str(&format!("M {x:.2} {y:.2} "));
            } else {
                d.push_str(&format!("L {x:.2} {y:.2} "));
            }
        }
        d.push('Z');
        out.push_str(&format!(
            "<path d=\"{d}\" fill=\"{fillcolor}\" stroke=\"none\" />\n"
        ));
    }

    let mut builder = LyonPath::builder();
    let mut points = spec.points.iter().map(|(lat, lon)| {
        let (x, y) = viewport.project(*lat, *lon);
        point(x as f32, y as f32)
    });

    if let Some(first) = points.next() {
        builder.begin(first);
        for p in points {
            builder.line_to(p);
        }
        builder.end(false);
    }
    let path = builder.build();

    // u32 indices: a long/thick route can tessellate to more than u16::MAX
    // (65535) vertices, which would overflow the index and corrupt the mesh.
    let mut geometry: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let mut tessellator = StrokeTessellator::new();
    {
        let mut buffers_builder = BuffersBuilder::new(&mut geometry, |vertex: StrokeVertex| {
            let p = vertex.position();
            [p.x, p.y]
        });
        let _ = tessellator.tessellate_path(
            &path,
            &StrokeOptions::default().with_line_width(spec.weight),
            &mut buffers_builder,
        );
    }

    for tri in geometry.indices.chunks(3) {
        if tri.len() < 3 {
            continue;
        }
        let a = geometry.vertices[tri[0] as usize];
        let b = geometry.vertices[tri[1] as usize];
        let c = geometry.vertices[tri[2] as usize];
        out.push_str(&format!(
            "<polygon points=\"{:.2},{:.2} {:.2},{:.2} {:.2},{:.2}\" fill=\"{color}\" />\n",
            a[0], a[1], b[0], b[1], c[0], c[1]
        ));
    }
    out
}

/// Loads system fonts once per process and caches the resulting database,
/// since scanning installed fonts on every request would be wasteful.
fn shared_fontdb() -> Arc<Database> {
    static DB: OnceLock<Arc<Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = Database::new();
        db.load_system_fonts();
        Arc::new(db)
    })
    .clone()
}

/// Eagerly loads the system font database. Call once at startup so the
/// (synchronous, disk-scanning) load doesn't spike latency on the first render.
pub fn warm_fontdb() {
    let _ = shared_fontdb();
}

/// Rasterizes an SVG document to PNG bytes using resvg/usvg/tiny-skia.
pub fn svg_to_png(svg: &str, width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
    let opt = resvg::usvg::Options {
        fontdb: shared_fontdb(),
        font_family: "Arial".to_string(),
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_str(svg, &opt)?;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| anyhow::anyhow!("failed to allocate {width}x{height} pixmap"))?;

    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );

    pixmap
        .encode_png()
        .map_err(|e| anyhow::anyhow!("failed to encode png: {e}"))
}
