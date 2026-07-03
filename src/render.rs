//! Builds an SVG document from decoded MVT tile geometries, marker pins, and
//! optional route polylines, and rasterizes it to PNG. Route polylines are
//! emitted as native stroked `<path>` elements.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use geo_types::{Geometry, LineString, Polygon};
use mvt_reader::feature::Value;
use resvg::usvg::fontdb::Database;

use crate::geo_util::Viewport;
use crate::params::{MarkerGroup, PathSpec, StyleOverride};
use crate::style::{style_for_layer, PALETTE, Z_LEVELS};
use crate::tiles::DecodedLayer;

/// A single fetched-and-decoded XYZ tile, paired with its coordinate.
pub type DecodedTile = ((u8, u32, u32), Vec<DecodedLayer>);

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
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
    // Roads are styled per-feature by class and collected across all tiles, then
    // emitted sorted by rank so major roads draw over minor ones (and all
    // casings under all fills). (rank, casing_svg, fill_svg).
    let mut road_segments: Vec<(i32, Option<String>, String)> = Vec::new();

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
            let zi = style.z as usize;
            // Casing draws one level below the stroke so all casings sit under
            // all road fills (keeps intersections connected).
            let casing_zi = zi.saturating_sub(1);

            let scale = 256.0 / layer.extent as f64;
            // Layers that carry named features worth labeling: settlements
            // (`places`), and water/physical features (oceans, seas, lakes,
            // rivers). POIs are intentionally excluded to avoid clutter.
            let labelable = {
                let n = layer.name.to_lowercase();
                n.contains("place") || n.contains("water") || n.contains("physical")
            };
            let is_road_layer = {
                let n = layer.name.to_lowercase();
                n.contains("road") || n.contains("transportation") || n.contains("highway")
            };

            // Colors can originate from user-supplied `style` overrides, so
            // escape them (once per layer) before they reach the SVG to avoid
            // XML injection. Path data is numeric and needs no escaping.
            let fill = style.fill.as_deref().map(xml_escape);
            let stroke = style.stroke.as_deref().map(xml_escape);
            let casing = style.casing.as_deref().map(xml_escape);
            // Per-feature road styling bypasses the generic bucket, so resolve
            // any user `style` override for roads (feature:road or feature:all)
            // here and apply it to each road segment below.
            let road_override = overrides.get("road").or_else(|| overrides.get("all"));

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

                // Roads are drawn per-feature by class (width/color/casing),
                // collected for later rank-sorted emission — not via the generic
                // path below.
                if is_road_layer {
                    if let Some(path_d) =
                        geometry_to_path(&feature.geometry, scale, offset_x, offset_y)
                    {
                        let kind = feature
                            .properties
                            .as_ref()
                            .and_then(|p| match p.get("kind") {
                                Some(Value::String(s)) => Some(s.as_str()),
                                _ => None,
                            });
                        let detail =
                            feature
                                .properties
                                .as_ref()
                                .and_then(|p| match p.get("kind_detail") {
                                    Some(Value::String(s)) => Some(s.as_str()),
                                    _ => None,
                                });
                        let rs = road_style(kind, detail);
                        // Apply a user `style` override, if any. The override
                        // color is user input, so it must be escaped; the
                        // built-in class colors are constants and safe.
                        let road_color = match road_override.and_then(|o| o.color.as_deref()) {
                            Some(c) => xml_escape(c),
                            None => rs.color.to_string(),
                        };
                        let road_width = road_override.and_then(|o| o.weight).unwrap_or(rs.width);
                        let casing_svg = rs.casing.map(|cc| {
                            format!(
                                "<path d=\"{path_d}\" fill=\"none\" stroke=\"{cc}\" stroke-width=\"{:.1}\" stroke-linecap=\"round\" stroke-linejoin=\"round\" />\n",
                                road_width + rs.casing_extra
                            )
                        });
                        let dash = rs
                            .dash
                            .map(|d| format!(" stroke-dasharray=\"{d}\""))
                            .unwrap_or_default();
                        let fill_svg = format!(
                            "<path d=\"{path_d}\" fill=\"none\" stroke=\"{road_color}\" stroke-width=\"{:.1}\"{dash} stroke-linecap=\"round\" stroke-linejoin=\"round\" />\n",
                            road_width
                        );
                        road_segments.push((rs.rank, casing_svg, fill_svg));
                    }
                    continue;
                }

                if let Some(path_d) = geometry_to_path(&feature.geometry, scale, offset_x, offset_y)
                {
                    let dash = style
                        .dash
                        .map(|d| format!(" stroke-dasharray=\"{d}\""))
                        .unwrap_or_default();

                    if let Some(fill) = fill.as_deref() {
                        if let Some(border) = stroke.as_deref() {
                            // The border already covers the fill's edge, so no
                            // seal stroke here — a fill-colored seal would stick
                            // out past a thin border (e.g. buildings) as a fuzzy
                            // halo.
                            levels[zi].push_str(&format!(
                                "<path d=\"{path_d}\" fill=\"{fill}\" stroke=\"none\" />\n",
                            ));
                            levels[zi].push_str(&format!(
                                "<path d=\"{path_d}\" fill=\"none\" stroke=\"{border}\" stroke-width=\"{}\"{dash} />\n",
                                style.stroke_width
                            ));
                        } else {
                            // Borderless fills (earth/landuse/water) that butt
                            // against same-color neighbors across tile edges: seal
                            // with a 1px stroke of the fill color so independently
                            // anti-aliased edges overlap instead of leaving a
                            // hairline seam.
                            levels[zi].push_str(&format!(
                                "<path d=\"{path_d}\" fill=\"{fill}\" stroke=\"{fill}\" stroke-width=\"1\" />\n",
                            ));
                        }
                    } else if let Some(stroke) = stroke.as_deref() {
                        // Optional casing drawn wider, one level below, so it
                        // reads as an outline around the (later) road fill.
                        if let Some(casing) = casing.as_deref() {
                            levels[casing_zi].push_str(&format!(
                                "<path d=\"{path_d}\" fill=\"none\" stroke=\"{casing}\" stroke-width=\"{}\" stroke-linecap=\"round\" stroke-linejoin=\"round\" />\n",
                                style.stroke_width + 1.6
                            ));
                        }
                        levels[zi].push_str(&format!(
                            "<path d=\"{path_d}\" fill=\"none\" stroke=\"{stroke}\" stroke-width=\"{}\"{dash} stroke-linecap=\"round\" stroke-linejoin=\"round\" />\n",
                            style.stroke_width
                        ));
                    }
                }
            }
        }
    }

    // Emit roads: all casings first, then all fills, each in ascending rank so
    // motorways draw over residential streets. Road z-levels mirror the `road`
    // bucket in `style.rs` (fill z=8, casing z=7).
    const ROAD_CASING_Z: usize = 7;
    const ROAD_FILL_Z: usize = 8;
    road_segments.sort_by_key(|s| s.0);
    for (_, casing, _) in &road_segments {
        if let Some(c) = casing {
            levels[ROAD_CASING_Z].push_str(c);
        }
    }
    for (_, _, fill) in &road_segments {
        levels[ROAD_FILL_Z].push_str(fill);
    }

    // Declutter labels: place the most important first (lowest priority value
    // = highest rank), and skip any whose box overlaps an already-placed label.
    // This is what keeps low zooms readable instead of a wall of overlapping
    // town names.
    label_candidates.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.name.cmp(&b.name)));
    let mut placed: Vec<[f64; 4]> = Vec::new();
    for c in &label_candidates {
        // Rough text box scaled to the label's font size (~0.55em per char).
        let half_w = (c.name.chars().count() as f64 * c.font_size * 0.275).max(6.0) + 2.0;
        let half_h = c.font_size * 0.65;
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
        "<text x=\"{x:.1}\" y=\"{y:.1}\" font-size=\"10\" font-family=\"Roboto, sans-serif\" text-anchor=\"end\" fill=\"none\" stroke=\"#ffffff\" stroke-width=\"2.5\" paint-order=\"stroke\">{notice}</text>\n\
         <text x=\"{x:.1}\" y=\"{y:.1}\" font-size=\"10\" font-family=\"Roboto, sans-serif\" text-anchor=\"end\" fill=\"#5f5f5f\">{notice}</text>\n",
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
        let cmd = if i == 0 { 'M' } else { 'L' };
        let _ = write!(d, "{cmd} {x:.2} {y:.2} ");
    }
}

fn append_polygon(d: &mut String, poly: &Polygon<f32>, scale: f64, ox: f64, oy: f64) {
    append_ring(d, poly.exterior(), scale, ox, oy);
    for interior in poly.interiors() {
        append_ring(d, interior, scale, ox, oy);
    }
}

fn append_ring(d: &mut String, ring: &LineString<f32>, scale: f64, ox: f64, oy: f64) {
    let mut any = false;
    for (i, c) in ring.coords().enumerate() {
        let (x, y) = tp(c.x, c.y, scale, ox, oy);
        let cmd = if i == 0 { 'M' } else { 'L' };
        let _ = write!(d, "{cmd} {x:.2} {y:.2} ");
        any = true;
    }
    // Don't emit a bare `Z` for an empty ring — that's malformed path data.
    if any {
        d.push_str("Z ");
    }
}

/// A placed-or-not label with the data needed to declutter it.
struct LabelCandidate {
    name: String,
    x: f64,
    y: f64,
    /// Lower is more important (from the tile's `sort_key`, else `-population`).
    priority: i64,
    font_size: f64,
    svg: String,
}

/// How a label is drawn, chosen by place kind to loosely follow Google Maps
/// "roadmap" conventions: uppercase, letter-spaced country/region names; a
/// weighted size hierarchy for settlements; and italic blue-grey water labels.
struct LabelStyle {
    size: f64,
    color: &'static str,
    italic: bool,
    upper: bool,
    tracking: f64,
    weight: u32,
}

fn label_style(kind: Option<&str>, kind_detail: Option<&str>) -> LabelStyle {
    let base = LabelStyle {
        size: 11.0,
        color: "#3c4043",
        italic: false,
        upper: false,
        tracking: 0.0,
        weight: 400,
    };
    match kind {
        Some("country") => LabelStyle {
            size: 13.0,
            upper: true,
            tracking: 1.5,
            weight: 500,
            ..base
        },
        Some("region") | Some("state") | Some("province") => LabelStyle {
            size: 11.0,
            color: "#5f6368",
            upper: true,
            tracking: 1.0,
            weight: 500,
            ..base
        },
        Some("ocean") | Some("sea") => LabelStyle {
            size: 14.0,
            color: "#48688f",
            italic: true,
            ..base
        },
        Some("lake") | Some("river") | Some("water") | Some("stream") | Some("canal") => {
            LabelStyle {
                color: "#48688f",
                italic: true,
                ..base
            }
        }
        Some("locality") => match kind_detail {
            Some("city") => LabelStyle {
                size: 14.0,
                weight: 500,
                ..base
            },
            Some("town") => LabelStyle { size: 12.0, ..base },
            Some("village") | Some("hamlet") | Some("suburb") => LabelStyle {
                size: 10.0,
                color: "#5f6368",
                ..base
            },
            _ => LabelStyle { size: 12.0, ..base },
        },
        _ => base,
    }
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
        let n = ls.coords().count();
        ls.coords().nth(n / 2).map(|c| tp(c.x, c.y, scale, ox, oy))
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

    let str_prop = |k: &str| match props.get(k) {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    let ls = label_style(str_prop("kind"), str_prop("kind_detail"));
    let font_size = ls.size;
    let halo = font_size / 4.0;

    let display = if ls.upper {
        name.to_uppercase()
    } else {
        name.clone()
    };
    let escaped = xml_escape(&display);
    let style_attrs = format!(
        "font-size=\"{size:.1}\" font-family=\"Roboto, sans-serif\" font-weight=\"{weight}\"{italic}{tracking} text-anchor=\"middle\"",
        size = font_size,
        weight = ls.weight,
        italic = if ls.italic { " font-style=\"italic\"" } else { "" },
        tracking = if ls.tracking > 0.0 {
            format!(" letter-spacing=\"{:.1}\"", ls.tracking)
        } else {
            String::new()
        },
    );
    let svg = format!(
        "<text x=\"{x:.1}\" y=\"{y:.1}\" {style_attrs} fill=\"none\" stroke=\"#ffffff\" stroke-width=\"{halo:.1}\" paint-order=\"stroke\">{escaped}</text>\n\
         <text x=\"{x:.1}\" y=\"{y:.1}\" {style_attrs} fill=\"{color}\">{escaped}</text>\n",
        color = ls.color,
    );
    Some(LabelCandidate {
        name: display,
        x,
        y,
        priority,
        font_size,
        svg,
    })
}

/// Per-class road styling. Colors are constant (from the data, not user input),
/// so they need no escaping.
struct RoadStyle {
    color: &'static str,
    width: f32,
    /// Casing (outline) color, drawn `casing_extra` px wider beneath the fill.
    casing: Option<&'static str>,
    casing_extra: f32,
    dash: Option<&'static str>,
    /// Draw order among roads; higher renders on top.
    rank: i32,
}

/// Maps a road feature's `kind`/`kind_detail` to a Google-like style: orange
/// motorways, white arterials that get thinner down the hierarchy, thin service
/// roads, faint dashed paths, and dashed rail/ferry lines.
fn road_style(kind: Option<&str>, detail: Option<&str>) -> RoadStyle {
    const WHITE: &str = "#ffffff";
    const CASING: &str = "#d5dde3";
    const MOTORWAY: &str = "#f6b25a";
    const MOTORWAY_CASING: &str = "#e0912f";
    match (kind, detail) {
        (Some("highway"), Some("motorway")) => RoadStyle {
            color: MOTORWAY,
            width: 3.4,
            casing: Some(MOTORWAY_CASING),
            casing_extra: 1.6,
            dash: None,
            rank: 100,
        },
        (Some("highway"), _) => RoadStyle {
            color: MOTORWAY,
            width: 2.4,
            casing: Some(MOTORWAY_CASING),
            casing_extra: 1.4,
            dash: None,
            rank: 92,
        },
        (Some("major_road"), Some("trunk")) => RoadStyle {
            color: WHITE,
            width: 2.8,
            casing: Some(CASING),
            casing_extra: 1.6,
            dash: None,
            rank: 80,
        },
        (Some("major_road"), Some("primary") | Some("primary_link")) => RoadStyle {
            color: WHITE,
            width: 2.6,
            casing: Some(CASING),
            casing_extra: 1.5,
            dash: None,
            rank: 72,
        },
        (Some("major_road"), Some("secondary") | Some("secondary_link")) => RoadStyle {
            color: WHITE,
            width: 2.1,
            casing: Some(CASING),
            casing_extra: 1.4,
            dash: None,
            rank: 64,
        },
        (Some("major_road"), _) => RoadStyle {
            color: WHITE,
            width: 1.8,
            casing: Some(CASING),
            casing_extra: 1.3,
            dash: None,
            rank: 56,
        },
        (Some("minor_road"), Some("service")) => RoadStyle {
            color: WHITE,
            width: 1.0,
            casing: Some(CASING),
            casing_extra: 1.0,
            dash: None,
            rank: 30,
        },
        (Some("minor_road"), _) => RoadStyle {
            color: WHITE,
            width: 1.4,
            casing: Some(CASING),
            casing_extra: 1.1,
            dash: None,
            rank: 36,
        },
        (Some("path"), _) => RoadStyle {
            color: "#c8ccd2",
            width: 0.9,
            casing: None,
            casing_extra: 0.0,
            dash: Some("1,2.5"),
            rank: 12,
        },
        (Some("rail"), _) => RoadStyle {
            color: "#c2c7cf",
            width: 0.9,
            casing: None,
            casing_extra: 0.0,
            dash: Some("4,3"),
            rank: 16,
        },
        (Some("ferry"), _) => RoadStyle {
            color: "#8fbfe0",
            width: 1.1,
            casing: None,
            casing_extra: 0.0,
            dash: Some("2,4"),
            rank: 14,
        },
        _ => RoadStyle {
            color: WHITE,
            width: 1.4,
            casing: Some(CASING),
            casing_extra: 1.1,
            dash: None,
            rank: 34,
        },
    }
}

fn render_marker_group(viewport: &Viewport, group: &MarkerGroup) -> String {
    let mut out = String::new();
    // Head radius of the teardrop pin; the tip sits exactly on the location.
    let hr = (group.size.radius() * 1.5) as f64;
    // Color and label come from user query parameters; escape before emitting.
    let color = xml_escape(&group.color);
    let label = group.label.map(|c| xml_escape(&c.to_string()));
    for (lat, lon) in &group.points {
        let (mx, my) = viewport.project(*lat, *lon);
        // Google-style pin: a circular head above a pointed tip. The two sides
        // run from the tip up to the tangent points of the head circle, then a
        // major arc closes the head.
        let cy = my - hr * 2.4;
        let (ltx, lty) = (mx - hr * 0.909, cy + hr * 0.417);
        let (rtx, rty) = (mx + hr * 0.909, cy + hr * 0.417);
        out.push_str(&format!(
            "<path d=\"M {mx:.2} {my:.2} L {ltx:.2} {lty:.2} A {hr:.2} {hr:.2} 0 1 1 {rtx:.2} {rty:.2} Z\" fill=\"{color}\" stroke=\"#3c4043\" stroke-opacity=\"0.25\" stroke-width=\"0.75\" />\n",
        ));
        if let Some(label) = &label {
            out.push_str(&format!(
                "<text x=\"{mx:.2}\" y=\"{cy:.2}\" font-size=\"{:.1}\" font-family=\"Roboto, sans-serif\" font-weight=\"700\" text-anchor=\"middle\" dominant-baseline=\"central\" fill=\"#ffffff\">{label}</text>\n",
                hr * 1.15
            ));
        } else {
            // Unlabeled: a small white dot in the head, like Google's default pin.
            out.push_str(&format!(
                "<circle cx=\"{mx:.2}\" cy=\"{cy:.2}\" r=\"{:.2}\" fill=\"#ffffff\" />\n",
                hr * 0.42
            ));
        }
    }
    out
}

fn render_path(viewport: &Viewport, spec: &PathSpec) -> String {
    let mut out = String::new();
    // Colors come from user query parameters; escape before emitting to SVG.
    let color = xml_escape(&spec.color);

    let mut d = String::new();
    for (i, (lat, lon)) in spec.points.iter().enumerate() {
        let (x, y) = viewport.project(*lat, *lon);
        let cmd = if i == 0 { 'M' } else { 'L' };
        let _ = write!(d, "{cmd} {x:.2} {y:.2} ");
    }
    if d.is_empty() {
        return out;
    }

    // Optional polygon fill: the point list as a closed ring, beneath the
    // stroked line (akin to Google's `fillcolor`).
    if let Some(fillcolor) = &spec.fillcolor {
        let fillcolor = xml_escape(fillcolor);
        out.push_str(&format!(
            "<path d=\"{d}Z\" fill=\"{fillcolor}\" stroke=\"none\" />\n"
        ));
    }

    // The route line, stroked natively by the SVG renderer (no tessellation).
    out.push_str(&format!(
        "<path d=\"{d}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"{:.1}\" stroke-linecap=\"round\" stroke-linejoin=\"round\" />\n",
        spec.weight
    ));
    out
}

/// The label font family. Roboto is bundled (below), so this renders
/// identically everywhere — matching Google Maps, which also uses Roboto.
const FONT_FAMILY: &str = "Roboto";

/// Loads the font database once per process and caches it. Roboto is embedded
/// in the binary so text renders consistently (including in the container);
/// system fonts are also loaded as a fallback for any missing glyphs.
fn shared_fontdb() -> Arc<Database> {
    static DB: OnceLock<Arc<Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = Database::new();
        db.load_font_data(include_bytes!("../assets/fonts/Roboto-Regular.ttf").to_vec());
        db.load_font_data(include_bytes!("../assets/fonts/Roboto-Medium.ttf").to_vec());
        db.load_font_data(include_bytes!("../assets/fonts/Roboto-Bold.ttf").to_vec());
        db.load_font_data(include_bytes!("../assets/fonts/Roboto-Italic.ttf").to_vec());
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
        font_family: FONT_FAMILY.to_string(),
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
