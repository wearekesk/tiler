//! Kesk Maps Server: an HTTP service for map imagery from PMTiles archives.
//!
//! - `GET /staticmap` — composed static map images (see [`handlers::staticmap`]).
//! - `GET /tiles/...` — raw PMTiles tiles + TileJSON (see [`handlers::tiles`]).
//!
//! Configured via the environment (a `.env` file is loaded at startup):
//! `PMTILES_URL` (staticmap source), `PMTILES_PATH` (tile-server archive name
//! template), `PORT`, `RUST_LOG`, `OTEL_EXPORTER_OTLP_ENDPOINT`,
//! `ALLOWED_ORIGINS`, `TILES_CACHE_CONTROL`, `PUBLIC_HOSTNAME`.

mod config;
mod geo_util;
mod handlers;
mod params;
mod render;
mod style;
mod tiles;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use poem::listener::TcpListener;
use poem::middleware::{CatchPanic, Cors, SetHeader, Tracing};
use poem::{get, EndpointExt, Route, Server};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// OpenTelemetry `service.name` for traces.
const SERVICE_NAME: &str = "kesk-tiler";
/// Outward-facing server name (the HTTP `Server` header).
const SERVER_NAME: &str = "Kesk Maps Server";

/// Initializes `tracing` (structured logs + spans). When
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set, spans are also exported to that
/// OpenTelemetry collector over OTLP/gRPC. The log level is controlled by
/// `RUST_LOG` (default `info`). Returns the tracer provider (if OTLP is
/// enabled) so the caller can keep it alive for the process lifetime.
fn init_telemetry() -> Option<SdkTracerProvider> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let provider = if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        match opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build()
        {
            Ok(exporter) => Some(
                SdkTracerProvider::builder()
                    .with_batch_exporter(exporter)
                    .with_resource(Resource::builder().with_service_name(SERVICE_NAME).build())
                    .build(),
            ),
            Err(e) => {
                eprintln!("OTLP exporter init failed, continuing without export: {e}");
                None
            }
        }
    } else {
        None
    };

    let otel_layer = provider.as_ref().map(|p| {
        opentelemetry::global::set_tracer_provider(p.clone());
        tracing_opentelemetry::layer().with_tracer(p.tracer(SERVICE_NAME))
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(otel_layer)
        .init();

    provider
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    dotenv::dotenv().ok();
    // Kept alive for the process lifetime so buffered spans keep flushing.
    let _telemetry = init_telemetry();

    // Load the system font database now so the first render doesn't pay for it.
    render::warm_fontdb();

    // CORS: allow any origin by default, or restrict to a comma-separated
    // `ALLOWED_ORIGINS` list (`*` also means any).
    let cors = match config::allowed_origins() {
        Some(v) if !v.trim().is_empty() && v.trim() != "*" => {
            Cors::new().allow_origins(v.split(',').map(|s| s.trim().to_string()))
        }
        _ => Cors::new(),
    };

    // `CatchPanic` (innermost) turns any handler panic into a 500 instead of
    // killing the worker; `Tracing` (outermost) still logs the response.
    let app = Route::new()
        .at("/staticmap", get(handlers::staticmap::static_map))
        // Raw PMTiles tile server (z/x/y tiles + TileJSON); `name` may be nested
        // so match everything under /tiles.
        .at("/tiles", get(handlers::tiles::serve))
        .at("/tiles/*path", get(handlers::tiles::serve))
        .with(cors)
        .with(SetHeader::new().overriding("Server", SERVER_NAME))
        .with(CatchPanic::new())
        .with(Tracing);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    tracing::info!(port, "{SERVER_NAME} listening on http://0.0.0.0:{port}");
    Server::new(TcpListener::bind(format!("0.0.0.0:{port}")))
        .run(app)
        .await
}
