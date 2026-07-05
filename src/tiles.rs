//! Tile access: fetching raw MVT bytes from a PMTiles archive (local file or
//! `http(s)://` URL) and decoding them into per-layer geo-types geometries.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use mvt_reader::error::ParserError;
use mvt_reader::feature::Feature;
use mvt_reader::Reader;
use pmtiles::{AsyncPmTilesReader, HashMapCache, HttpBackend, MmapBackend, TileCoord, TileType};

/// Upper bound on distinct opened sources kept alive. The `/tiles/{name}`
/// endpoint derives the source from a user-controlled `name`, so an unbounded
/// map would let an attacker exhaust memory by cycling through many names.
const MAX_CACHED_SOURCES: usize = 256;

/// Process-wide LRU cache of opened tile sources, keyed by source string.
/// Opening a PMTiles archive reads its header + root directory (a network
/// round-trip for remote URLs), and the reader is thread-safe and reusable with
/// its own internal caches — so open each source once and share it. Evicting an
/// entry only drops this map's handle; any request still using it holds its own
/// `Arc`, so the reader lives until the last user is done.
struct SourceCache {
    map: HashMap<String, Arc<TileSource>>,
    /// Keys ordered least- to most-recently used; the front is evicted first.
    lru: VecDeque<String>,
}

/// Returns a cached, shared handle to the opened source, opening it if needed.
pub async fn get_source(source: &str) -> anyhow::Result<Arc<TileSource>> {
    static SOURCES: OnceLock<Mutex<SourceCache>> = OnceLock::new();
    let cache = SOURCES.get_or_init(|| {
        Mutex::new(SourceCache {
            map: HashMap::new(),
            lru: VecDeque::new(),
        })
    });

    // `unwrap_or_else(into_inner)` recovers the guard even if a thread panicked
    // while holding the lock (poisoning), instead of cascading the panic.
    {
        let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = c.map.get(source).cloned() {
            touch_lru(&mut c.lru, source);
            return Ok(existing);
        }
    }
    // Open outside the lock so a slow open doesn't block; if a concurrent
    // request opened the same source first, keep that one.
    let opened = Arc::new(TileSource::open(source).await?);
    let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = c.map.get(source).cloned() {
        touch_lru(&mut c.lru, source);
        return Ok(existing);
    }
    c.map.insert(source.to_string(), opened.clone());
    c.lru.push_back(source.to_string());
    // Evict the least-recently-used sources until back within the cap.
    while c.lru.len() > MAX_CACHED_SOURCES {
        if let Some(evicted) = c.lru.pop_front() {
            c.map.remove(&evicted);
        }
    }
    Ok(opened)
}

/// Moves `source` to the most-recently-used end of the LRU order.
fn touch_lru(lru: &mut VecDeque<String>, source: &str) {
    if let Some(pos) = lru.iter().position(|k| k == source) {
        lru.remove(pos);
    }
    lru.push_back(source.to_string());
}

/// A PMTiles archive opened from either a local path or a remote URL.
pub enum TileSource {
    Path(AsyncPmTilesReader<MmapBackend>),
    Url(AsyncPmTilesReader<HttpBackend, HashMapCache>),
}

/// A process-wide HTTP client, so all remote sources share one connection pool
/// (avoids repeated TCP/TLS handshakes and socket churn). Timeouts are set so a
/// hung or slow remote can't tie up connections indefinitely.
fn http_client() -> pmtiles::reqwest::Client {
    use std::sync::OnceLock;
    use std::time::Duration;
    static CLIENT: OnceLock<pmtiles::reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            pmtiles::reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to build reqwest client")
        })
        .clone()
}

impl TileSource {
    pub async fn open(source: &str) -> anyhow::Result<Self> {
        if source.starts_with("http://") || source.starts_with("https://") {
            let cache = HashMapCache::default();
            let reader =
                AsyncPmTilesReader::new_with_cached_url(cache, http_client(), source).await?;
            Ok(TileSource::Url(reader))
        } else {
            let reader = AsyncPmTilesReader::new_with_path(source).await?;
            Ok(TileSource::Path(reader))
        }
    }

    pub async fn get_tile(&self, z: u8, x: u32, y: u32) -> anyhow::Result<Option<Bytes>> {
        let coord = TileCoord::new(z, x, y)?;
        let bytes = match self {
            TileSource::Path(r) => r.get_tile_decompressed(coord).await?,
            TileSource::Url(r) => r.get_tile_decompressed(coord).await?,
        };
        Ok(bytes)
    }

    /// The archive's tile type and zoom range, from the PMTiles header.
    pub fn header_info(&self) -> (TileType, u8, u8) {
        let h = match self {
            TileSource::Path(r) => r.get_header(),
            TileSource::Url(r) => r.get_header(),
        };
        (h.tile_type, h.min_zoom, h.max_zoom)
    }

    /// TileJSON (serialized) for this archive. `tile_url` is the full tile-URL
    /// template, e.g. `https://host/tiles/name/{z}/{x}/{y}.mvt`.
    pub async fn tilejson_string(&self, tile_url: String) -> anyhow::Result<String> {
        let tj = match self {
            TileSource::Path(r) => r.parse_tilejson(vec![tile_url]).await?,
            TileSource::Url(r) => r.parse_tilejson(vec![tile_url]).await?,
        };
        Ok(serde_json::to_string(&tj)?)
    }
}

/// The HTTP `Content-Type` for a PMTiles tile type.
pub fn tile_content_type(t: TileType) -> &'static str {
    match t {
        TileType::Mvt => "application/x-protobuf",
        TileType::Png => "image/png",
        TileType::Jpeg => "image/jpeg",
        TileType::Webp => "image/webp",
        _ => "application/octet-stream",
    }
}

/// The canonical file extension for a PMTiles tile type (empty if unknown).
pub fn tile_extension(t: TileType) -> &'static str {
    match t {
        TileType::Mvt => "mvt",
        TileType::Png => "png",
        TileType::Jpeg => "jpg",
        TileType::Webp => "webp",
        _ => "",
    }
}

pub struct DecodedLayer {
    pub name: String,
    /// Tile-local coordinate extent (typically 4096).
    pub extent: u32,
    pub features: Vec<Feature<f32>>,
}

/// Decodes a single MVT (Mapbox Vector Tile) protobuf blob into a simple
/// per-layer list of geo-types geometries, using the `mvt-reader` crate.
pub fn decode_tile(bytes: Vec<u8>) -> Result<Vec<DecodedLayer>, ParserError> {
    let reader = Reader::new(bytes)?;
    let metas = reader.get_layer_metadata()?;

    let mut layers = Vec::with_capacity(metas.len());
    for meta in metas {
        let features = reader.get_features(meta.layer_index)?;
        layers.push(DecodedLayer {
            name: meta.name,
            extent: meta.extent,
            features,
        });
    }
    Ok(layers)
}
