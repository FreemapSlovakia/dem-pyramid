# Panorama API

One endpoint. Give it a viewpoint, get back a rendered panorama, the peaks
visible from it, and optionally a per-pixel distance buffer.

```
POST /panorama      Content-Type: application/json
GET  /health        -> "ok"
```

The service is deliberately policy-free. Its limits bound how much work one
request can demand, not who may demand it — a proxy in front decides what each
user is allowed to ask for.

## Request

All fields except `lon` and `lat` are optional.

| field | type | default | meaning |
|---|---|---|---|
| `lon`, `lat` | number | — | viewpoint, WGS84 degrees |
| `az` | number | `0` | azimuth of the **left edge**, degrees clockwise from north |
| `fov` | number | `360` | horizontal field of view, degrees (0.1–360) |
| `alt_min` | number | `-18` | bottom of frame, degrees below horizontal |
| `alt_max` | number | `12` | top of frame, degrees above horizontal |
| `step` | number | `0.05` | degrees per output pixel (minimum 0.02) |
| `eye` | number | `1.7` | eye height above ground, metres |
| `eye_search_radius` | number | `10` | see [Viewpoint elevation](#viewpoint-elevation) (0–200) |
| `range` | number | `300000` | maximum distance considered, metres (1 000–400 000) |
| `supersample_x` | int | `9` | rays per output pixel horizontally (1–9) |
| `supersample_y` | int | `9` | sub-rows per output pixel vertically (1–9) |
| `depth` | bool | `false` | include the distance buffer |
| `depth_step` | int | `4` | depth quantisation; see [Depth](#depth) |
| `peaks` | bool | `true` | include peak labels |
| `min_prominence` | number | `0.05` | drop peaks below this angular prominence, degrees |

Image dimensions follow from the angles:

```
width  = round(fov / step)
height = round((alt_max - alt_min) / step)
```

`width × height` may not exceed 24 000 000. A 360° view at the default step is
7200 × 600.

### What the parameters cost

`supersample_x` multiplies the number of rays and is the main cost.
`supersample_y` costs no extra rays — it only changes buffer size — but is
needed for a different reason: where several ridges fall inside one output
pixel, only the nearest survives without it.

Measured on a 12-core machine, 360° at the default step:

| settings | rays | wall |
|---|---|---|
| `supersample 3×3` | 157 M | ~9 s |
| `supersample 9×9` | 470 M | ~27 s |

A 60–120° viewport is proportionally cheaper. Renders are serialised by
default, because one already saturates nine cores.

## Response

`multipart/form-data`, parseable directly with `Response.formData()`:

| part | filename | arrives as | contents |
|---|---|---|---|
| `meta` | — | string | JSON, see below |
| `image` | `panorama.png` | `Blob` | RGB PNG, `width × height` |
| `depth` | `depth.bin.gz` | `Blob` | gzip, only when `depth: true` |

A part with a filename arrives as a `Blob`; one without arrives as a string.
That is why `meta` can be `JSON.parse`d directly.

### `meta`

```json
{
  "width": 1200, "height": 320,
  "eye_elevation": 2653.2,
  "az_start": 300, "fov": 60,
  "alt_min": -8, "alt_max": 8,
  "step_deg": 0.05,
  "samples": 26092800,
  "depth": {
    "encoding": "u16-le log, row delta-coded, gzip",
    "near_m": 10, "far_m": 400000, "step": 4, "sky": 0
  },
  "peaks": [ ... ]
}
```

`depth` is `null` unless requested. `eye_elevation` is metres above sea level,
already including `eye`.

### Peaks

Only peaks that are **visible and pass `min_prominence`** are returned, sorted
by `prominence` descending.

```json
{
  "osm_id": 477984782,
  "name": "Babia Góra / Babia hora",
  "type": "peak",
  "ele_osm": "1725",
  "lon": 19.5296, "lat": 49.5731,
  "ele": 1722.6,
  "distance": 63133.4,
  "azimuth": 316.3,
  "altitude": -1.09,
  "x": 326.4, "y": 181.8,
  "visible": true,
  "prominence": 0.34
}
```

| field | meaning |
|---|---|
| `osm_id` | OSM node id — resolve names, `name:*`, wikidata etc. from OSM |
| `name`, `ele_osm` | as imported; `ele_osm` is the raw tag, unparsed |
| `ele` | elevation from the DTM — **prefer this**, OSM's `ele` is unreliable |
| `distance` | metres, great-circle |
| `azimuth` | degrees clockwise from north |
| `altitude` | degrees above horizontal from the eye, including curvature and refraction |
| `x`, `y` | position in the image, **output pixels**, origin top-left |
| `prominence` | degrees the summit stands above the skyline behind it |

`prominence` is angular, not metric, and it is the right thing to rank labels
by: a high summit seen edge-on behind a nearer ridge scores low, a modest hill
alone on the horizon scores high.

`x` and `y` are fractional — place labels at sub-pixel positions.

Only `osm_id` is needed to identify a peak; everything else about it (names in
every language, wikipedia, etc.) should come from OSM, since this service's
copy carries only what its import kept.

## Depth

A distance for every pixel, so the client can answer "how far is that ridge?"
under the cursor.

Encoding, in order: distance → logarithmic 16-bit → quantised by `depth_step`
→ delta-coded along each row → gzip. `0` means sky.

Logarithmic because the useful precision is relative — a metre matters at 200 m
and is meaningless at 200 km. One unit is 0.0162% of the distance, so
`depth_step: 4` is ±6 m at 20 km.

| `depth_step` | 360° size | error at 20 km |
|---|---|---|
| 1 | 1.16 MB | ±2 m |
| 4 | 0.79 MB | ±6 m |
| 16 | 0.48 MB | ±26 m |

### Decoding

> **Accumulate modulo 65536.** Values span 0–65535 while the deltas are signed
> 16-bit, so a delta between sky and distant terrain overflows deliberately.
> Masking makes it come out right; omitting the mask gives silently wrong
> distances, not an error.

```js
const res = await fetch('/panorama', {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ lon: 20.134, lat: 49.164, az: 300, fov: 60, depth: true }),
});

const fd    = await res.formData();
const meta  = JSON.parse(fd.get('meta'));
const image = URL.createObjectURL(fd.get('image'));

const deltas = new Int16Array(await new Response(
  fd.get('depth').stream().pipeThrough(new DecompressionStream('gzip'))
).arrayBuffer());

const { width, height } = meta;
const { near_m, far_m } = meta.depth;
const logNear = Math.log(near_m);
const logSpan = Math.log(far_m) - logNear;

const depth = new Uint16Array(width * height);
for (let row = 0; row < height; row++) {
  let acc = 0;
  for (let col = 0; col < width; col++) {
    const i = row * width + col;
    acc = (acc + deltas[i]) & 0xffff;      // <- the mask matters
    depth[i] = acc;
  }
}

/** Distance in metres at a pixel, or null for sky. */
function distanceAt(x, y) {
  const v = depth[y * width + x];
  return v === 0 ? null : Math.exp(logNear + ((v - 1) / 65534) * logSpan);
}
```

`DecompressionStream` is native in current browsers; no library is needed.

## Coordinates

- **Azimuth** — degrees clockwise from north. `az` is the *left edge* of the
  frame, not its centre.
- **Altitude** — degrees above horizontal from the eye, so the horizon is near
  0 and slightly negative from a summit.
- **Image** — origin top-left. `x = ((azimuth - az_start) mod 360) / step`,
  `y = (alt_max - altitude) / step`.

For a 360° render the image wraps: column `width - 1` is adjacent to column 0,
so it can be panned continuously or tiled as a cylinder.

## Errors

| status | when |
|---|---|
| `400` | `alt_max` not greater than `alt_min`; pixel limit exceeded |
| `500` | render failed — most often the viewpoint has no elevation data |
| `503` | shutting down |

Out-of-range numeric parameters are clamped rather than rejected.

## Caveats worth surfacing to users

**The DTM is bare earth.** Forests are invisible: a panorama sees through
20–40 m of canopy. libremap.sk behaves the same way and PeakFinder-class tools
accept it, but it is by far the largest source of error here — around 200×
bigger than the difference between national datasets at a border.

**Coverage varies.** Where national LiDAR exists the near field is derived from
1 m data at 6.27 m resolution; elsewhere the fallback is 30 m GEDTM30. The
transition is seamless but detail is not uniform.

**Viewpoint elevation is the local maximum** within `eye_search_radius`, not
the value at the exact point. The pyramid stores a 6.27 m average, and
averaging costs a summit more the sharper it is, so the raw value reliably sits
below where a person would stand — putting nearby rock above the eye. Set the
radius to 0 to disable, and expect summit views to suffer.

**Sub-pixel viewpoint accuracy matters on peaks.** A few metres off a summit
can place terrain above the eye. When a user taps a named peak, resolve to the
local maximum rather than passing the tapped coordinate through.
