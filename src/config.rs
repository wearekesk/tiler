//! Cached access to environment configuration. Each value is read from the
//! environment once, on first use, and cached for the process lifetime.

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
    /// The `/tiles` archive-name template (`{name}` is substituted).
    pmtiles_path => "PMTILES_PATH"
);
env_str!(
    /// CORS allow-list (comma-separated origins, or `*`).
    allowed_origins => "ALLOWED_ORIGINS"
);
env_str!(
    /// Hostname used in TileJSON `tiles` URLs (falls back to the request Host).
    public_hostname => "PUBLIC_HOSTNAME"
);

/// `Cache-Control` for `/tiles` responses (defaulted).
pub fn tiles_cache_control() -> &'static str {
    static C: OnceLock<Option<String>> = OnceLock::new();
    cached(&C, "TILES_CACHE_CONTROL").unwrap_or("public, max-age=86400")
}
