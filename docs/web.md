# FilmCraft on the web

`apps/filmcraft-web` runs the same engine and egui UI as the desktop app in the browser: the
workspace compiled to `wasm32-unknown-unknown`, started by eframe's web runner on WebGPU (WebGL2
fallback). It is a static site: one `.wasm`, its `wasm-bindgen` JavaScript glue, `index.html`, an
`AudioWorklet` script and an icon. Nothing is uploaded anywhere; media stay on the user's machine.

## Build and run

| Tool | Version | Install |
|---|---|---|
| Rust target | `wasm32-unknown-unknown` | `rustup target add wasm32-unknown-unknown` |
| wasm-bindgen CLI | exactly the `wasm-bindgen` crate version (0.2.129) | `cargo install wasm-bindgen-cli --version 0.2.129 --locked` |
| wasm-opt (optional) | any recent binaryen | `brew install binaryen`; used by `cargo xtask web` when found |

```sh
cargo xtask web                 # release build → target/web/dist (index.html, filmcraft_web.js, filmcraft_web_bg.wasm, …)
cargo xtask web --dev           # unoptimised build (faster to compile, slow to run)
cargo xtask web --serve 8765    # build, then serve dist on http://127.0.0.1:8765/
```

`--serve` is a tiny localhost-only static server that sends `Cross-Origin-Opener-Policy` /
`Cross-Origin-Embedder-Policy`, so the page is cross-origin isolated like a production deployment
should be. Any static server works (`python3 -m http.server -d target/web/dist 8765`); it must
serve `.wasm` as `application/wasm` for streaming compilation.

The web crate is empty on non-wasm targets, so `cargo test --workspace` and clippy are
unaffected; `cargo xtask wasm` (part of `cargo xtask ci`) checks it, with `filmcraft-ui-egui`, for
`wasm32-unknown-unknown`.

URL flags: `?empty` (no demo project), `?norecover` (don't reopen the auto-saved project),
`?fresh` (also don't restore media kept in OPFS), `?cpu` (CPU compositor), `?webgl` (WebGL2 only;
the page reloads with it when WebGPU is present but fails to start), `?nowebcodecs` (decode with
FilmCraft's own decoders only).

If the app panics after start-up (or runs out of memory), `index.html` shows a "FilmCraft stopped
working" overlay with the panic message and a Reload button instead of a frozen canvas, and sets
`window.filmcraftLoad.fatal`. Code the web build runs must not call `std::env::temp_dir`,
`std::time::{Instant, SystemTime}::now`, `std::thread::sleep`/`spawn` or `std::process::id`: they
panic on `wasm32-unknown-unknown` (use `filmcraft_engine::temp_dir`, `web_time`, or a `cfg`).

## How the desktop pieces map to the browser

| Desktop | Web |
|---|---|
| frame worker threads | `FrameServer::pump`: queued frame jobs run on the UI thread between egui frames, highest priority first, within a time budget (24 ms while playing, 40 ms otherwise) |
| export on a worker thread | `filmcraft_export::Exporter`: the export advances one frame per step from `Session::pump_jobs`, 30 ms per UI frame; progress shows in the header like on the desktop |
| `std::fs` (`FsServices`) | `fs::WebServices`: a virtual file table. Picked/dropped files are `File` handles (never copied); files the app writes are kept in memory and offered as downloads |
| file dialogs | File System Access `showOpenFilePicker` where available, else `<input type=file>`; drop files anywhere on the page |
| crash-recovery journal | OPFS: `recovery/snapshot.fcproj` every 5 s while there are unsaved changes, plus copies of imported media in `media/`; the next visit reopens the snapshot |
| cpal output | WebAudio `AudioWorklet` |
| VideoToolbox & co. | WebCodecs `VideoDecoder` |
| TCP control channel / MCP | `window.filmcraft` (below) |

### Threads

wasm threads need a cross-origin isolated page **and** a wasm build with atomics (a nightly
`build-std` toolchain), so the default build is single-threaded everywhere: frame rendering,
decoding, mixing and encoding are cooperative on the UI thread, and `rayon` runs inline.
`filmcraft.info()` reports `crossOriginIsolated` so a threaded build can choose at start-up.
Commands that need a thread on the desktop (render previews, proxies, Project Manager, mask
tracking) report an error on the web for now.

`cargo xtask web --threads` additionally builds that threaded variant (nightly +
`-Z build-std=std,panic_abort`, `+atomics`, `--cfg fc_wasm_threads`) into `dist/mt/`.
`index.html` loads it when the page is cross-origin isolated, and falls back to the
single-threaded build otherwise, on `?nothreads`, or if the threaded start throws. In the
threaded build the FrameServer runs its real worker pool (a failed spawn degrades back to
the UI-thread pump, so frames keep flowing); the rest of the app — mixing, export, media
reads — still runs cooperatively as above. Its shared memory may grow to 4 GiB (the wasm32
maximum; it was 2 GiB, which a 4K export outgrew): a browser that cannot reserve that much fails
the threaded start, and `index.html` falls back to the single-threaded build. See
[web-optimization-plan.md](web-optimization-plan.md) for the roadmap.

### Media reads

Containers are opened through `Services::reader` (a `filmcraft_media::ByteReader`): MP4/MOV and
Matroska read their index at import and single samples afterwards, so a multi-gigabyte file is
never loaded whole. Browser `Blob` reads are asynchronous, so `BlobReader` keeps a 384 MiB LRU cache
of 1 MiB chunks: a read whose chunks are missing starts fetching them (plus three chunks of
read-ahead) and fails with `WouldBlock`, setting `filmcraft_media::pending`. The frame server then
drops that frame's result and retries the job after the bytes arrive (the fetch wakes the UI);
the stepped exporter re-renders the frame; an import retries `file.import`. Before importing, the
container index is prefetched (an MP4's `moov` box wherever it is, a small Matroska file whole).
Stills, WAV and compressed-audio files are decoded whole, as on the desktop.

Limits: Matroska/WebM files larger than the chunk cache import slowly (opening scans every
cluster header); a file the user picked is only readable while the page is open, unless its OPFS
copy exists (files up to 4 GiB are copied in the background).

### Decoding

FilmCraft's own decoders (H.264, HEVC, VP9, AV1, ProRes, DNxHR, MJPEG, AAC, Opus…) run in the
browser unchanged. When the browser has WebCodecs, H.264, HEVC, VP9 and AV1 video in MP4/MOV is
decoded by the browser's (usually hardware) `VideoDecoder` instead: `webcodecs::reader_opener` is
registered ahead of the built-in openers and opens such files as a `WcSource`, whose audio and
media info come from our `Mp4Source`. WebCodecs is asynchronous, so a frame request feeds samples
to the decoder and returns "pending"; the decoder's output callback converts the `VideoFrame` to
RGBA (drawn on an `OffscreenCanvas`) into a per-source frame cache, and the retried request finds
it. Codec support is probed at start-up with `VideoDecoder.isConfigSupported`; any decoder error
switches that source to our decoder.

### Audio

`audio::Handle` implements `AudioOut`. The UI thread mixes the sequence 250 ms ahead in 2048-frame
blocks and posts them to the worklet (`web/audio-worklet.js`), which counts the frames it really
played and reports them with its `currentTime`. `played_frames` extrapolates from the last report
on the audio context's clock, never past what was queued — so the audio clock is the playback
master as on the desktop, and a starved worklet holds the playhead. Browsers start audio suspended
until the first click or key press; until then playback runs on the wall clock.

### GPU

eframe creates a WebGPU device when the browser has one, else WebGL2. FilmCraft's GPU compositor
(`filmcraft-gpu`) is used on WebGPU, for the monitors and for exports; on WebGL2 (and with `?cpu`)
frames are composited on the CPU. Check `filmcraft.info().backend` and `.compositor`.

WebGPU answers error scopes asynchronously (a promise), so the compositor cannot know at creation
whether its effect stage (Lumetri, blurs… as compute shaders) compiled: it uses the stage and drops
it if the answer, checked every frame, is an error. (It used to assume the worst, which sent every
layer effect of the web build to the CPU, in previews too.)

### Export

Exports run as stepped jobs on the UI thread (`Session::pump_jobs`, ~30 ms per call).

**Compositing.** On WebGPU the picture is composited on the GPU, on eframe's own device
(`gpu_export.rs`, `filmcraft_gpu::AsyncExportRenderer`): the main thread may not block on a
read-back, so a frame is submitted (composite into the linear float accumulator, copy into one of a
ring of staging buffers, `mapAsync`) and answered `Pending`; the exporter yields and a later step
collects it (half floats → f32, as the desktop's synchronous read-back does), then the CPU
post-processing (placement, overlays, limiter, RGBA8) runs as before. The exporter steps in batches
of half the frames the ring holds (512 MiB of staging buffers: 1080p 8 frames a step, 4K 3) and
prefetches the next batch before collecting the current one, so the GPU draws batch *k+1* while the
CPU converts and encodes batch *k*. Frames the GPU path declines render on the CPU: transitions
other than dissolves (`FramePlan::Composite`), adjustment layers, HDR / wide-gamut sequences, and
every frame after a GPU validation / out-of-memory error, a failed `mapAsync` (lost device) or a
read-back that has not finished after 30 s and 30 looks at it. `file.exportMedia` `gpuRendering: "off"` keeps an export on the CPU
(the default on the web is `auto`; natively `off`); `?cpu` / `?webgl` have no WebGPU export at all.
`filmcraft.info().export.compositor` says what drew the last export (`gpu`, `cpu`, or `gpu+cpu`
when some frames fell back), with `gpuFrames` / `cpuFrames`, `gpuEffects` (effect stage on the
GPU) and `gpuError` (why the GPU path stopped). `export.gpu` (timestamp-query pass timings) is
`null`: eframe's device is created without the `timestamp-query` feature.

**Memory.** The decode and frame caches are sized for playback (a 1 GiB shared decode budget
with at least 64 frames per source, 40 frames per WebCodecs source, 768 MB of monitor frames);
a one-pass export reads each frame once, in order, so while a stepped export job exists the
caches are lean (`filmcraft_media::lean`): 256 MiB of decoded frames shared (128 MiB or 12 frames
per source), 16 frames per WebCodecs source, 128 MiB of monitor frames. The job holds the guard,
so the caches go back to their playback sizes however the export ends. `perf.stats`
`decode.leanCaches` says whether they are lean now.

**Encoding.** H.264 and HEVC are encoded by the browser's WebCodecs `VideoEncoder`
(hardware where the browser has one: `webcodecs_encode.rs`) and otherwise by the built-in H.264
encoder. The choice is `file.exportMedia` `encoder`: `auto` (default on the web: WebCodecs when the
start-up `isConfigSupported` probe passed with `prefer-hardware`), `hardware` (fail instead of
falling back), `software` (the built-in, byte-reproducible encoder). `?softenc` skips the probe, so
everything uses the built-in encoder. The RGBA picture is converted to I420 in Rust with the same
BT.709 limited-range conversion as the software and VideoToolbox paths and handed over as a
`VideoFrame` tagged BT.709; the sample entry comes from the encoder's `decoderConfig.description`.
WebCodecs is asynchronous: the exporter yields (`Step::Pending`) while the encoder's queue is full
(> 8) or its codec record has not arrived, and after `flush` until the last chunk came back.
`filmcraft.info().export` reports the backend (`webcodecs-hw` / `h264enc`), queue high-water mark,
encoder errors, compositor (see above), sink and thread.

## `window.filmcraft`: the agent / test API

The control channel of the desktop app (`docs/control-protocol.md`) as promises. Results resolve
with the method's `result`, errors reject.

| Call | |
|---|---|
| `filmcraft.execute(command, params)` | run an engine command (same ids and params as MCP / `filmcraft-cli`) |
| `filmcraft.request(method, params)` | any control method: `ui.inspect`, `ui.click`, `ui.key`, `ui.playback`, `ui.menu.invoke`… |
| `filmcraft.commands()` | the command list |
| `filmcraft.inspect()` | `ui.inspect` |
| `filmcraft.screenshot(params)` | `{pngBase64, width, height}` of the canvas (or `{panel}`) |
| `filmcraft.importFiles(files)` | import `File` / `FileList` / `File[]`; resolves `{items, errors, paths}` |
| `filmcraft.importUrl(url, name?)` | fetch a URL and import it (tests) |
| `filmcraft.openProject(fileOrUrl)` | open a `.fcproj` |
| `filmcraft.files()` | the virtual files: `{path, size, kind}` |
| `filmcraft.readFile(path)` | a file's bytes (`Uint8Array`), e.g. an export |
| `filmcraft.info()` | backend, compositor, isolation, OPFS, WebCodecs support and counters, start-up time |

`window.filmcraftLoad` holds `{wasmMs, readyMs}` (module fetch+compile, and until the app runs).

```js
await filmcraft.importUrl("/clip.mp4");
await filmcraft.execute("file.newSequence", {fromItem: 48});
await filmcraft.request("ui.playback", {action: "play"});
const {job} = await filmcraft.execute("file.exportMedia", {format: "h264", path: "/exports/out.mp4"});
```

## Web platform support (export work)

Everything below is used behind feature detection; the pure-Rust path is the fallback that always
works. "Measured" = probed by `apps/filmcraft-web/tests/probe-env.mjs` on the bench machine
(Chrome 155.0.8059.40, Apple M4 Pro, macOS 27.0.1; `target/web-bench/env.json`); other columns
are from MDN / Chrome Platform Status and were not measured here.

| API | Use | Chrome 155 (measured) | Firefox | Safari |
|---|---|---|---|---|
| WebCodecs `VideoEncoder` H.264 (hardware) | export encoder | yes, High/Main/Baseline up to 4K; CBR, VBR, quantizer | yes (software/hardware varies) | yes |
| WebCodecs `VideoEncoder` HEVC Main (hardware) | HEVC export | yes, 1080p and 4K | no | yes |
| WebCodecs HEVC Main 10, AV1, VP9 hardware encode | – | no (AV1, VP9 software only) | software AV1/VP9 | varies |
| WebCodecs `AudioEncoder` AAC / Opus | – (the built-in AAC encoder costs < 1 % of an export) | AAC yes, Opus yes | Opus | AAC |
| WebCodecs `VideoDecoder` | import / playback decode | H.264, HEVC, VP9, AV1 hardware | yes | yes |
| WebGPU (+ `timestamp-query`, `shader-f16`) | compositor, GPU timing | yes, yes, yes | yes (recent) | yes (recent) |
| `new VideoFrame(OffscreenCanvas)` | zero-copy frames | yes | yes | yes |
| OPFS + `createSyncAccessHandle` (worker) | streaming sink | yes | yes | yes |
| `showSaveFilePicker` / `showDirectoryPicker` | streaming sink | yes | no | no |
| `CompressionStream("deflate-raw")` | ZIP of image sequences | yes | yes | yes |
| JSPI (`WebAssembly.Suspending`) | async bridging | yes | no | no |
| wasm memory64 | > 4 GiB heaps | yes | yes | no |
| SharedArrayBuffer (COOP/COEP) | threaded build | yes | yes | yes |
| `scheduler.yield` / `postTask` | adaptive pump | yes | partial | no |
| Long Animation Frames API | bench (jank) | yes | no | no |
| Wake Lock, Web Locks, `setAppBadge`, Notifications | long exports | yes | partial | partial |

Note: `FileSystemWritableFileStream.write` rejects views of shared memory (its IDL takes a
`BufferSource`), so writes from the threaded build copy first; `FileSystemSyncAccessHandle.write`
takes an `AllowSharedBufferSource`, so the worker OPFS sink can write straight from wasm memory.

## Export benchmark

`docs/export-optimize-gpu.md` is the plan; numbers live in `docs/benchmarks/web-export.md`. The
harness (no npm packages; ffmpeg only as fixture generator and oracle):

```sh
cargo xtask web --threads --serve 8765 &                     # release + wasm-opt, st + mt builds
node apps/filmcraft-web/tests/probe-env.mjs --url http://127.0.0.1:8765/   # env.json (GPU, codecs, APIs)
node apps/filmcraft-web/tests/fixtures.mjs                    # deterministic media → dist/bench/
node apps/filmcraft-web/tests/make-ref.mjs                    # CLI reference renders (PSNR/SSIM)
node apps/filmcraft-web/tests/bench-matrix.mjs --label <what changed> --url http://127.0.0.1:8765/
node apps/filmcraft-web/tests/bench-compare.mjs --baseline baseline --doc docs/benchmarks/web-export.md
```

`export-bench.mjs` attaches to a running, headed Chrome (`--attach http://127.0.0.1:9222`; headless
GPU numbers are not representative) and runs one configuration; `validate-export.mjs` checks an
output file (ffprobe layout and colour, frame count, `-xerror`, A/V sync from the fixtures' flash +
beep markers, PSNR/SSIM vs the reference, sha256).

## Browser test

`apps/filmcraft-web/tests/smoke.mjs` drives headless Chrome over the DevTools protocol (Node ≥ 22,
no npm packages): load, demo playback, import of a generated MP4, playback, H.264 export
(download). It writes screenshots and `report.json`:

```sh
cargo xtask web --serve 8765 &
ffmpeg -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine -t 3 -c:v libx264 -pix_fmt yuv420p -c:a aac target/web/dist/web-test.mp4
node apps/filmcraft-web/tests/smoke.mjs --url http://127.0.0.1:8765/ --media web-test.mp4 --out target/web/smoke
```
