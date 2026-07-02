//! Per-layer styling with a fixed Google-Maps-like ("roadmap") palette. Layer
//! names are matched by substring to stay tolerant of schema differences across
//! vector-tile sources (OpenMapTiles / Protomaps "basemap" and similar).
//! Callers may override colors/weights per feature bucket via the `style` query
//! parameter (see [`crate::params::StyleOverride`]), applied on top.

use std::collections::HashMap;

use crate::params::StyleOverride;

/// The Google-Maps-like color palette. All fields are CSS color strings.
pub struct Palette {
    /// Canvas background and the full-tile `earth` polygon (kept identical so
    /// the earth layer is invisible and never traces a tile grid).
    pub background: &'static str,
    pub landuse: &'static str,
    pub water: &'static str,
    pub waterway: &'static str,
    pub building_fill: &'static str,
    pub building_stroke: &'static str,
    pub road: &'static str,
    /// Darker outline drawn under road fills, Google's road "casing".
    pub road_casing: &'static str,
    pub transit: &'static str,
    pub boundary: &'static str,
    pub other: &'static str,
}

// Sampled from Google Maps' "roadmap" output so the defaults match closely.
pub const PALETTE: Palette = Palette {
    background: "#f5f4f4",
    landuse: "#c3f1d5",
    water: "#90daee",
    waterway: "#7fc4de",
    building_fill: "#eaeced",
    building_stroke: "#d8e0e6",
    road: "#ffffff",
    road_casing: "#d5dde3",
    transit: "#c5c9d0",
    boundary: "#c0c4cc",
    other: "#eef0f2",
};

pub struct LayerStyle {
    pub fill: Option<String>,
    pub stroke: Option<String>,
    pub stroke_width: f32,
    pub dash: Option<&'static str>,
    /// Optional casing color drawn as a wider stroke one z-level *below* this
    /// layer (used for roads, to give Google's outlined look).
    pub casing: Option<String>,
    /// Painter's-algorithm draw order, lowest first. Rendering is layer-major
    /// across *all* tiles (every tile's `earth` before any tile's `landuse`,
    /// and so on); drawing tile-by-tile instead would let each tile's
    /// full-tile background repaint over the previous tile's foreground along
    /// the shared edge, producing a visible seam.
    pub z: u8,
}

/// Number of distinct z-order buckets; sized to hold every `z` used below.
pub const Z_LEVELS: usize = 10;

pub fn style_for_layer(name: &str, overrides: &HashMap<String, StyleOverride>) -> LayerStyle {
    let p = &PALETTE;
    let n = name.to_lowercase();

    // Order matters: more specific names are matched before generic ones. Every
    // known geometry-bearing layer gets a bucket; anything unmatched lands in
    // `other`. Point-only layers (e.g. `pois`, `physical_point`) have no path
    // representation and so are intentionally not drawn.
    let (bucket, mut style) = if n.contains("earth") || n.contains("background") {
        // A full-tile background polygon; painted the same color as the canvas
        // with no border so it never shows a tile grid.
        (
            "earth",
            LayerStyle {
                fill: Some(p.background.to_string()),
                stroke: None,
                stroke_width: 0.0,
                dash: None,
                casing: None,
                z: 0,
            },
        )
    } else if n.contains("landuse")
        || n.contains("landcover")
        || n.contains("park")
        || n.contains("natural")
    {
        (
            "landuse",
            LayerStyle {
                fill: Some(p.landuse.to_string()),
                stroke: None,
                stroke_width: 0.0,
                dash: None,
                casing: None,
                z: 1,
            },
        )
    } else if n.contains("waterway")
        || n.contains("river")
        || n.contains("stream")
        || n.contains("physical_line")
    {
        // Linear water/physical features (rivers, streams). Stroked, not filled.
        (
            "waterway",
            LayerStyle {
                fill: None,
                stroke: Some(p.waterway.to_string()),
                stroke_width: 1.0,
                dash: None,
                casing: None,
                z: 3,
            },
        )
    } else if n.contains("water") || n.contains("ocean") || n.contains("lake") {
        (
            "water",
            LayerStyle {
                fill: Some(p.water.to_string()),
                stroke: None,
                stroke_width: 0.0,
                dash: None,
                casing: None,
                z: 2,
            },
        )
    } else if n.contains("building") {
        (
            "building",
            LayerStyle {
                fill: Some(p.building_fill.to_string()),
                stroke: Some(p.building_stroke.to_string()),
                stroke_width: 0.5,
                dash: None,
                casing: None,
                z: 4,
            },
        )
    } else if n.contains("transit") || n.contains("rail") {
        // Railways / transit lines: thin dashed casing.
        (
            "transit",
            LayerStyle {
                fill: None,
                stroke: Some(p.transit.to_string()),
                stroke_width: 1.0,
                dash: Some("5,3"),
                casing: None,
                z: 6,
            },
        )
    } else if n.contains("road") || n.contains("transportation") || n.contains("highway") {
        // White road fill over a darker casing (drawn a z-level below), the
        // outlined look Google uses.
        (
            "road",
            LayerStyle {
                fill: None,
                stroke: Some(p.road.to_string()),
                stroke_width: 1.5,
                dash: None,
                casing: Some(p.road_casing.to_string()),
                z: 8,
            },
        )
    } else if n.contains("boundary") || n.contains("admin") {
        (
            "boundary",
            LayerStyle {
                fill: None,
                stroke: Some(p.boundary.to_string()),
                stroke_width: 1.0,
                dash: Some("4,2"),
                casing: None,
                z: 9,
            },
        )
    } else {
        (
            "other",
            LayerStyle {
                fill: Some(p.other.to_string()),
                stroke: None,
                stroke_width: 0.0,
                dash: None,
                casing: None,
                z: 5,
            },
        )
    };

    if let Some(o) = overrides.get("all") {
        apply_override(&mut style, o);
    }
    if let Some(o) = overrides.get(bucket) {
        apply_override(&mut style, o);
    }

    style
}

fn apply_override(style: &mut LayerStyle, o: &StyleOverride) {
    if let Some(c) = &o.color {
        // Recolor whichever channels the bucket actually uses. Buckets like
        // `building` set both a fill and a distinct outline stroke; applying the
        // override to both honors the user's intent instead of silently
        // recoloring only the fill.
        if style.fill.is_some() {
            style.fill = Some(c.clone());
        }
        if style.stroke.is_some() {
            style.stroke = Some(c.clone());
        }
    }
    if let Some(w) = o.weight {
        style.stroke_width = w;
    }
}
