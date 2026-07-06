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
    // Entries are separated by newlines or commas. A source URL may itself
    // contain a comma (e.g. `?layers=roads,buildings`), which the comma split
    // would otherwise truncate — so within a line, a comma-separated fragment
    // with no `=` is folded back onto the previous entry's value as a literal
    // comma. Newlines are always hard separators.
    for line in raw.split('\n') {
        let mut pending: Option<String> = None;
        for frag in line.split(',') {
            if starts_new_alias(frag, pending.as_deref()) {
                insert_alias(&mut map, pending.take());
                pending = Some(frag.to_string());
            } else if !frag.trim().is_empty() {
                // Not a new `alias=...` entry and not just separator whitespace:
                // this is the tail of a value that contained a literal comma
                // (e.g. `?layers=roads,buildings`), so re-attach it.
                if let Some(p) = pending.as_mut() {
                    p.push(',');
                    p.push_str(frag);
                }
                // else: stray text with nothing pending — ignore.
            }
        }
        insert_alias(&mut map, pending.take());
    }
    map
}

/// Whether a comma-separated fragment begins a new `alias=source` entry (rather
/// than being the tail of a previous value that contained a comma). Requires the
/// text before the first `=` to be a plausible alias name (the character set
/// accepted for request names).
///
/// `pending` is the entry currently being accumulated, if any. With nothing
/// pending, any `valid-key=...` fragment starts an entry — including a
/// misconfigured non-URL one, so `insert_alias` can warn about it rather than
/// silently dropping it. When an entry is already pending, this fragment came
/// after a comma: it is a genuinely new entry only if it's clearly a fresh URL,
/// or if the pending value has no `?` — since only a query string can hold a
/// comma we'd want to fold back (e.g. `?layers=roads,format=png`).
fn starts_new_alias(frag: &str, pending: Option<&str>) -> bool {
    let Some((key, val)) = frag.split_once('=') else {
        return false;
    };
    let (key, val) = (key.trim(), val.trim());
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
    {
        return false;
    }
    match pending {
        None => true,
        // This fragment came after a comma while an entry is pending. Treat it
        // as a new entry when its value looks like a source — a URL, or a
        // local-path-shaped value we still want to recognize so `insert_alias`
        // can reject it rather than folding it into the previous URL — or when
        // the pending value has no `?` (so its comma can't be a query separator
        // we'd need to fold, e.g. `?layers=roads,format=png`).
        Some(pending) => looks_like_source(val) || !pending.contains('?'),
    }
}

/// Whether a value looks like a PMTiles source (a remote URL or a local path),
/// as opposed to a URL query-parameter fragment. Used to decide whether a
/// comma-separated fragment starts a new alias entry.
fn looks_like_source(val: &str) -> bool {
    val.starts_with("http://")
        || val.starts_with("https://")
        || val.starts_with('/')
        || val.starts_with('.')
        || val.ends_with(".pmtiles")
}

/// Parses a single `alias=source` entry and inserts it, if valid.
fn insert_alias(map: &mut HashMap<String, String>, entry: Option<String>) {
    let Some(entry) = entry else {
        return;
    };
    // Split on the first `=` only, so a source value may itself contain `=`
    // (e.g. a URL with a query string).
    let Some((alias, target)) = entry.split_once('=') else {
        return;
    };
    let (alias, target) = (alias.trim(), target.trim());
    if alias.is_empty() || target.is_empty() {
        return;
    }
    // `/tiles` archives are served only from a backend URL; a local disk path
    // is rejected so a misconfigured path can't be silently opened.
    if !(target.starts_with("http://") || target.starts_with("https://")) {
        tracing::warn!(
            alias,
            url = target,
            "ignoring PMTILES_ALIASES entry: source must be an http(s):// URL"
        );
        return;
    }
    map.insert(alias.to_string(), target.to_string());
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
    fn keeps_equals_and_commas_in_url_value() {
        // A `,` inside the URL (here a query value) must not truncate it, and
        // `=` in the query string is preserved (only the first `=` splits). The
        // `format=png` after the comma looks like `alias=value` but is a query
        // parameter, not a new entry, since its value isn't an http(s):// URL.
        let map = parse_aliases(Some("q=https://h/a.pmtiles?layers=roads,format=png&k=v"));
        assert_eq!(
            map.get("q").map(String::as_str),
            Some("https://h/a.pmtiles?layers=roads,format=png&k=v")
        );
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn splits_multiple_comma_separated_url_aliases() {
        // Two real aliases on one comma-separated line still split correctly,
        // because each value is an http(s):// URL.
        let map = parse_aliases(Some(
            "planet=https://h/p.pmtiles,firenze=https://h/f.pmtiles",
        ));
        assert_eq!(
            map.get("planet").map(String::as_str),
            Some("https://h/p.pmtiles")
        );
        assert_eq!(
            map.get("firenze").map(String::as_str),
            Some("https://h/f.pmtiles")
        );
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn skips_malformed_and_empty_entries() {
        let map = parse_aliases(Some("nourl,=noalias,alias=,ok=https://h/a.pmtiles"));
        assert_eq!(
            map.get("ok").map(String::as_str),
            Some("https://h/a.pmtiles")
        );
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn rejects_non_url_sources() {
        let map = parse_aliases(Some(
            "disk=/data/firenze.pmtiles\nrel=firenze.pmtiles\nok=https://h/a.pmtiles",
        ));
        assert_eq!(
            map.get("ok").map(String::as_str),
            Some("https://h/a.pmtiles")
        );
        assert!(map.get("disk").is_none());
        assert!(map.get("rel").is_none());
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn non_url_alias_next_to_valid_one_on_same_line_is_dropped_not_folded() {
        // A misconfigured non-URL entry sharing a comma-separated line with a
        // valid one must not corrupt the valid URL: it is recognized as its own
        // (rejected) entry rather than folded onto the previous value.
        let map = parse_aliases(Some(
            "planet=https://h/p.pmtiles,disk=/data/firenze.pmtiles",
        ));
        assert_eq!(
            map.get("planet").map(String::as_str),
            Some("https://h/p.pmtiles")
        );
        assert!(map.get("disk").is_none());
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn non_url_alias_after_query_url_on_same_line_is_dropped_not_folded() {
        // Even when the valid URL carries a query string (so it contains `?`), a
        // following non-URL entry must be recognized and rejected, not folded
        // into the query string.
        let map = parse_aliases(Some(
            "planet=https://h/p.pmtiles?token=123,disk=/data/firenze.pmtiles",
        ));
        assert_eq!(
            map.get("planet").map(String::as_str),
            Some("https://h/p.pmtiles?token=123")
        );
        assert!(map.get("disk").is_none());
        assert_eq!(map.len(), 1);
    }
}
