# Panorama API

One endpoint. Give it a viewpoint, get back a rendered panorama, the peaks
visible from it, and optionally a per-pixel distance buffer.

```
POST /panorama      Content-Type: application/json
GET  /health        -> "ok"
```

## Where it runs

`dem-pyramid serve` on fm6, bound to `127.0.0.1:3100` — loopback only, not
reachable from outside the host.

**Clients do not call it directly.** The service is deliberately policy-free:
its limits bound how much work one request can demand, not who may demand it.
`freemap-v3-api` proxies it and clamps quality per user, the same way it
already gates elevation sources on `premiumExpiration` — for example
`supersample 3` / `step 0.1` for anonymous users and `9` / `0.05` for premium.

Renders are serialised server-side (one at a time by default), so the proxy
should expect to queue.

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
| `min_dominance` | number | `30` | drop peaks standing less than this above their surroundings, metres; may be negative |
| `max_peaks` | int | `0` | keep at most this many, most label-worthy first — *not* most dominant, see [What `max_peaks` keeps](#what-max_peaks-keeps); 0 is no cap |
| `format` | string | `avif` | image encoding: `avif` or `png` |
| `quality` | int | `95` | AVIF quality, 1–100; ignored for PNG |
| `dither_strength` | number | `1.5` | 8-bit dither amplitude, levels; 0 disables |
| `ridge_strength` | number | `1` | multiplier on the silhouettes' alpha; 0 removes them, no upper bound |
| `ridge_width` | number | `1` | silhouette thickness in output pixels (0–20) |
| `ridge_color` | string | `#000000` | silhouette colour, `#rrggbb` or `#rgb` |
| `ground_color` | string | `#3a4a34` | near-terrain colour, before haze |
| `depth_lift` | number | `0` | degrees of extra elevation at `range`, tapering to nothing at the eye (0–45); see [Depth lift](#depth-lift) |
| `revealed_peaks` | bool | `true` | let summits only `depth_lift` brought into view take label slots |
| `peak_rank_power` | number | `0.5` | exponent on distance in the `max_peaks` cut (0–4); `0` ranks on raw dominance |
| `peak_profile_step` | number | `0.2` | degrees per column of the grid `dominance` is measured on (0.02–2), independent of `step` and `supersample_x`; held to the ray spacing if finer |

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

Measured on a 12-core machine, 360°, peaks and depth included:

| settings | wall |
|---|---|
| `step 0.1`, `supersample 3×3` | ~5 s |
| `step 0.05`, `supersample 3×3` | ~9 s |
| `step 0.05`, `supersample 9×9` | ~27 s |

A 60–120° viewport is proportionally cheaper. Peaks add roughly a second;
depth costs almost nothing.

#### What peaks cost, and what `max_peaks` does not

The peak pass scales with **candidates within `range`**, not with how many you
ask to keep. Everything expensive — reading each summit's elevation, answering
the two rays that bracket its bearing, and the dominance sweep — runs over the
whole candidate set, before `max_peaks` is looked at. On a 360° Ötztal view
with 52 728 candidates in range, peaks took the render from **11.4 s to 13.5 s**
(best of three, on a busy host): about 2 s, and that 2 s is the same whether
you keep 50 peaks or all 2631.

**`max_peaks` is a truncation before serialization and nothing else.** Raising
it costs the server no measurable time — only bytes:

| peaks returned | payload |
|---|---|
| 50 | 415 KB |
| 2000 | 1085 KB |
| 2631 (uncapped) | 1302 KB |

That is **~344 B per peak**, linear and consistent to 0.2% across the range
(`bytes ≈ 397 500 + 344·n` on that view). So a cap is a client-side bandwidth
decision, not a server-load one. The bytes go on full `f64` precision —
`"distance": 116483.52984247255`, `"ele": 3198.131752006181` — so if the
payload matters more than the resolution, say so and the numbers can be
rounded to something defensible (0.1 m, 6 decimal places of latitude) for
roughly a quarter off.

**`min_dominance` is not a load control either.** It is applied in the same
place as `max_peaks`, after every visible candidate has already had its
dominance measured, so raising it returns no time — only fewer bytes. The one
parameter that genuinely reduces the peak work is `range`, because it decides
how many candidates exist at all; `peaks: false` removes it entirely.

Three things multiply the **wall-clock** figures above, and they compound:

- **A cold viewpoint is about twice a warm one** — 9.6 s against 4.9 s for the
  same request — because the pyramid blocks come off disk the first time.
- **Requests are serialised**, so a second one waits for the first: two
  overlapping requests measured 8.9 s and 14.9 s.
- **Other load on the host** stretches everything, since a render already uses
  nine of twelve cores.

So budget for several times the table under real conditions, and treat these as
a floor rather than a promise.

### Image format

AVIF by default, and by a wide margin: at the shipped defaults a 7200×600
render is **292 KB against 4.6 MB** as PNG. `format: "png"` is still there for
callers written before it.

> **The default changed, so a client that sends no `format` now receives AVIF**
> — and the `image` part's filename changes from `panorama.png` to
> `panorama.avif`. Read the part by its field name, `image`, not by filename,
> and it does not matter. If you need the old bytes, send `format: "png"`
> explicitly rather than relying on the default.

The reason PNG is so poor here is the sky dither. This renderer draws nothing
but smooth gradients, which PNG compresses beautifully — until a level of
noise lands on every pixel to stop the sky banding, and that noise is exactly
what PNG cannot pack. The same render was ~600 KB before dithering.

`quality` therefore matters more than it usually would, because it decides
whether the dither survives:

| setting | size | worst step in a soft sky |
|---|---|---|
| PNG | 3708 KB | 0.04 levels |
| q95, 8-bit | 125 KB | 1.12 — visible |
| q99, 8-bit | 358 KB | 0.63 |
| **q95, 10-bit** | **216 KB** | **0.28** — the default |
| q97, 10-bit | 411 KB | 0.19 |

Encoding is 10-bit even though the picture and your display are both 8-bit.
The headroom is for the encoder: its quantisation error then lands below what
8-bit output can show, and the decoder rounds back down. It beats spending the
same bytes on quality — q99 at 8 bits is larger than q95 at 10 and still steps
a full level.

Below q93 the encoder stops preserving the sky dither at all and the banding
comes back outright. That threshold moves with the picture, since rate
allocation gives a smooth frame fewer bits, so a plain sky is where the noise
goes first. Other codecs were measured and are worse: JPEG at q85 flattens 91
of 100 sky rows, and lossless AVIF is 2.5 MB — worse than lossless WebP.

Encoding costs about 3 seconds on a 25-second render. Depth is unaffected: it
travels as its own lossless part whatever the image format.

### Dithering

The sky is a very slow gradient — about one 8-bit level per twenty rows — so it
is dithered before quantisation, or it bands. `dither_strength` is the
amplitude in levels, and it is worth knowing why the default is not the
textbook 1:

| strength | longest flat run in the sky | image |
|---|---|---|
| 0 | 25 px | 216 KB |
| 1 | 18 px | 216 KB |
| **1.5** | **3 px** | **292 KB** |
| 2 | 2 px | 760 KB |

At 1, where the true value sits near a whole number the dither almost never
flips it, so the sky alternates between flat stretches and dithered ones —
banding that has been half-fixed rather than fixed. 1.5 is where that stops,
and where the size curve turns: noise is exactly what an image codec cannot
compress, so going further costs a great deal for one more pixel of flatness.

Set `0` to see the undithered gradient — useful for telling a dithering
problem apart from anything else in the picture.

### Styling

Four knobs, all optional, all defaulting to exactly what the renderer drew
before they existed — omit them and nothing changes.

`ridge_strength` is a gain on the alpha of the silhouettes the renderer
strokes along ridge lines. It is a multiplier, not an opacity: the geometry
inks a near ridge at about 0.55 alpha and a distant one at 0.15, so `1` is
translucent already and there would be no way to draw a solid line if the
field stopped there.

| value | effect |
|---|---|
| `0` | no linework — plain shaded relief |
| `1` | default |
| ~1.8 | nearest ridges reach full ink |
| ~4 | mid-distance ridges solid, strokes start merging |
| ~6.7 | even the haziest distant ridges saturate |

There is no upper limit — alpha is clamped at composite time, so a large value
simply makes everything solid. Negative is rejected as meaningless: alpha is
clamped at zero too, so it would draw exactly what `0` draws.

`ridge_width` is thickness in **output pixels**, so a line looks the same
weight whatever `step` you render at — sub-rows are only how it is drawn.
It is independent of `ridge_strength`: the interior of a stroke inks at the
same alpha however wide it is, so widening thickens a line without darkening
it. Fractional values work, and antialias.

Unlike strength this one *is* bounded, to 20. Every stroke inks a band of
rows, so the cost of the pass grows with the width, and an unbounded value
would let one request paint the full column height for every edge it found.

`ridge_color` is what those strokes are drawn in. Black is the default and
reads as shading, since inking towards black is a plain multiply. A colour
near the ground makes ridges read as folds rather than outlines, and a light
ink on dark ground gives an engraved effect.

`ground_color` is near terrain before haze washes it towards the sky. Haze is
unchanged, so a warm ground still fades to the same blue with distance —
which is what keeps depth readable whatever colour you choose.

```jsonc
{ "ridge_strength": 0 }                        // shaded relief, no linework
{ "ridge_strength": 2.2, "ground_color": "#7a6a4a" }   // drawn, sandy
{ "ridge_color": "#2b1a10", "ground_color": "#d8cfc0" } // engraved
{ "ridge_width": 2.5, "ridge_strength": 3 }    // bold outlines, poster-like
{ "ridge_width": 0.5 }                         // hairlines for a big render
```

Both colours take `#rrggbb` or `#rgb`, with or without the `#`. A malformed
colour is a `400` naming the field, not a silently ignored parameter.

### Depth lift

A true panorama wastes its frame. From Kráľova hoľa the Low Tatra ridge four
kilometres off fills a third of the picture, while the High Tatras — higher,
and the reason anyone looks north — get a sliver above it. Distance compresses
everything towards the horizon, and the interesting part is all at distance.

`depth_lift` unfolds that. Terrain is raised in proportion to how far away it
is: nothing at the eye, the full amount at `range`, linear in between. Set it
to 3 with `range: 200000` and a range 100 km out rises 1.5°, one at 200 km
rises 3°, and the layers separate.

```jsonc
{ "depth_lift": 1.5, "alt_max": 13.5 }   // gentle; ranges begin to separate
{ "depth_lift": 3,   "alt_max": 15 }     // strong, drawn-panorama look
```

**It shows you things you cannot see.** This is not a display trick applied
after the fact — the lift raises the world, and what is hidden is decided in
the raised world, so a range lifted clear of the ridge in front of it becomes
visible. That is the point: it is what makes hand-drawn panoramas legible, and
without it the lift tears the picture apart (see below). But the result is no
longer a photograph, and anything built on "the user can see this from here"
has to account for it. Peaks brought into view this way come back with
`revealed: true` — see [Peaks](#peaks).

**`max_peaks` needs `revealed_peaks: false` to stay honest.** Dominance is in
metres, so distant ranges outrank near hills — and revealed summits are distant
by construction, being the ones that were behind something. Under a lift they
sort to the top and a `max_peaks: 20` request can come back with twenty labels
naming nothing you can see, the near summits truncated away. Sending
`revealed_peaks: false` drops them *before* the cap, so the twenty slots go to
summits genuinely in sight. Filtering on `revealed` client-side cannot recover
this: truncation has already happened, and you are left with fewer labels than
you asked for.

Three things to expect:

- **Raise `alt_max` by roughly `depth_lift`.** The horizon moves up by exactly
  that much, so far ridges climb out of an unchanged frame.
- **Pair it with a sensible `range`.** With a strong lift the topmost
  silhouette becomes *whatever is farthest*, and at 200 km the elevation angle
  is set almost entirely by curvature drop — the same in every direction. It
  draws as a dead-flat, fully hazed line across the sky. At `range: 100000`
  the same view layers cleanly instead.
- **It is free.** The marcher walks the same rays and reads the same samples
  either way — measured at 118 566 000 samples for `depth_lift` 0, 1.5 and 3
  alike, with the render times inside run-to-run noise. Only the arithmetic
  deciding where a sample lands changes.

There is no variant that lifts the picture while keeping true visibility. It
was built, and it does not work: the lift opens a vertical gap between a near
crest and the range behind it, the renderer has nothing to put in that gap but
a stretched copy of the far surface, and distant ranges come out as flat-topped
slabs with vertical sides — taller the more lift is asked for. The only honest
filling for that gap is the terrain genuinely behind the crest, which is what
the lift does now.

## Queueing and cancellation

**One render at a time.** A single render already saturates nine of twelve
cores, so overlapping them would trade latency for nothing. Requests queue.

**Priority, not FIFO.** Among requests *already waiting*, the one with the
highest priority goes next. There is no preemption: a request arriving at an
idle service starts immediately whatever its priority, and a render in progress
is never interrupted. With one render already occupying the slot, three
requests submitted anonymous → plus → premium were served premium → plus →
anonymous.

Priority comes from the **`X-Priority` header, not the request body**, and the
public vhost sets it to `0` unconditionally. A caller cannot promote itself;
only something reaching the service on loopback, having authenticated the user,
can raise it.

Waiting also counts: a waiter gains 0.2 priority per second, so it overtakes a
fresh request ten points above it after fifty seconds. Without that, a steady
trickle of premium requests would starve anonymous ones indefinitely.

**Queue depth is capped.** Beyond 32 waiting, the service returns `503` rather
than accepting work nobody will reach.

**Aborting the connection cancels the work.** Hang up — `AbortController`,
navigation, a closed tab — and the render stops within about a second, whether
it was queued or already running. Measured: 11.3 cores in use, 0.3 two seconds
after the client died, rather than grinding on for the remaining 24 s.

So **abort the previous request before issuing a new one**, or a user
reframing a view queues behind their own abandoned work.

But **debounce reframes too**. The public vhost rate-limits per IP, and while
the burst allowance is sized for the abort-and-resubmit pattern, a user
dragging continuously can still exhaust it and get `503` from nginx instead of
a render. Wait for the interaction to settle before submitting.

`X-Queue-Depth` on the response reports how many requests were ahead on
arrival, counting the one already rendering — so `0` genuinely means the
service was idle.

## Response

`multipart/form-data`, parseable directly with `Response.formData()`:

| part | filename | arrives as | contents |
|---|---|---|---|
| `meta` | — | string | JSON, see below |
| `image` | `panorama.avif` | `Blob` | RGB image, `width × height`; `panorama.png` when `format: "png"` |
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
  "peak_profile_step": 0.2,
  "samples": 26092800,
  "depth": {
    "encoding": "u16-le log, row delta-coded, gzip",
    "near_m": 10, "far_m": 400000, "step": 4, "sky": 0
  },
  "peaks": [ ... ]
}
```

`depth` is `null` unless requested. `eye_elevation` is metres above sea level,
already including `eye`. `peak_profile_step` is the grid `dominance` was
measured on — the requested value, or the ray spacing where that was coarser.
Two requests return the same peaks only if they agree here.

### Peaks

Only peaks that are **visible and pass `min_dominance`** are returned, sorted
by label rank and then cut to `max_peaks`. Because the cut comes after the
sort, a small `max_peaks` keeps the summits worth labelling rather than an
arbitrary slice — it is the cheapest way to control label density, and it
costs the server nothing.

#### What `max_peaks` keeps

Not the highest `dominance`. Dominance is in metres and **metres are not a
rank**: a 400 m top sixty kilometres out scores far above a 90 m one two
kilometres away, while the near one fills more of the frame and is what a
viewer is looking at. The cut therefore orders by

```
rank = dominance / distance ** peak_rank_power     where dominance >= 0
rank = dominance * distance ** peak_rank_power     where dominance <  0
```

with `peak_rank_power` defaulting to `0.5`. **`peak_rank_power: 0` gives the
old ordering exactly** — raw dominance, both branches collapsing to it.

The two branches are not a quirk. Dominance is signed, and dividing a negative
by a larger number *raises* it, so a single expression that penalises a distant
peak starts rewarding one the moment its dominance goes negative — which in
ridge country is most visible tops. Measured on the 2631-peak Ötztal view that
prompted this: a near subordinate top sat 2471st under raw dominance, **2627th**
under naive division, and **1966th** under the rule above, which is what brings
it inside a `max_peaks: 2000` cut.

`min_dominance` still filters on **raw** metres, deliberately: it is a
statement about the landscape — "nothing flatter than this is a summit" — and
would stop meaning anything in metres if distance entered it. Only the cut is
ranked. The two can in principle disagree, a near top failing the threshold
while ranking above distant ones that pass; measured on the view above it does
not happen at any cap, because the weighting already pushes deeply negative
scores down, which is the same direction the threshold cuts in.

`dominance` itself is untouched in the payload, so a client is free to re-rank
however it likes — the ordering only decides *which* peaks survive to be sent.
If your client uses its own exponent, send the same one here so the cut and the
re-rank agree. Raising the exponent favours the near field steadily: on that
same view the top moves 2471 → 2249 → 1966 → 1647 → 1419 for
`peak_rank_power` 0 → 0.25 → 0.5 → 0.75 → 1.

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
  "revealed": false,
  "dominance": 412.8
}
```

| field | meaning |
|---|---|
| `osm_id` | OSM node id — resolve names, `name:*`, wikidata etc. from OSM |
| `name`, `ele_osm` | as imported; `ele_osm` is the raw tag, unparsed |
| `ele` | elevation from the DTM — **prefer this**, OSM's `ele` is unreliable |
| `distance` | metres, great-circle |
| `azimuth` | degrees clockwise from north |
| `altitude` | degrees above horizontal from the eye, including curvature and refraction — the true angle, unaffected by `depth_lift` |
| `x`, `y` | position in the image, **output pixels**, origin top-left — `y` *does* follow `depth_lift`, so place labels by this, not by `altitude` |
| `revealed` | `true` where `depth_lift` is what brought the summit into view: it is drawn and labelled, but the eye could not see it from here. Always `false` without a lift |
| `dominance` | **metres** the summit stands above the terrain around it, **signed** |

This is what makes a summit worth a label: one standing clear of its
neighbours reads as a peak, one on a long level ridge does not, however tall
it is. Measured within 3 km of the summit, walking out each way until the
ground rises above it and taking the higher of the two lowest points — the
shape of topographic prominence, and close to it where positive: Slavkovský
štít measures 338 m against a true 370 m, Východná Vysoká 207 m against ~180 m.

**Not called prominence, because it is signed and prominence is not.**
Topographic prominence is non-negative by definition, so a field promising it
would invite comparison against published figures for tops that score below
zero. A top that never rises clear of its own ridge scores how far the ridge
stands over it: −37 m for a shoulder, −281 m for a bump inside a massif. In
ridge country most visible tops are like this — from one viewpoint above
Krompachy, 167 of 219. They are still real, still worth labelling when there
is room, and the sign is what lets you order them.

The two halves are one continuous scale in metres, not two quantities glued
together: flatten a top until its col rises to the summit and it passes
through zero; drop it a metre below the ground beside it and it reads −1. So
rank on the value, but do not treat it as a magnitude — `Math.abs` or a
`size ∝ dominance` rule will put the least significant tops on top.
`dominance: 0` means only that nothing at the peak's own depth was found to
compare it against.

One honest limit: where a valley runs alongside a peak rather than a ridge,
the figure reads the valley depth and comes out high. Terrain hidden behind
nearer ground no longer biases it — the measurement uses every elevation the
ray marcher sampled, not only the surfaces that ended up drawn.

**Visibility barely depends on render quality, but "barely" is not "not at
all".** It is asked of the ray marcher directly, against the horizon it already
tracks, rather than read back out of the finished image, which is what removed
the large swings. One viewpoint measured across `step` 0.2→0.05 and
`supersample_x` 1→9 returned 765–778 peaks agreeing on all but ~1%; another,
across `step` 0.2 / `ssx` 1 against `step` 0.05 / `ssx` 3, returned 366 and 363
visible peaks sharing only 349 — **8% disagreement**. Coarse rays simply miss
gaps that fine rays find. Do not treat the set as reproducible across tiers.

**`dominance` no longer moves with quality.** It used to: it was measured from
whatever rays the image needed, so `step: 0.2, ssx: 1` sampled a summit's
neighbourhood along 1800 bearings where `step: 0.05, ssx: 3` used 21 600, and
more bearings find more of the ground beside it. A client rendering a preview
and the real tier behind it got two different label sets for one viewpoint —
**14 of 40 labels changed** at one viewpoint — and watched names swap as the
second image landed.

It is now measured on its own angular grid, `peak_profile_step` degrees wide,
with exactly one ray per column however many crossed it. Same viewpoint, same
two tiers, capped at 40: **40 of 40 identical.** Across all visible peaks the
score now differs between those tiers by a median of **1.1 m** (was 10.1), 4.0 m
at the 90th percentile (was 50.6), and 7.5 m at worst (was 326).

Not exactly zero, because the ray nearest a column's centre is not at the same
bearing in both tiers — it is within half a ray spacing, which is metres of
ground. What is gone is the systematic part.

> **This changed the numbers.** A full-quality render now measures dominance on
> a 0.2° grid rather than its own 0.017° rays, and scores shift accordingly:
> median −4.8 m at one viewpoint, but reshaped rather than merely moved, and
> **52 → 70 peaks cleared the default `min_dominance: 30`**. If your label
> density was tuned against the old numbers, re-check it.

**Visibility is a separate matter and still moves.** It is decided from the two
rays bracketing each summit, which are 0.2° apart at preview quality and 0.017°
apart at full — so marginal peaks still appear and disappear. Measured above:
366 against 363 visible, sharing 349. It did not show in the capped set because
the disputed peaks rank low, but a client asking for hundreds of labels will
still see churn in the tail.

#### Progressive rendering

Both passes must agree about `peak_profile_step`, which they do by default. If
you set it, set it identically — and `meta.peak_profile_step` reports the value
actually used, so a client can assert rather than assume.

Asking for peaks on **one** pass only is still the strongest guarantee, and it
takes the peak work off the expensive render:

```jsonc
// preview — labels land immediately and never move again
{ "step": 0.2,  "supersample_x": 1, "peaks": true  }
// final — image only
{ "step": 0.05, "supersample_x": 3, "peaks": false }
```

Then rescale the positions, because `x` and `y` are in **output pixels** and
the two images are not the same size. Both are the same view, so it is one
factor:

```js
const k = stepOfPeaksPass / stepOfImagePass;   // 0.2 / 0.05 = 4
const px = peak.x * k, py = peak.y * k;
```

Which pass to take them from is a real trade-off. The preview gives labels at
once and drops the peak work from the expensive render; the final gives the
better-measured set, since coarse rays miss ground beside a summit and bias its
`dominance` upward. What you cannot have, until dominance stops being measured
from the render, is both — and asking twice guarantees the disagreement is
visible.

Making this exact needs dominance to stop being a render-time measurement: true
topographic prominence is a property of the summit, computable once from the
DEM at ingest and stored with the peak. That would be perfectly stable, free
per request, and is the intended direction.

Metres, not degrees, because degrees are not comparable across distance: a 2 km
hill subtends more than the whole High Tatra range and would outrank every
summit in it. The reverse also holds — ranking purely by metres puts a big
distant massif above a nearby hill that fills far more of the frame — which is
why the `max_peaks` cut weighs distance in, and why you should too when placing
labels. `dominance` is reported raw so you can.

`x` and `y` are fractional — place labels at sub-pixel positions.

Only `osm_id` is needed to identify a peak; everything else about it (names in
every language, wikipedia, etc.) should come from OSM, since this service's
copy carries only what its import kept.

## Depth

A distance for every pixel, so the client can answer "how far is that ridge?"
under the cursor.

Encoding, in order: distance → logarithmic 16-bit → quantised by `depth_step`
→ delta-coded along each row → gzip. `0` means sky, and **only** sky:
quantisation never floors real ground onto that sentinel. Ground closer than
`near_m` saturates at `near_m` rather than reading as sky, so from a normal eye
height the bottom of the frame is terrain at 10 m, not a hole.

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

## `POST /viewshed`

What can be seen from a point, as a map overlay: a square RGBA image in Web
Mercator, centred on the viewpoint, transparent where nothing is visible.

```jsonc
{ "lon": 20.888781, "lat": 48.878479, "radius": 30000, "scale": 20 }
```

| field | type | default | meaning |
|---|---|---|---|
| `lon`, `lat` | number | — | viewpoint |
| `radius` | number | `30000` | how far to look, ground metres (max 300 000) |
| `scale` | number | `20` | ground metres per pixel |
| `eye` | number | `1.7` | eye height above ground |
| `eye_search_radius` | number | `10` | as for panoramas |
| `target_height` | number | `0` | height of the thing looked *at* |
| `color` | string | `#ffd666` | overlay colour, `#rrggbb` |
| `gamma` | number | `1` | curve on the opacity, `alpha ** (1/gamma)` (0.1–10); above 1 lifts grazing ground |
| `alpha_floor` | number | `0` | least opacity visible ground may take (0–1); a stencil rather than a shading |
| `format` | string | `avif` | `avif` or `png` |
| `quality` | int | `95` | AVIF quality |

`radius` and `scale` together fix the image size — `2 × radius / scale` on a
side — and are **validated together**, because neither looks unreasonable
alone: a 100 km radius is fine, 5 m per pixel is fine, and asking for both is a
40000 × 40000 raster. The pair must come to no more than **96 M pixels**, which
for a square image means **9797 px a side**.

That is a total, not a side length, so it trades against `radius`:

| `radius` | finest `scale` | image |
|---|---|---|
| 10 km | 2.05 m | 9756² |
| 30 km (default) | **6.13 m** | 9788² |
| 50 km | 10.2 m | 9804² |
| 100 km | 20.4 m | 9804² |
| 300 km (max) | 61.3 m | 9788² |

**Set `scale` from the data, not from the cap.** The finest pyramid level is
6.27 m of ground in the LiDAR countries and 30 m everywhere else, and the
marcher steps by cell size — so anything finer is more pixels carrying no more
information, at quadratic cost. Around 6–10 m is the useful floor over LiDAR,
30 m for anything wide or outside the surveyed countries.

Measured at these settings, warm:

| request | wall | payload |
|---|---|---|
| 30 km @ 12.25 m | 9 s | 2.9 MB |
| 30 km @ 6.1 m | 16 s | **9.1 MB** |
| 100 km @ 20.5 m | 36 s | 6.4 MB |
| 300 km @ 61.3 m | 19 s | 1.9 MB |

**The payload is the real cost of a fine scale, not the compute.** A 30 km
viewshed at 6.1 m is 9 MB — three times the 300 km one, because near terrain
at full resolution is all edges and edges do not compress. Peak memory is about
0.6 GB for a full-size raster, and renders are serialised, so that is one
request's worth rather than everyone's.

The response is the same multipart shape as a panorama: `meta` plus `image`.
`meta.bounds` is `[west, south, east, north]` in degrees, which is what a
Leaflet `ImageOverlay` wants:

```js
const { bounds } = meta;
L.imageOverlay(URL.createObjectURL(image), [
  [bounds[1], bounds[0]],
  [bounds[3], bounds[2]],
]).addTo(map);
```

### What the opacity means

Not a flat stencil. Opacity is the **projected area** of each patch of ground —
the sine of the angle between the line of sight and the surface — so a slope
facing you is solid and one seen edge-on fades out. That is what you actually
see of it: a hillside at five degrees of grazing shows under a tenth of its
area.

One consequence worth expecting: ground close to the viewpoint on a convex
summit reads faint, because you are looking along it rather than at it, even
though it is unmistakably visible.

#### Making it read stronger

The faintness is in the pixels, not in the compositing, so client-side opacity
cannot recover it: most of a large viewshed is distant, gently sloping ground
seen near edge-on, and lands at 0.05–0.15. **`gamma` reshapes it**, applying
`alpha ** (1/gamma)`:

| `gamma` | a 0.09 slope reads | a 0.5 slope reads |
|---|---|---|
| 1 (default, measured) | 0.09 | 0.50 |
| 2 | **0.30** | 0.71 |
| 3 | 0.45 | 0.79 |

A curve rather than a gain, deliberately. Any multiplier large enough to
rescue the far field drives the near field to solid and throws away the
projected-area gradation, which is the part worth keeping — a gamma lifts the
faint end while leaving the ordering intact and both endpoints fixed, so
nothing saturates and nothing vanishes however hard it is pushed. Measured on
a 25 km viewshed, mean alpha roughly doubles at `gamma: 2` and rises 2.5× at
`gamma: 3`, with the shading structure still legible at both.

`alpha_floor` is the blunter instrument: visible ground never falls below it,
whatever the geometry says. `alpha_floor: 1` is a plain stencil — visible or
not, nothing in between. **Neither knob can make hidden ground appear.** The
render writes only where something is visible, so transparency still means
exactly "not visible from here".

`gamma: 2` is a good starting point for an overlay meant to be read at a
glance; leave it at 1 when the opacity is being interpreted quantitatively.

### Cost

Rays are derived from the rim — two per pixel of circumference — and each is
drawn as a line rather than plotted where it lands, since the marcher steps by
DEM cell and that can exceed a pixel. One ray per pixel and dots instead of
lines left the far field stippled: 31% of the outer annulus covered against 96%
near the middle.

A 30 km radius at 20 m is 3000 × 3000 px and 18 850 rays; a 40 km radius at
25 m is 3200 × 3200 and 20 107 rays, about 3 s warm. Cold, expect several times
that while the DEM blocks come off disk. It shares the render queue with
panoramas, one at a time.

### Put the viewpoint on the summit, not near it

A viewshed is far more sensitive to the exact viewpoint than a panorama is, and
the difference is not subtle. From Gerlachovský štít's nominal coordinates the
DEM reads 2575 m, while the true summit is 2653 m about 75 metres away — so the
eye sits below the ridge beside it, which blocks that whole side:

| viewpoint | eye | coverage |
|---|---|---|
| nominal coordinates | 2587 m | **10%**, nothing at all to the north or east |
| DEM maximum, 75 m away | 2654 m | **37%**, all quadrants |

`eye_search_radius` helps but only samples a ring of eight points at that exact
radius, not the disc inside it, so widening it can miss a summit it steps over.
If the viewpoint comes from a peak database rather than from the user's finger
on the map, snap it to the local DEM maximum first.

### Two limits worth telling users

**It is bare earth.** No trees, no buildings. In forest or town it will say you
can see more than you can, sometimes much more.

**It is only as good as the DEM** — 1 m LiDAR in the surveyed countries, 30 m
GEDTM30 elsewhere. The [coverage](#where-it-runs) applies here as it does to
panoramas.

## `GET /progress/{token}` — live progress

A render takes tens of seconds and the response arrives all at once at the end,
so progress comes over a side channel. Invent a token, send it with the
request, and subscribe to it separately. Nothing about the request or the
response changes; progress is opt-in.

```js
const token = crypto.randomUUID();

const events = new EventSource(`${BASE}/progress/${token}`);
events.onmessage = (e) => {
  const { phase, ahead, percent, final } = JSON.parse(e.data);
  // queued  -> "waiting, N ahead"
  // rendering / encoding -> percent
  if (final) events.close();
};

const res = await fetch(`${BASE}/panorama`, {
  method: "POST",
  headers: { "Content-Type": "application/json", "X-Job": token },
  body: JSON.stringify({ lon, lat }),
});
```

| field | meaning |
|---|---|
| `phase` | `queued`, `rendering`, `encoding`, `done`, or `unknown` |
| `ahead` | renders that must finish before yours starts; 0 means next |
| `percent` | 0–100 through the current render |
| `final` | last event on this stream — close on it |

Both `/panorama` and `/viewshed` accept `X-Job`.

**Subscribe first, then post.** The stream tolerates either order — an unknown
token reports `phase: "unknown"` and keeps waiting for about ten seconds rather
than 404ing — but subscribing first means you never miss the queued phase.

**`percent` is a percentage, not a clock.** It counts output columns for a
panorama and rays for a viewshed, and those are not equal work: a column of sky
costs less than one of near terrain, so the rate drifts a few percent. Deriving
an ETA is left to the client, which knows when it started and can smooth as it
likes.

**Close on `final`, not on a phase.** A stream can end for reasons that are not
"the render finished": the request may have been rejected before it ever
registered — a 400, or a full queue — or nothing may ever arrive under that
token, in which case the stream gives up after about ten seconds with
`phase: "unknown"`. Every one of those endings carries `final: true`. A browser
reconnects an `EventSource` automatically, so a client watching only for
`done` would reopen for ever in those cases.

**One token, one request.** A token already in use is refused progress rather
than taking over the entry, so reusing one across a retry or across a panorama
and a viewshed gives the second request no progress rather than corrupting the
first's. Use a fresh token each time.

It needs a second connection alongside the one the render is using. That is
fine in a browser — six per origin — but progress will not work behind anything
that serialises requests per client.

## Coordinates

- **Azimuth** — degrees clockwise from north. `az` is the *left edge* of the
  frame, not its centre.
- **Altitude** — degrees above horizontal from the eye, so the horizon is near
  0 and slightly negative from a summit.
- **Image** — origin top-left. `x = ((azimuth - az_start) mod 360) / step`,
  `y = (alt_max - altitude) / step`.

> **`y` needs a term for `depth_lift`.** With a lift the row is
> `y = (alt_max - (altitude + depth_lift * distance / range)) / step`, because
> terrain is raised in proportion to its distance. The formula above is the
> `depth_lift: 0` case. Using it under a lift puts a label 60 px off for a
> summit at `range` with `depth_lift: 3` and `step: 0.05` — and inverting a
> hovered row back to an altitude needs the distance from the depth buffer
> before the term can be removed. Peaks already come back with `y` computed
> correctly; this matters for overlays a client positions itself.

For a 360° render the image wraps: column `width - 1` is adjacent to column 0,
so it can be panned continuously or tiled as a cylinder.

## Errors

| status | when |
|---|---|
| `400` | `alt_max` not greater than `alt_min`; pixel limit exceeded |
| `500` | render failed — most often the viewpoint has no elevation data |
| `503` | shutting down |

Most out-of-range numbers are clamped rather than rejected — `range`, `fov`,
`step`, the supersampling factors, `eye_search_radius`, `dither_strength`. The
exceptions are the ones where silently rewriting the request would hide a real
mistake, and they answer `400` naming the field: `alt_min` or `alt_max` outside
−90–90, `depth_lift` outside 0–45, `peak_rank_power` outside 0–4,
`peak_profile_step` outside 0.02–2, `ridge_width` outside 0–20, a negative
`ridge_strength`, a malformed colour,
and a non-finite value in any numeric field — though the JSON parser refuses
`NaN` and `Infinity` before the check ever sees them, so that one is belt and
braces rather than something you can trigger.

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

**`depth_lift` breaks the "you can see this" promise.** With a lift the render
is a drawing, not a photograph: labelled peaks marked `revealed` are hidden
from the actual viewpoint, and the depth buffer describes where terrain was
*drawn*, so a pixel's distance no longer implies a clear line of sight to it.
Anything doing "what can I see from here" should ask
[`/viewshed`](#post-viewshed) or render without a lift.

**A 360° image is 7200 px wide** at the default step, which can exceed texture
and canvas limits on older mobile GPUs. Either request `step: 0.1` (3600 px) or
split the image client-side.

## Not implemented yet

- **Sun path** — the data is there in the distance buffer, the projection is not
  written.
- **Caching or precomputation** — every request renders from scratch, which is
  why latency is seconds. Plan the UI around an explicit action with progress
  rather than something firing on map movement.
