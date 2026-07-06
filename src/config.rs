//! Cached access to environment configuration. Each value is read from the
//! environment once, on first use, and cached for the process lifetime.

use std::collections::HashMap;
use std::sync::OnceLock;

fn cached(cell: &'static OnceLock<Option<String>>, name: &str) -> Option<&'static str> {
    cell.get_or_init(|| std::env::var(name).ok()).as_deref()
}

/// Defines `pub fn $name() -> Option<&'static str>` returning the cached value
/// of environment variable `$var`.
macro_rules! env_str {
    ($(#[$m:meta])* $name:ident => $var:literal) => {
        $(#[$m])*
        pub fn $name() -> Option<&'static str> {
            static C: OnceLock<Option<String>> = OnceLock::new();
            cached(&C, $var)
        }
    };
}

env_str!(
    /// The `/staticmap` PMTiles source.
    pmtiles_url => "PMTILES_URL"
);
env_str!(
    /// CORS allow-list (comma-separated origins, or `*`).
    allowed_origins => "ALLOWED_ORIGINS"
);
env_str!(
    /// Hostname used in TileJSON `tiles` URLs (falls back to the request Host).
    public_hostname => "PUBLIC_HOSTNAME"
);

/// Parsed alias map from `PMTILES_ALIASES`, mapping a friendly archive name to a
/// backend `http(s)://` PMTiles source. Entries are separated by commas or
/// newlines, each `alias=source`. This is the sole source of `/tiles` archives:
/// a name must match an alias to be served, e.g. `planet=https://host/x.pmtiles`
/// makes `/tiles/planet/...` resolve to that archive. Sources must be remote
/// URLs — a local disk path is a configuration error and is dropped (with a
/// warning). Reads `PMTILES_ALIASES` once, on first use.
pub fn pmtiles_aliases() -> &'static HashMap<String, String> {
    static C: OnceLock<HashMap<String, String>> = OnceLock::new();
    C.get_or_init(|| parse_aliases(std::env::var("PMTILES_ALIASES").ok().as_deref()))
}

fn parse_aliases(raw: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(raw) = raw else {
        return map;
    };
    // Entries are separated by commas or newlines, each `alias=source`. PMTiles
    // source URLs don't contain commas, so a plain split is enough; put a URL
    // with a comma on its own line if you ever need one.
    for entry in raw.split([',', '\n']) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // Split on the first `=` only, so a source value may itself contain `=`
        // (e.g. a URL with a query string).
        let Some((alias, target)) = entry.split_once('=') else {
            continue;
        };
        let (alias, target) = (alias.trim(), target.trim());
        if alias.is_empty() || target.is_empty() {
            continue;
        }
        // `/tiles` archives are served only from a backend URL; a local disk
        // path is rejected so a misconfigured path can't be silently opened.
        if !(target.starts_with("http://") || target.starts_with("https://")) {
            tracing::warn!(
                alias,
                url = target,
                "ignoring PMTILES_ALIASES entry: source must be an http(s):// URL"
            );
            continue;
        }
        map.insert(alias.to_string(), target.to_string());
    }
    map
}

/// `Cache-Control` for `/tiles` responses (defaulted).
pub fn tiles_cache_control() -> &'static str {
    static C: OnceLock<Option<String>> = OnceLock::new();
    cached(&C, "TILES_CACHE_CONTROL").unwrap_or("public, max-age=86400")
}

#[cfg(test)]
mod tests {
    use super::parse_aliases;

    #[test]
    fn none_is_empty() {
        assert!(parse_aliases(None).is_empty());
    }

    #[test]
    fn parses_comma_and_newline_separated_url_entries() {
        let map = parse_aliases(Some(
            "planet = https://build.protomaps.com/20260702.pmtiles ,\n basemap=http://host/basemap.pmtiles",
        ));
        assert_eq!(
            map.get("planet").map(String::as_str),
            Some("https://build.protomaps.com/20260702.pmtiles")
        );
        assert_eq!(
            map.get("basemap").map(String::as_str),
            Some("http://host/basemap.pmtiles")
        );
    }

    #[test]
    fn rejects_non_url_sources() {
        // Only http(s):// sources are kept; a disk path is dropped.
        let map = parse_aliases(Some("disk=/data/firenze.pmtiles,ok=https://h/a.pmtiles"));
        assert_eq!(
            map.get("ok").map(String::as_str),
            Some("https://h/a.pmtiles")
        );
        assert!(!map.contains_key("disk"));
        assert_eq!(map.len(), 1);
    }
}
