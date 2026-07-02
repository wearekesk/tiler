# tiler

A small **static maps HTTP endpoint** — similar in spirit to the Google Maps
Static API — that renders SVG/PNG map images on the fly from a
[PMTiles](https://protomaps.com/docs/pmtiles) vector-tile archive.

Give it a center/zoom (or let it auto-fit to your markers), and it fetches the
covering vector tiles, styles them with a Google-Maps-like palette, draws your
markers and route paths, and returns a PNG or SVG.

```
GET /staticmap?center=43.7715,11.2540&zoom=15&size=640x480
      &markers=color:red|label:A|43.7715,11.2540
      &path=color:0x1a73e8ff|weight:5|43.768,11.250|43.773,11.258
```

## Features

- **PNG or SVG** output, at `scale` 1/2/4 for HiDPI.
- **Seamless tiling.** Tiles are composited layer-major (painter's algorithm
  across the whole scene) and each fill is sealed with a stroke of its own
  color, so there are no grey seams where tiles meet.
- **Google-Maps-style** default palette (land, water, parks, buildings, roads,
  railways, waterways, boundaries).
- **Markers** with color, single-letter label, and size.
- **Route paths** with color/weight/fill, including Google
  [encoded-polyline](https://developers.google.com/maps/documentation/utilities/polylinealgorithm)
  (`enc:`) support. The response carries an `x-route-length-meters` header.
- **Auto-fit viewport** — omit `center`+`zoom` and the viewport is fit to your
  `markers` / `path` / `visible` points.
- **Per-feature style overrides** via the `style` parameter.
- **De-duplicated labels** across tile boundaries.
- **Aggressive HTTP caching** (`Cache-Control` + `ETag`, with `304 Not Modified`
  short-circuiting).
- **Observability** — structured logs and spans via `tracing`, exported to an
  OpenTelemetry collector over OTLP when configured.

## Running

```sh
cp .env.example .env    # then edit PMTILES_URL if you like
cargo run
# tiler (service name: kesk-tiler) listening on http://0.0.0.0:3000/staticmap
```

Then open, for example:

```
http://localhost:3000/staticmap?center=43.7715,11.2540&zoom=15&size=640x480&format=png
```

### Configuration

All configuration is via environment variables (a `.env` file is loaded at
startup):

| Variable | Default | Description |
| --- | --- | --- |
| `PMTILES_URL` | — | Default PMTiles source (local path or `http(s)://` URL). Used when a request omits `pmtiles`. |
| `PMTILES_ALLOW_PARAM` | `false` | Allow requests to select their own source via the `pmtiles` query param. Off by default — it permits arbitrary local-file/URL access (SSRF), so only enable it for trusted deployments. |
| `PORT` | `3000` | Port to listen on. |
| `RUST_LOG` | `info` | Log/trace filter (e.g. `info,tiler=debug`). |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | — | When set, spans are exported to this OTLP/gRPC collector (e.g. `http://localhost:4317`). |

Output size is capped: `size` is at most `4096x4096`, and the *scaled* output
(`size` × `scale`) is capped at `8192` px per side to bound memory.

## API

`GET /staticmap`

| Parameter | Repeatable | Description |
| --- | --- | --- |
| `size=WxH` | | **Required.** Output size in pixels (max `4096x4096`). |
| `pmtiles=PATH_OR_URL` | | PMTiles source; falls back to `PMTILES_URL`. Only honored when `PMTILES_ALLOW_PARAM` is enabled (otherwise `403`). |
| `center=LAT,LON` & `zoom=Z` | | Viewport center and zoom. If omitted, the viewport auto-fits the points below. |
| `scale=1\|2\|4` | | Pixel density multiplier (default `1`). |
| `format=png\|svg` | | Output format (default `png`). |
| `markers=[color:C\|label:L\|size:S\|]LAT,LON\|…` | ✓ | A marker group. `size` is `tiny\|small\|mid`. |
| `path=[color:C\|weight:W\|fillcolor:C\|]LAT,LON\|…` | ✓ | A route path. Accepts `enc:<polyline>` instead of coordinate pairs. |
| `style=feature:F\|color:C\|weight:W` | ✓ | Override a feature bucket's color/weight. `F` is one of `earth,landuse,water,waterway,building,road,transit,boundary,other`, or `all`. |
| `visible=LAT,LON\|…` | ✓ | Extra points to include when auto-fitting the viewport. |

Colors accept CSS names, `#rrggbb[aa]`, or Google-style `0xrrggbb[aa]`.

### Examples

Auto-fit around a route:

```
/staticmap?size=600x400&path=color:0x1a73e8ff|weight:5|43.768,11.250|43.773,11.258|43.775,11.250
```

Two marker groups and a filled polygon:

```
/staticmap?size=600x400&center=43.77,11.25&zoom=14
  &markers=color:red|label:A|43.771,11.254
  &markers=color:green|size:small|43.768,11.250|43.775,11.258
  &path=fillcolor:0x00ff0033|color:0x008800ff|43.770,11.250|43.773,11.256|43.769,11.258
```

## How it works

The source is organized as a handful of modules:

- `main.rs` — HTTP server (poem), request parsing, caching, and telemetry setup.
- `tiles.rs` — opening a PMTiles source and decoding MVT tiles.
- `geo_util.rs` — Web-Mercator projection and viewport/tile math.
- `params.rs` — query-string parsing (markers, paths, styles, encoded polylines).
- `style.rs` — the Google-Maps-like palette and per-layer styling.
- `render.rs` — SVG assembly (layer-major compositing, fill sealing) and PNG rasterization.

### Why the tiles are seamless

Vector features are clipped per tile, and a naive tile-by-tile renderer draws
each tile's full-tile background *after* its neighbor's foreground, so the
background repaints over the seam — a visible grey line along every tile edge.
`tiler` instead composites **layer-major across all tiles** (all `earth`, then
all `landuse`, then buildings, …) so a background never lands on top of a
neighbor's foreground, and seals each fill with a 1px stroke of its own color to
cover the sub-pixel anti-aliasing gap where two fills meet. Together these
remove the seams — without distorting feature geometry.
