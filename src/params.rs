//! Parsing helpers for the `/staticmap` query string: marker groups (with
//! per-group color/label/size styling), path specs (with color/weight/
//! fillcolor styling, plus Google encoded-polyline `enc:` support), per
//! feature `style` overrides, and `visible` bounding-box points.

use std::collections::HashMap;

use poem::http::StatusCode;

pub fn bad_request(msg: impl std::fmt::Display) -> poem::Error {
    poem::Error::from_string(msg.to_string(), StatusCode::BAD_REQUEST)
}

/// A parsed query string, preserving repeated keys (e.g. multiple
/// `markers=...` params) as a multimap.
pub struct QueryMap(HashMap<String, Vec<String>>);

impl QueryMap {
    pub fn parse(query: &str) -> Self {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()).into_owned() {
            map.entry(k).or_default().push(v);
        }
        QueryMap(map)
    }

    pub fn get_one(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.first()).map(|s| s.as_str())
    }

    pub fn get_all(&self, key: &str) -> &[String] {
        self.0.get(key).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

fn parse_latlon(s: &str) -> poem::Result<(f64, f64)> {
    let mut parts = s.split(',');
    let lat = parts
        .next()
        .ok_or_else(|| bad_request("missing latitude"))?
        .trim()
        .parse::<f64>()
        .map_err(|e| bad_request(format!("invalid latitude '{s}': {e}")))?;
    let lon = parts
        .next()
        .ok_or_else(|| bad_request("missing longitude"))?
        .trim()
        .parse::<f64>()
        .map_err(|e| bad_request(format!("invalid longitude '{s}': {e}")))?;
    Ok((lat, lon))
}

/// Normalizes a Google-style `0xRRGGBB[AA]` color into a CSS/SVG-compatible
/// `#RRGGBB[AA]` string. Anything else (named colors, already-`#`-prefixed
/// hex) is passed through unchanged.
pub fn normalize_color(s: &str) -> String {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        format!("#{hex}")
    } else {
        s.to_string()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MarkerSize {
    Tiny,
    Small,
    Mid,
}

impl MarkerSize {
    pub fn radius(self) -> f32 {
        match self {
            MarkerSize::Tiny => 4.0,
            MarkerSize::Small => 6.0,
            MarkerSize::Mid => 8.0,
        }
    }
}

pub struct MarkerGroup {
    pub color: String,
    pub label: Option<char>,
    pub size: MarkerSize,
    pub points: Vec<(f64, f64)>,
}

/// Parses one `markers=...` value: a `|`-separated mix of `key:value` style
/// tokens (`color:red`, `label:A`, `size:small`) and `lat,lon` coordinate
/// pairs, e.g. `color:blue|label:S|48.85,2.35|48.86,2.36`.
pub fn parse_marker_group(raw: &str) -> poem::Result<MarkerGroup> {
    let mut color = "red".to_string();
    let mut label = None;
    let mut size = MarkerSize::Mid;
    let mut points = Vec::new();

    for token in raw.split('|') {
        if token.is_empty() {
            continue;
        }
        if let Some((key, value)) = token.split_once(':') {
            match key.to_lowercase().as_str() {
                "color" => color = normalize_color(value),
                "label" => label = value.chars().next().map(|c| c.to_ascii_uppercase()),
                "size" => {
                    size = match value.to_lowercase().as_str() {
                        "tiny" => MarkerSize::Tiny,
                        "small" => MarkerSize::Small,
                        _ => MarkerSize::Mid,
                    }
                }
                _ => {}
            }
        } else {
            points.push(parse_latlon(token)?);
        }
    }

    Ok(MarkerGroup {
        color,
        label,
        size,
        points,
    })
}

pub fn parse_marker_groups(raw: &[String]) -> poem::Result<Vec<MarkerGroup>> {
    raw.iter().map(|s| parse_marker_group(s)).collect()
}

pub struct PathSpec {
    pub color: String,
    pub weight: f32,
    pub fillcolor: Option<String>,
    pub points: Vec<(f64, f64)>,
}

/// Parses one `path=...` value: `|`-separated `key:value` style tokens
/// (`color:0xff0000`, `weight:5`, `fillcolor:0x00ff0033`), plus either
/// `lat,lon` coordinate pairs or a single `enc:<encoded polyline>` segment.
pub fn parse_path_spec(raw: &str) -> poem::Result<PathSpec> {
    let mut color = "#3388ff".to_string();
    let mut weight = 4.0f32;
    let mut fillcolor = None;
    let mut points = Vec::new();

    // The `enc:` encoded-polyline alphabet can itself contain literal `|`
    // characters, so it must be treated as running to the end of the raw
    // value rather than being split on `|` like the other tokens.
    let (style_part, encoded_part) = match raw.find("enc:") {
        Some(idx) => (&raw[..idx], Some(&raw[idx + "enc:".len()..])),
        None => (raw, None),
    };

    for token in style_part.split('|') {
        if token.is_empty() {
            continue;
        }
        if let Some((key, value)) = token.split_once(':') {
            match key.to_lowercase().as_str() {
                "color" => color = normalize_color(value),
                "weight" => {
                    weight = value
                        .parse::<f32>()
                        .map_err(|e| bad_request(format!("invalid path weight: {e}")))?
                }
                "fillcolor" => fillcolor = Some(normalize_color(value)),
                // `geodesic` is accepted but has no effect: our paths are
                // already rendered as straight segments in projected space.
                "geodesic" => {}
                _ => {}
            }
        } else {
            points.push(parse_latlon(token)?);
        }
    }

    if let Some(encoded) = encoded_part {
        points.extend(polyline::decode(encoded));
    }

    Ok(PathSpec {
        color,
        weight,
        fillcolor,
        points,
    })
}

pub fn parse_path_specs(raw: &[String]) -> poem::Result<Vec<PathSpec>> {
    raw.iter().map(|s| parse_path_spec(s)).collect()
}

#[derive(Default, Clone)]
pub struct StyleOverride {
    pub color: Option<String>,
    pub weight: Option<f32>,
}

/// Parses one `style=...` value, e.g. `feature:water|color:0x224466`.
/// Overrides are keyed by feature bucket name (`water`, `landuse`,
/// `building`, `road`, `boundary`), or `all` to apply to every bucket.
pub fn parse_style_override(raw: &str) -> poem::Result<(String, StyleOverride)> {
    let mut feature = "all".to_string();
    let mut over = StyleOverride::default();

    for token in raw.split('|') {
        if token.is_empty() {
            continue;
        }
        if let Some((key, value)) = token.split_once(':') {
            match key.to_lowercase().as_str() {
                "feature" => feature = value.to_lowercase(),
                "color" => over.color = Some(normalize_color(value)),
                "weight" => {
                    over.weight = Some(
                        value
                            .parse::<f32>()
                            .map_err(|e| bad_request(format!("invalid style weight: {e}")))?,
                    )
                }
                _ => {}
            }
        }
    }

    Ok((feature, over))
}

pub fn parse_style_overrides(raw: &[String]) -> poem::Result<HashMap<String, StyleOverride>> {
    let mut map = HashMap::new();
    for s in raw {
        let (feature, over) = parse_style_override(s)?;
        map.insert(feature, over);
    }
    Ok(map)
}

/// Parses all `visible=...` values (each itself `|`-separated `lat,lon`
/// pairs) into a single flattened list of points.
pub fn parse_visible(raw: &[String]) -> poem::Result<Vec<(f64, f64)>> {
    let mut points = Vec::new();
    for s in raw {
        for part in s.split('|') {
            if !part.is_empty() {
                points.push(parse_latlon(part)?);
            }
        }
    }
    Ok(points)
}

/// Decoder for Google's "encoded polyline" algorithm format, as used by the
/// `path=enc:...` parameter.
/// <https://developers.google.com/maps/documentation/utilities/polylinealgorithm>
mod polyline {
    /// Decodes an encoded polyline string into `(lat, lon)` pairs, using the
    /// standard precision of 5 decimal digits.
    pub fn decode(encoded: &str) -> Vec<(f64, f64)> {
        let bytes = encoded.as_bytes();
        let mut index = 0usize;
        let mut lat: i64 = 0;
        let mut lon: i64 = 0;
        let mut coords = Vec::new();
        const FACTOR: f64 = 1e5;

        while index < bytes.len() {
            match decode_value(bytes, index) {
                Some((dlat, next)) => {
                    lat += dlat;
                    index = next;
                }
                None => break,
            }
            match decode_value(bytes, index) {
                Some((dlon, next)) => {
                    lon += dlon;
                    index = next;
                }
                None => break,
            }
            coords.push((lat as f64 / FACTOR, lon as f64 / FACTOR));
        }

        coords
    }

    fn decode_value(bytes: &[u8], mut index: usize) -> Option<(i64, usize)> {
        let mut result: i64 = 0;
        let mut shift = 0u32;
        loop {
            let b = *bytes.get(index)? as i64 - 63;
            index += 1;
            result |= (b & 0x1f) << shift;
            shift += 5;
            if b < 0x20 {
                break;
            }
        }
        let delta = if (result & 1) != 0 {
            !(result >> 1)
        } else {
            result >> 1
        };
        Some((delta, index))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn decodes_known_example() {
            // From Google's documentation example.
            let points = decode("_p~iF~ps|U_ulLnnqC_mqNvxq`@");
            assert_eq!(
                points,
                vec![(38.5, -120.2), (40.7, -120.95), (43.252, -126.453)]
            );
        }
    }
}
