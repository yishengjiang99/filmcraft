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
   completion (timeout 10 min; 90 min for P4).
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
  |offset| ≤ 1 audio frame (~21 ms).
- **Picture quality**: compare against a **reference render** of the same project, made
  once per project with the current software path (`filmcraft-cli` export to lossless or
  PNG frames) and cached in `target/web-bench/ref/`. Compute PSNR/SSIM with ffmpeg
  (`-lavfi psnr`/`ssim`). Thresholds: hardware/GPU paths at equal bitrate ≥ 38 dB luma
  PSNR, SSIM ≥ 0.97, and **no single frame below 30 dB** (catches colour-matrix, range or
  frame-order bugs). Save the worst frame side-by-side as a PNG when a check fails.
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

### 4.1 Each iteration
```
1. Pick ONE change from the plan (§5). State the hypothesis + expected metric movement.
2. Implement it behind a flag/feature detection; keep the fallback path untouched.
3. cargo clippy / cargo test for touched crates (fast gate).
4. cargo xtask web  (release build)  — restart the --serve process if needed.
5. Correctness: node export-bench.mjs --runs 1 --warmup 0 → validate-export.mjs. Fix before going on.
6. Performance: quick-loop bench (--runs 3, warmup 1).
7. node bench-compare.mjs --baseline <previous accepted iteration>
8. Decide:
     - all gates (§4.2) pass AND target metric improved beyond noise → KEEP: commit with numbers in the message.
     - gates pass, no measurable gain → keep only if it simplifies code or unlocks the next step; say so.
     - any gate fails → fix, or revert (git checkout) and record why in docs/benchmarks/web-export.md.
9. Log the iteration: label, hypothesis, result, decision (one row).
```

Limits: at most **3 attempts** on one hypothesis before moving on and noting it as blocked.
Never change a test threshold, fixture or reference to make a run pass. If a threshold looks
wrong, write down why and ask. When results are noisy (CV > 5% after reruns), don't claim a
win; increase `--runs` or investigate the noise source (thermal, other tabs, power mode).

### 4.2 Gates (must hold after every kept iteration)
- `cargo xtask ci` passes. There are no new `unwrap`/`expect`/`unsafe`.
- `smoke.mjs` passes (headless correctness).
- Every exported file passes §2.2. The software path is byte-identical to baseline.
- Zero console panics, uncaught rejections, or WebGPU validation errors. Zero leaked
  `VideoFrame`s or GPU buffers after export.
- No regression beyond noise on any **non-target** metric: more than +5% wall time, more
  than +10% UI p95, or more than +10% peak memory.
- Fallbacks still work: rerun the quick loop with `?softenc&cpu` and with `?webgl`. Export
  must still succeed.

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
`filmcraft.info().export.gpu`. *Target: P3 4K wall time ≥ 2.5× faster than Phase 1 with
the CPU compositor; PSNR gates hold.*

**Phase 3: Streaming output.** Use `FragmentedWriter` (fMP4, ~1–2 s fragments) for web MP4,
and progressive + faststart when the sink is seekable. Add a chunked streaming
`OutputSink`, with sinks tried in this order:
1. `showSaveFilePicker` → `FileSystemWritableFileStream`.
2. OPFS `createSyncAccessHandle()` in a worker, downloaded as the OPFS `File` (no copy).
3. `new Blob(chunks)`.

Check `navigator.storage.estimate()` and `persist()` first. Remove the `Entry::Mem` copy
for exports. Image sequences go to `showDirectoryPicker` or a streamed ZIP (store, or
`CompressionStream("deflate-raw")`). *Target: P4 (60 min 1080p) completes with wasm heap
growth ≤ 512 MB; P2 peak memory ≤ 50% of baseline.*

**Phase 4: Off the main thread.** Run export in a dedicated worker with its own wasm
instance. WebCodecs and WebGPU (`navigator.gpu`, `OffscreenCanvas`) are available in
workers. Pass a project snapshot plus `File` handles; send progress, cancel and errors
over `postMessage`. On cross-origin-isolated pages, also enable the `mt/` build's rayon
slice threads for the software fallback, and try wasm memory64 behind detection. Until the
worker lands: an adaptive pump with `scheduler.yield()` /
`scheduler.postTask({priority: "background"})`, and bigger batches when hidden or idle.
Make every export format stepped or worker-run, with no synchronous `run()` on wasm. Add an
optional screen Wake Lock, a Web Lock per output file, and `Notification` /
`setAppBadge` on completion. *Target: UI p95 ≤ 20 ms and zero long animation frames over
50 ms caused by export during the concurrent-scrub test; export throughput ≥ the
main-thread-pump throughput.*

**Phase 5: Faster software fallback.** Add `core::arch::wasm32` simd128 kernels in
`crates/h264enc` (SAD/SATD, sub-pel interpolation, 4x4/8x8 transforms + quant, deblock).
They must be **bit-exact** with the scalar code, verified by tests that compare both
implementations on random blocks. Use `Preset::Speed` for single-threaded wasm.
*Target: P2 software-path wall time ≥ 1.8× faster than baseline, output byte-identical
to scalar.*

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
| 4 | P4 60-min export completes; wasm heap growth ≤ **512 MB**; no leaks | memory samples, leak counters |
| 5 | All outputs valid: ffprobe, `-xerror`, A/V sync ≤ 21 ms, PSNR ≥ 38 dB (min frame ≥ 30 dB), SSIM ≥ 0.97 | validate-export.mjs |
| 6 | Software path byte-identical to baseline; SIMD bit-exact; software P2 ≥ 1.8× faster | sha256, unit tests, bench |
| 7 | Fallbacks pass: `?softenc&cpu`, `?webgl`, `?nothreads`, and a correctness run in Firefox and Safari if available locally | smoke + validate |
| 8 | `cargo xtask ci` green; docs updated (`docs/web.md` support matrix, `docs/web-optimization-plan.md`, `docs/performance.md`, `docs/benchmarks/web-export.md` with every iteration logged) | CI, review |

When a target is impossible on this machine (for example, no hardware HEVC encoder), say so
with the env.json evidence, meet it on the paths that exist, and don't fake numbers.

## 7. Reporting

At the end of each phase, post: the compare table against baseline and the previous phase,
the iteration log (kept / reverted and why), open issues, and the next hypothesis. Every
performance claim cites a `results.jsonl` line (label + sha).