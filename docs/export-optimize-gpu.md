# Agent task: GPU-first, hardware-encoded, streaming web export for FilmCraft

**Who you are and what you have.** You are a coding agent working in a local checkout of
FilmCraft (https://github.com/yishengjiang99/filmcraft). You have a shell, the Rust
toolchain, Node ≥ 22, ffmpeg/ffprobe, and **a running Chrome instance reachable over the
Chrome DevTools Protocol at `http://127.0.0.1:9222`** (headed, on real GPU hardware). You
drive Chrome only through CDP (raw WebSocket; no npm packages, matching the repo's existing
`apps/filmcraft-web/tests/smoke.mjs`). You work in a closed loop: change code, build, run in
Chrome, measure, validate, and keep or revert.

**Goal.** Make export in the browser build fast, GPU-accelerated, memory-safe for long
timelines, and smooth while the user keeps editing. Use current web platform technology,
always behind feature detection, and keep the existing pure-Rust path as a fallback that
always works.

**Rules that outrank this prompt.** Read `AGENTS.md` and `CLAUDE.md` first. Never crash:
no `unwrap`/`expect`/`panic!` outside tests, and everything returns `Result`. No `unsafe`
outside `crates/platform`. Clean-room only: no GPL code, and don't read third-party encoder
source. ffmpeg is allowed **only** as a test oracle and fixture generator. Use permissive
dependencies only. Everything is a command and agent-drivable. `cargo xtask ci` passes before
every commit.

---

## 0. Setup (do once, verify, then record in `target/web-bench/env.json`)

1. **Check Chrome.** `curl -s http://127.0.0.1:9222/json/version` gives the browser version
   and the WebSocket URL. If nothing is listening, **launch a headed Chrome yourself** with
   its own profile, in the background. Chrome ≥ 136 refuses remote debugging on the default
   profile, and a separate profile can run alongside the user's normal Chrome:
   ```sh
   # macOS (Linux: google-chrome; Windows: chrome.exe)
   "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
     --remote-debugging-port=9222 --user-data-dir="$HOME/.chrome-filmcraft-bench" \
     --no-first-run --no-default-browser-check --window-size=1600,1000 about:blank &
   ```
   Poll `/json/version` for up to 15 s. A headed instance on the real GPU is a valid
   benchmark browser. Record its pid and leave it running between iterations. Stop and
   report only if it still won't start. **Never** use headless Chrome for benchmarks,
   because headless GPU numbers are not representative. Headless is fine for
   **correctness-only** runs, as `smoke.mjs` already does. Keep the bench window visible
   and don't minimize it, so the tab isn't throttled.
2. **Check the GPU.** Open a new target (`Target.createTarget`) at `chrome://gpu` and record:
   whether WebGPU and video encode are hardware-accelerated, the GPU name, and the driver.
   Use `SystemInfo.getInfo` over CDP as well. Then, in an app page, record
   `navigator.gpu.requestAdapter()` features (especially `timestamp-query`, `shader-f16`) and
   limits.
3. **Check codecs.** In the app page, run a `VideoEncoder.isConfigSupported` probe for each
   candidate (H.264 High/Main/Baseline at 1080p and 4K; HEVC Main/Main 10; AV1; VP9), with
   `hardwareAcceleration` set to `"prefer-hardware"` and then `"prefer-software"`, plus
   `AudioEncoder.isConfigSupported` for AAC (`mp4a.40.2`) and Opus. Save the matrix. Every
   later decision about which paths to test comes from this file.
4. **Build and serve.** `cargo xtask web --serve 8765` builds `target/web/dist` and serves
   it on `http://127.0.0.1:8765/` with COOP/COEP, so `crossOriginIsolated === true`. Use
   `cargo xtask web` (release, wasm-opt) for benchmarks, and `--dev` only for quick
   correctness iterations. Never benchmark a `--dev` build. Also run
   `cargo build --release -p filmcraft-cli`: the reference renders in §2.2 use it, and so do
   the repo's `filmcraft` MCP servers.
5. **Create the fixtures.** Write `apps/filmcraft-web/tests/fixtures.mjs`, which generates
   **deterministic** test media with ffmpeg (fixture generation is allowed) into
   `target/web/dist/bench/` so the page can fetch them. Don't commit binaries. Generate:
   - `f1080p30_h264.mp4`: 60 s, 1920×1080, 30 fps, `testsrc2` + a moving gradient,
     plus a 1 kHz/440 Hz stereo tone (AAC 48 kHz).
   - `f4k30_hevc.mp4`: 20 s 3840×2160 HEVC (decode stress).
   - `f720p60_vp9.webm`: 30 s 1280×720 60 fps VP9 + Opus.
   - `f1080p_10bit_pq.mov`: 10 s 10-bit HDR source (only where an encoder is available).
6. **Define the bench projects.** Build each project through the `window.filmcraft` API
   (`filmcraft.importUrl`, `filmcraft.execute(...)`), never by clicking, so every run is
   identical:
   - **P1 "simple"**: one 1080p30 clip, 20 s, no effects.
   - **P2 "typical"**: 1080p30, 60 s; 2 video tracks, 3 cuts, one cross-dissolve, Lumetri
     basic, one title, music + dialogue audio with a fade.
   - **P3 "heavy"**: 4K30, 20 s; blur, LUT, vignette, a picture-in-picture with a scale
     keyframe, 3 titles.
   - **P4 "long"**: P1 looped to **60 min** (memory and streaming test; run on demand and
     at the end, not every iteration).

---

## 1. Background (verified by reading the code)

- Encoders are pluggable. `crates/export/src/lib.rs` defines `trait VideoEncoder`
  (`sample_entry`, `timescale`, `encode`, `flush`, `media_start`) and `AudioEncoder`,
  registered via `register_encoder` / `register_audio_encoder` / `register_format_probe` /
  `register_hdr_probe`. VideoToolbox (`crates/platform/src/hardware_encode.rs`) and NVENC
  (`crates/platform/src/nvenc/`) are working templates.
- The web build only **decodes** with WebCodecs (`apps/filmcraft-web/src/webcodecs.rs`), and
  it converts every decoded `VideoFrame` to RGBA through a 2D `OffscreenCanvas` (GPU→CPU→GPU
  per frame). All web exports use the software `crates/h264enc` (I/P/B, CABAC, 2-pass), which
  is single-threaded on wasm and has no hand-written `simd128` kernels.
- The web build **composites exports on the CPU**. Desktop and CLI call
  `register_frame_renderer` (GPU renderer in `crates/gpu/src/export_renderer.rs`);
  `apps/filmcraft-web` does not.
- Export runs **on the UI thread**. `engine/src/export_tools.rs::spawn_export` makes a
  stepped `Exporter` with `set_batch(1)`, and `apps/filmcraft-web/src/lib.rs` pumps it 30 ms
  per egui frame. Non-stepped formats (GIF, image sequences, audio-only) run synchronously and
  freeze the tab.
- The whole output is **buffered in wasm memory and copied about 3 times**
  (`Out::Mem` → `write_output` → `fs.rs::write_file` `Arc::from` → Blob). `crates/isobmff`
  already has `FragmentedWriter` (fMP4) and `Mp4Writer::finish_faststart`.
- Image sequences download one file every 125 ms (`download_pacing.rs`).
- `index.html` picks a threaded `mt/` build when `crossOriginIsolated`.
  `docs/web-optimization-plan.md` is the existing roadmap: align with it and update it.

---

## 2. Build the harness first (iteration 0; no product changes yet)

### 2.1 `apps/filmcraft-web/tests/export-bench.mjs`
Reuse the CDP helpers from `smoke.mjs` (factor them into `tests/cdp.mjs`). Add
`--attach http://127.0.0.1:9222` to connect to the existing Chrome instead of spawning one.
CLI:

```
node apps/filmcraft-web/tests/export-bench.mjs --attach http://127.0.0.1:9222 \
  --url http://127.0.0.1:8765/ --project P2 --codec h264 --res 1080p \
  --encoder auto|hardware|software --compositor auto|gpu|cpu --sink auto|opfs|blob \
  --build st|mt --runs 5 --warmup 1 --label "<what changed>" --out target/web-bench
```

For **each run**:
1. Open a fresh target with `Target.createTarget`, using
   `?empty&fresh&norecover` plus the flags under test. Close it afterwards. Never reuse a
   page across runs: caches and GPU state leak between them.
2. Before starting, clear OPFS (`navigator.storage.getDirectory()` → remove entries) and
   set `Browser.setDownloadBehavior({behavior: "allowAndName", downloadPath, eventsEnabled:
   true})`.
3. Keep the tab active and unthrottled: `Target.activateTarget`,
   `Emulation.setFocusEmulationEnabled({enabled: true})`, and
   `Page.setWebLifecycleState({state: "active"})`. Background-tab timer throttling will
   otherwise distort results. Assert `document.visibilityState === "visible"`.
4. Build the project through the API, wait for `filmcraft.execute("jobs.list")` to show no
   pending import/indexing jobs, then start
   `filmcraft.execute("file.exportMedia", {..., encoder, sink})` and poll the job to
   completion (timeout 3× the configuration's baseline wall time, 10 min where there is none
   yet, 90 min for P4; a stall watchdog and first-error abort fail it sooner, §4.1).
5. During export, collect:
   - `wallMs` and `exportFps` (frames / wall seconds), plus `realtimeFactor`
     (media duration / wall time).
   - `timeToFirstPacketMs`.
   - **UI responsiveness**: `PerformanceObserver` for `long-animation-frame` and `longtask`
     (count, max, total blocking time). Meanwhile, the harness scripts a **concurrent
     interaction** every 500 ms (CDP `Input.dispatchMouseEvent` scrubbing the timeline
     ruler) and measures egui frame times via `filmcraft.request('perf.stats')` (p50, p95,
     max).
   - **Memory**: the wasm heap (`filmcraft.info().memory.wasmBytes`; add it), sampled every
     250 ms for peak and growth. Also `performance.measureUserAgentSpecificMemory()` (works
     because the page is cross-origin isolated), and CDP `Performance.getMetrics`
     (JSHeapUsedSize).
   - **GPU timing** when the adapter has `timestamp-query`: per-frame compositor and
     RGB→YUV pass durations, reported by the app through `filmcraft.info().export.gpu`.
     If it's not available, record `null`; never guess.
   - **Encoder internals**: backend (`webcodecs-hw` / `webcodecs-sw` / `h264enc`), maximum
     `encodeQueueSize`, frames dropped or re-encoded, any `error` callbacks.
   - **Leaks**: open `VideoFrame`s / GPU buffers before and after (app counters in
     `filmcraft.info()`), plus CDP `Memory.getDOMCounters`.
   - **Console**: any Rust panic, `unhandledrejection`, or WebGPU validation error fails
     the run.
6. Get the output file: from the download directory for `blob`/`picker` sinks, or via
   `filmcraft.readFile(path)` / OPFS read for `opfs`. Then run validation (2.2).
7. Optionally, with `--trace`, record a CDP `Tracing` session (categories `gpu`, `v8`,
   `devtools.timeline`, `disabled-by-default-devtools.timeline`, `media`) to
   `target/web-bench/traces/<run>.json` for diagnosis. Never trace during measured runs,
   because tracing perturbs timing.

**Statistics.** Discard warmup runs. Report the median and the median absolute deviation
(MAD) of each metric. If the coefficient of variation of `wallMs` is above 5%, rerun (up to
3 more times) and flag the result as noisy. Append one JSON line per configuration to
`target/web-bench/results.jsonl`, with: git sha + dirty flag, label, Chrome version, GPU,
OS, build (st/mt), all flags, the project, and all metrics.

### 2.2 `apps/filmcraft-web/tests/validate-export.mjs` (correctness oracle)
For every exported file:
- `ffprobe -v error -show_streams -show_format -of json`: check codec, profile, pix_fmt,
  width/height, frame rate, colour primaries/transfer/matrix/range, duration within ±1
  frame, frame count exact, and the audio stream (codec, rate, channels).
- `ffmpeg -v error -xerror -i out -f null -`: zero decode errors.
- **A/V sync**: the fixtures carry a beep on a white flash every 5 s. Detect both in the
  output (`ffmpeg` `astats`/`silencedetect` and `blackdetect`/`signalstats`) and require
  |offset| ≤ 1 audio frame (~21 ms). Every audio encoder path must signal its priming in the
  edit list (`edts`/`elst`): AAC's 1024-sample encoder delay alone is 21.3 ms at 48 kHz. (Today's
  web exports encode audio with our own AAC encoder, which does; measured 0.02 ms in every run.
  A WebCodecs `AudioEncoder` path must do the same; the tolerance does not change for it.)
- **Picture quality**: compare against a **reference render** of the same project, made
  once per project with the current software path (`filmcraft-cli` export to lossless or
  PNG frames) and cached in `target/web-bench/ref/`. Compute PSNR/SSIM with ffmpeg
  (`-lavfi psnr`/`ssim`). Thresholds: hardware/GPU paths at equal bitrate ≥ 38 dB luma
  PSNR, SSIM ≥ 0.97, and **no single frame below 30 dB** (catches colour-matrix, range or
  frame-order bugs). Save the worst frame side-by-side as a PNG when a check fails. Measured
  on Chrome / M4 Pro hardware H.264 at the pinned bitrates: 47–51 dB, so 38 dB has margin; if
  another encoder lands within ~3 dB of it, calibrate that encoder's threshold from its
  iteration-0 run (and say so) rather than chase it, keeping the 30 dB per-frame minimum as
  the bug-catcher. Order the checks cheapest first (metadata, decode, A/V sync, PSNR); the quick
  loop computes PSNR on sampled frames, keep candidates on every frame.
- **Determinism**: the software encoder + CPU compositor path must produce a
  **byte-identical** file across runs and versus the pre-change baseline (sha256).
- Our own decoder must also decode the file (round trip through `filmcraft.importUrl`).

### 2.3 `apps/filmcraft-web/tests/bench-compare.mjs`
Compare the latest results against the baseline (`--baseline <label|sha>`). Print a table
for each configuration: wall time, fps, UI p95, long-animation-frame total, peak memory,
PSNR, each with deltas. Exit non-zero on a **regression** (see §4.2). Write the table into
`docs/benchmarks/web-export.md` under a dated heading.

### 2.4 Baseline
Run the full matrix (§3) on the **unmodified** code and record it with label `baseline`.
That table is the reference for every claim you make later. Commit the harness alone,
with the baseline numbers, as the first PR.

---

## 3. Bench matrix

| Dimension | Values (skip combinations the env.json probe says are unsupported) |
|---|---|
| Project | P1, P2, P3 (P4 on demand + final) |
| Codec | H.264, HEVC, AV1, VP9 (WebM) |
| Encoder | software (`h264enc`), WebCodecs prefer-hardware, WebCodecs prefer-software |
| Compositor | CPU, GPU (WebGPU) |
| Sink | blob, OPFS (sync handle in worker), stream/picker (test hook, see below) |
| Build | `st` (default), `mt` (threaded, cross-origin isolated) |
| Thread | main-thread pump, export worker |

- **Quick loop** (each iteration, about 3–5 min): P2 × H.264 × {software, hardware} × the
  current compositor/sink under development, `--runs 3`.
- **Full matrix**: at the end of every phase.
- **Pickers can't be automated.** Add a test-only `?testsink=opfs-stream` flag that
  exercises the same streaming writer code path as `showSaveFilePicker`, minus the dialog.
  Verify the real picker once by hand (or with CDP `Browser.grantPermissions` where
  applicable) and note it.

---

## 4. The loop

### 4.1 Each iteration: a ladder, cheapest check first
Every rung must pass before the next one runs; most bugs should be caught on rungs 1–2.
```
1. Pick ONE change from the plan (§5). State the hypothesis + expected metric movement.
2. Implement it behind a flag/feature detection; keep the fallback path untouched.
3. Native tests (seconds): cargo clippy / cargo test for the touched crates. Logic that does
   not need a browser is tested natively: WGSL kernels against their CPU versions on wgpu
   (Metal/Vulkan), the chunk queue, reserved `moov`, fMP4 fragments, cache policies. Stop here
   on any failure.
4. Build without wasm-opt for rungs 5–6 (`cargo xtask web --threads --no-opt`: release, no
   wasm-opt); restart the --serve process if needed.
5. Smoke export (~30 s): a 60-frame slice of P2 (`--project P2x`), then metadata → `-xerror`
   decode → A/V sync (cheapest first). Catches muxing, colour and leak bugs before any long run.
   The fallbacks (`?softenc&cpu`, `?webgl`) get this smoke every iteration, their full run only
   at phase end.
6. Correctness: export-bench --runs 1 --warmup 0 --validate on the target project(s), PSNR on
   sampled frames (every 10th + the flash frames). Fix before going on.
7. Performance (only if 3–6 pass): A/B interleaved bench, baseline and candidate builds served
   on two ports and run alternately (thermal / power drift then hits both). Stop early: a first
   measured run worse than the baseline beyond the noise band rejects; a clearly better one is
   confirmed with 2 more.
8. node bench-compare.mjs --baseline <previous accepted iteration>
9. Decide:
     - all gates (§4.2) pass AND target metric improved beyond noise → KEEP: rebuild with
       wasm-opt, confirm the result once, full PSNR validation, `cargo xtask ci`, commit with
       numbers in the message.
     - gates pass, no measurable gain → keep only if it simplifies code or unlocks the next step; say so.
     - any gate fails → fix, or revert (git checkout) and record why in docs/benchmarks/web-export.md.
10. Log the iteration: label, hypothesis, result, decision (one row).
```

**Abort early, don't wait for timeouts.** The harness fails a run on the first
`Runtime.exceptionThrown`, console error, WebGPU validation error, wasm trap or renderer crash,
and when the export's frame count does not move for `--stall-s` seconds (default 15; 60 for
configurations whose baseline frame takes over 3 s, e.g. 4K on the CPU compositor). The project
timeout is 3× the configuration's baseline wall time, not a flat 10 minutes. A stalled run stores
the exporter and decoder state (`runs.json` `stall`, `lastSamples`).

Limits: at most **3 attempts** on one hypothesis before moving on and noting it as blocked.
Never change a test threshold, fixture or reference to make a run pass. If a threshold looks
wrong, write down why and ask. When results are noisy (CV > 5% after reruns), don't claim a
win; increase `--runs` or investigate the noise source (thermal, other tabs, power mode).

### 4.2 Gates (must hold after every kept iteration)
- `cargo xtask ci` passes before each commit (not each attempt). There are no new
  `unwrap`/`expect`/`unsafe`.
- `smoke.mjs` passes (headless correctness).
- Every exported file passes §2.2. The software path is byte-identical to baseline (at the
  baseline preset and thread count; see Phase 5 for the one sanctioned exception).
- Zero console panics, uncaught rejections, or WebGPU validation errors. Zero leaked
  `VideoFrame`s or GPU buffers after export.
- No regression beyond noise on any **non-target** metric: more than +max(5%, 2×MAD) wall
  time, more than +max(10%, 2×MAD) UI p95, or more than +10% peak memory (a flat +5% sits
  inside the 5% CV noise band and fails at random).
- Fallbacks still work: the `?softenc&cpu` and `?webgl` smoke export (§4.1 step 5) passes every
  iteration; their full quick loop runs at phase end.

---

## 5. Implementation plan (one PR per phase; iterate inside each with §4)

**Phase 1: WebCodecs hardware encoder.** Add `apps/filmcraft-web/src/webcodecs_encode.rs`
implementing `VideoEncoder`, plus an AAC/Opus `AudioEncoder`. Register them only for codecs
that pass `isConfigSupported`. Use `hardwareAcceleration: "prefer-hardware"`,
`latencyMode: "quality"`, and `avc/hevc {format: "avc"|"hevc"}`. Map rate control:
CBR/VBR → `bitrateMode` + `bitrate`; constant quality → `bitrateMode: "quantizer"` with
per-frame quantizer where supported. Keyframes via `{keyFrame: true}` every N frames.
Set colour space on frames, and HDR only where the probe confirms it. Build
`sample_entry()` from the first output's `decoderConfig.description`. **Async bridging:**
use JSPI (suspending imports) where available, else the stepped model (submit, drain
outputs on the next step, bound `encodeQueueSize` ≤ ~8 using the `dequeue` event). On an
encoder `error`, surface an actionable `ExportError` and offer "Retry with software
encoder". Add a Settings toggle and the `encoder` parameter on `file.exportMedia`.
*Target: P2 1080p H.264 wall time ≤ 25% of baseline (≥ 4× faster) where hardware encode is
available.*

**Phase 2: GPU compositing and zero-copy frames.** Register the GPU frame renderer in the
web app on eframe's wgpu device (keep the CPU path on WebGL2). Add a WGSL compute pass for
RGB(A)/RGBf → NV12 / P010 that matches `rgba_to_yuv420_8` / `rgbf_to_yuv420_8` /
`rgbf_to_yuv420_10` within ±1 code value (unit test). Render the final picture to a
WebGPU `OffscreenCanvas` and use `new VideoFrame(canvas, {timestamp, duration})` so frames
reach the hardware encoder without CPU readback. For the software path, use a 2–3 deep
`mapAsync` readback ring. On decode, replace the 2D-canvas conversion with
`importExternalTexture` / `copyExternalImageToTexture`. Add `timestamp-query` timing to
`filmcraft.info().export.gpu`. **Device-agnostic renderer:** the export renderer must take
the `wgpu::Device`/`Queue` it is given and must not reach for eframe's device or any
UI-owned resource. A `GPUDevice` cannot be transferred to a worker, so Phase 4b will create
a second device in the export worker; nothing in this phase may make that a rewrite. Add a
unit/integration test that builds the renderer on a freshly created device with no eframe
context. (State on 2026-10-17: `filmcraft_gpu::AsyncExportRenderer::new(device, queue, budget)`
already takes any device and `async_readback_matches_sync` builds it on a fresh one; what is
still tied to eframe is the web glue, `apps/filmcraft-web/src/gpu_export.rs`, which stores
`eframe::wgpu` handles and is installed from `start()`. Make its `install` take a plain
`wgpu::Device`/`Queue` so a worker can call it.) **Measure, don't assume, zero-copy:** record per-frame time from canvas present to
`VideoEncoder.encode()` return, and compare against the `mapAsync` readback path; Chrome
does not document `new VideoFrame(webgpuCanvas)` as copy-free. *Target: P3 4K wall time
≥ 2.5× faster than Phase 1 with the CPU compositor; PSNR gates hold.* (Phase 1 never measured
P3 on the CPU compositor: it ran out of the 2 GiB wasm heap. Measure that configuration now
that it fits, or compare with the baseline; P3s baseline 1118 s.)

**Order after Phase 2.** Phases 3–5 run in this order, each its own PR:
**3a** (in-memory copies out, chunked fMP4) → **4a** (cooperative pump) → **4b** (export
worker, which brings the OPFS sync sink) → **3b** (save picker) → **5** (only if its
entry gate passes). 3a and 4a don't depend on Phase 2 and may start in parallel with it in
a separate worktree.

**Phase 3a: Chunked output, no whole-file buffer.** Already in place (`bd13e6c8`): web
exports default to `Out::Stream` (`filmcraft_export::StreamTarget`, positioned 4 MiB chunks)
into an OPFS `createWritable` or JS-side chunks (`apps/filmcraft-web/src/export_sink.rs`,
`web/export-sink.js`); `sink: "memory"` keeps the old `Out::Mem` path. Missing, and the core
of this phase: **backpressure.** `export-sink.js` accepts every write synchronously and
appends it to an unbounded promise chain, so when OPFS writes fall behind the bytes pile up
in JS memory. Put a bounded queue of chunks (default 8 MiB each) between the muxer's
synchronous `Write` and the sink, drained by the pump between steps; when it is full, the
exporter answers `Step::Pending` until JS has drained it. Never block on a promise. Then
remove `sink: "memory"`, and route the formats that still finish through `write_output` →
`fs.rs::write_file` (`Arc::from` copies: image-sequence frames, the other unstreamed
formats) through the stream sink where they are single files. Container choice, by sink:
- **Unseekable sink** (Blob of chunks, stream): `FragmentedWriter`, ~1–2 s fragments.
- **Seekable sink** (OPFS, save picker): progressive MP4. For faststart, **reserve** the
  `moov` box at the start of the file. Export knows the exact frame and sample counts up
  front, so the worst-case `moov` size is computable. Write the `moov` box into that space
  at the end, padded with a `free` box. Never use `finish_faststart`'s whole-file rewrite
  on web.
Interleave audio and video into each fragment/chunk (≤ 1 s apart) so the muxer never holds
one track's packets waiting for the other. The Blob fallback still holds the whole file in
**browser** memory: say so in the UI for exports estimated over 2 GB, and offer the save
picker where it exists. Check `navigator.storage.estimate()` against the estimated output
size before starting, and call `persist()` for OPFS. Image-sequence ZIP /
`showDirectoryPicker` is out of scope here; track it as a separate task.
*Target: P2 peak tab memory (not just wasm heap) ≤ 50% of baseline; P4 completes with the
Blob sink, wasm heap growth ≤ 512 MB.*

**Phase 4a: Cooperative pump (before the worker).** Replace the fixed 30 ms per egui frame
pump with an adaptive one: step until `scheduler.yield()` is due (target ≤ 8 ms of export
work per animation frame while the user is interacting, larger batches when the tab is
idle or hidden), with a `setTimeout(0)` fallback where `scheduler` is missing. Make every
export format stepped: today `filmcraft_export::stepped` covers H.264, HEVC, AV1, ProRes,
DNxHR, APV, MJPEG and MXF; GIF, the image sequences (PNG, TIFF, BMP, JPEG, TGA, DPX, EXR)
and audio-only (WAV, AIFF, AAC, FLAC) must stop calling a synchronous `run()` on wasm. This is a small change and is measured on its own, so the worker's gain
is visible separately. *Target: no export-caused long animation frame over 100 ms; UI p95
improvement recorded vs Phase 3a.*

**Phase 4b: Export worker.** Run export in a dedicated worker with its own wasm instance,
its own `wgpu` device (created from the Phase 2 device-agnostic renderer), and its own
WebCodecs decoders. Those decoders must keep the UI path's rules: start an open-GOP leading
picture (HEVC RASL after a CRA) one GOP earlier, and keep recently asked frames cached
(`webcodecs.rs`, `filmcraft_media::cache::evict_far`); without them the 4K P3s export
re-decoded one frame forever. Pass a serialized project snapshot (the UI state is already serde) and
`File` handles. Progress, cancel and errors go over `postMessage`. Inside the worker, add
the OPFS `createSyncAccessHandle()` sink (it only works in dedicated workers), and
download the result as the OPFS `File` with no copy. Things to budget and measure:
- **Duplicate memory.** The worker re-opens media and rebuilds decode/index caches. Report
  the worker's wasm heap separately from the UI's, and include both in peak tab memory.
- **GPU contention.** The worker's compositor and encoder share the GPU with the UI.
  Report UI frame GPU time during export; if p95 misses because of the GPU rather than the
  main thread, record that rather than chasing it with CPU changes.
- **Async stays async.** WebCodecs output callbacks run on the worker's own event loop, so
  `Atomics.wait` on them deadlocks. Keep the Phase 1 bridging (JSPI where available, else
  stepped).
On cross-origin-isolated pages, the software fallback may use the `mt/` build's rayon
threads from inside the worker. Dropped from this phase: wasm memory64 (not needed once
output streams, and slower), Wake Lock, Web Locks, `Notification` and `setAppBadge`. List
them under "later" in `docs/web-optimization-plan.md`. *Target: UI p95 ≤ 20 ms and zero
export-caused long animation frames over 50 ms during the concurrent-scrub test; export
throughput ≥ the Phase 4a pump.*

**Phase 3b: Save picker sink.** Add `showSaveFilePicker` →
`FileSystemWritableFileStream` (Chromium only) as the first-choice sink, using
`write({type: "seek"})` for the reserved-`moov` layout from 3a. Writes are async: drive
them from the same bounded chunk queue. A cancelled picker is a clean cancel. Sinks are
tried in this order: save picker, OPFS (worker), Blob.

**Phase 5: Faster software fallback (gated).** *Entry gate:* from `env.json` and the full
matrix, list the browser/OS combinations where no WebCodecs H.264 encoder passes
`isConfigSupported` (hardware **or** `prefer-software`). Also bench WebCodecs
`prefer-software` against `h264enc` on P2. If WebCodecs software encode is available and
faster on every combination tested, record that, and do only the smallest useful part of
this phase (or none). Otherwise:
- Profile `h264enc` on wasm first, and work on the top hotspots in profile order.
- On the `mt/` build, add slice threads before SIMD if the profile supports it.
- `simd128` kernels (SAD/SATD, sub-pel interpolation, transforms + quant, deblock) must be
  **bit-exact** with the scalar code, verified by tests on random blocks.
**Determinism rule (replaces "byte-identical to baseline" for this phase):** with the same
preset and thread count, the SIMD build's output is byte-identical to the scalar build's.
Changing the default wasm preset (for example to `Preset::Speed`) or the slice count is a
separate, explicit decision. It changes the output, so it needs a new reference hash and
PSNR/SSIM checks against the old one, and it must be logged as such.
*Target: P2 software-path wall time ≥ 1.8× faster than baseline at the same preset.*

**Applies to every phase:** feature-detect every API, check current support on MDN /
caniuse / Chrome Platform Status before relying on it, and record a support matrix in
`docs/web.md`. Release `VideoFrame`s (`close()`) and GPU resources (`destroy()`)
deterministically. Convert JS errors into `ExportError`s, and treat a cancelled picker as a
clean cancel. Put new `web-sys` features in `apps/filmcraft-web/Cargo.toml`, use no npm
dependencies, and keep JS glue small in `apps/filmcraft-web/web/`. `file.exportMedia` gains
`encoder` and `sink` parameters, and `filmcraft.info().export` reports the backend,
compositor, sink, thread, fps, bytes and GPU timings.

---

## 6. Done means

All of the following hold in the final full-matrix run on the attached Chrome, compared
against the iteration-0 baseline:

| # | Criterion | Measured by |
|---|---|---|
| 1 | P2 1080p30 H.264 export ≥ **4×** faster (hardware path) | export-bench wallMs median |
| 2 | P3 4K export ≥ **6×** faster end to end (GPU + hardware) | export-bench |
| 3 | UI p95 ≤ **20 ms**; no long animation frame over 50 ms from export during concurrent scrub | perf.stats + LoAF observer |
| 4 | P4 60-min export completes with the OPFS/picker sinks; wasm heap growth (UI + worker) ≤ **512 MB**; peak tab memory reported for every sink, including Blob; no leaks | memory samples, `measureUserAgentSpecificMemory`, leak counters |
| 5 | All outputs valid: ffprobe, `-xerror`, A/V sync ≤ 21 ms, PSNR ≥ 38 dB (min frame ≥ 30 dB), SSIM ≥ 0.97 | validate-export.mjs |
| 6 | Software path byte-identical to baseline at the baseline preset and thread count; SIMD bit-exact with scalar; any preset/slice change logged with a new reference hash. If Phase 5 ran: software P2 ≥ 1.8× faster at the same preset. If its entry gate skipped it: the gate's evidence is recorded | sha256, unit tests, bench |
| 7 | Fallbacks pass: `?softenc&cpu`, `?webgl`, `?nothreads`, and a correctness run in Firefox and Safari if available locally | smoke + validate |
| 8 | `cargo xtask ci` green; docs updated (`docs/web.md` support matrix, `docs/web-optimization-plan.md`, `docs/performance.md`, `docs/benchmarks/web-export.md` with every iteration logged) | CI, review |

When a target is impossible on this machine (for example, no hardware HEVC encoder), say so
with the env.json evidence, meet it on the paths that exist, and don't fake numbers.

## 7. Reporting

At the end of each phase, post: the compare table against baseline and the previous phase,
the iteration log (kept / reverted and why), open issues, and the next hypothesis. Every
performance claim cites a `results.jsonl` line (label + sha).