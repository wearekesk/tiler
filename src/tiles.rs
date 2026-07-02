//! Tile access: fetching raw MVT bytes from a PMTiles archive (local file or
//! `http(s)://` URL) and decoding them into per-layer geo-types geometries.

use bytes::Bytes;
use mvt_reader::error::ParserError;
use mvt_reader::feature::Feature;
use mvt_reader::Reader;
use pmtiles::{AsyncPmTilesReader, HashMapCache, HttpBackend, MmapBackend, TileCoord};

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
