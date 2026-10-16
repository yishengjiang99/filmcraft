# Handoff: GPU-first, hardware-encoded, streaming web export

Status of the work on [`export-optimize-gpu.md`](export-optimize-gpu.md) (the plan) as of
2026-10-16. Numbers: [`benchmarks/web-export.md`](benchmarks/web-export.md); every number there
cites a `target/web-bench/results.jsonl` line (label + git sha) on the bench machine.

## Where it stands

| Plan step | State |
|---|---|
| §0 setup (env probe, fixtures, projects) | done, committed |
| §2 harness (`export-bench`, `validate-export`, `bench-compare`, `bench-matrix`, `make-ref`) | done, committed |
| §2.4 baseline | short projects (P1s/P2s/P3s) + full P1 measured; full P2/P3/P4 not run (see "Baseline") |
| Phase 1 WebCodecs hardware encoder | done, committed (`bd13e6c8`); measured on P1s/P2s (below) |
| Phase 3 streaming output (OPFS / Blob) | done, committed (`bd13e6c8`); OPFS + Blob sinks validated; memory effect not yet separated from Phase 1 (below) |
| Phase 2 GPU compositing for export | designed, not started (below) |
| Phase 4 off the main thread | not started; design notes below |
| Phase 5 simd128 in `h264enc` | not started |

Also done on the way: `cargo xtask ci` fast test tier (`--full` for everything), OPFS writes in
the threaded build fixed (recovery snapshots were never saved), rustfmt drift on `main`.

## Baseline (pre-Phase-1 export path: built-in H.264 encoder, CPU compositor, wasm buffer)

Chrome 155.0.8059.40, Apple M4 Pro, threaded (`mt`) build unless noted, ruler scrubbed every
500 ms during export, 12 Mb/s (1080p) / 40 Mb/s (4K) VBR.

| config | runs | wall s | fps | realtime | UI p95 ms | LoAF blocking s | wasm peak MiB | render / encode / convert s | Y-PSNR dB (min) | sha |
|---|---|---|---|---|---|---|---|---|---|---|
| P1-h264-software-cpu-auto-mt | 3+1w | 81.1 ± 0.3 | 7.40 | 0.250 | 325 | 53 | 686 | 15.2 / 63.0 / 5.3 | 47.31 (43.7) | 7a02034c |
| P1s-h264-software-cpu-auto-mt | 3+1w | 23.6 ± 0.0 | 7.63 | 0.250 | 309 | 15 | 619 | 4.6 / 17.9 / 1.5 | 47.48 (43.7) | 7a02034c |
| P2s-h264-software-cpu-auto-mt | 3+1w | 168.2 ± 0.2 | 2.14 | 0.070 | 667 | 148 | 1400 | 130.1 / 36.6 / 3.1 | 46.45 (41.64) | 7a02034c |
| P2s-h264-software-cpu-auto-st | 2 | 168.8 ± 0.3 | 2.13 | 0.070 | 675 | 149 | 1332 | 130.2 / 36.7 / 3.1 | 46.45 (41.64) | 788dae8c |
| P3s-h264-software-cpu-auto-mt | 1 | 1118.0 ± 0.0 | 0.16 | 0.010 | 5292 | 1100 | 1933 | 1041.2 / 73.8 / 6.3 | 54.79 (50.53) | 788dae8c |

## Phase 1 + 3 (label `phase1`, `bd13e6c8`: WebCodecs hardware encoder, OPFS sink, CPU compositor)

| config | runs | wall s | fps | UI p95 ms | wasm peak MiB | render / encode / convert s | Y-PSNR dB (min) | vs baseline |
|---|---|---|---|---|---|---|---|---|
| P1s-h264-hardware-cpu-opfs-mt | 3+1w | 7.1 ± 0.0 | 25.19 | 42 | 533 | 4.5 / 1.6 / 1.5 | 48.09 (43.77) | 3.3× faster, UI p95 309 → 42 ms |
| P2s-h264-hardware-cpu-opfs-mt | 2 | 133.8 ± 0.0 | 2.69 | 658 | 981 | 128.4 / 3.3 / 3.1 | 47.19 (43.54) | 1.26× faster, wasm peak −30 % |

The encoder is no longer the cost; CPU compositing is (P2s: 96 % render). `?softenc` falls back to
the built-in encoder and is byte-identical to the baseline (sha `97aad066…` for P1s).

Where the time goes (P2s, per frame): CPU render 361 ms (77 %), H.264 encode 102 ms (22 %),
RGB→YUV 9 ms. So the hardware encoder alone gives ~4× on P1-like edits but only ~1.3× on P2;
the 4× P2 target needs fast rendering (Phase 2 GPU, or parallel CPU render in workers).
Wasm heap peaks at 0.6–1.4 GiB, mostly decode/frame caches (the encoded output is small);
`results.jsonl` now records the cache breakdown (`memoryDetail`) to confirm.

All baseline outputs pass validation: A/V sync 0.02 ms, Y-PSNR vs the CLI reference 46–47 dB
(min frame ≥ 41.6), SSIM ≥ 0.98, frame counts exact, `-xerror` clean.

## Phase 1 / Phase 3 code (what it does)

- `crates/export`: `VideoEncoder` gained `is_hardware`, `ready`, `poll`, `pending` (asynchronous
  encoders; defaults keep native encoders unchanged); the stepped `Exporter` yields
  (`Step::Pending`) while an encoder is not ready and after `flush` until it delivered everything;
  `ExportSettings::require_hardware` (+ `make_venc` check), `stream_sink`; the web default of
  `hardware_encoding` is `Auto` (native stays `Off`). `Out::Stream` + `StreamTarget` /
  `register_stream_target`: positioned 4 MiB chunks instead of a wasm buffer. Tests:
  `async_encoder_tests.rs` (a lagging fake encoder must give the byte-identical file; errors;
  `require_hardware`), `stream_out_tests.rs` (streamed == buffered bytes; seek/patch; hostile seeks).
- `crates/engine`: `file.exportMedia` `encoder: auto|hardware|software`, `sink: auto|opfs|blob|memory`.
- `apps/filmcraft-web/src/webcodecs_encode.rs`: WebCodecs `VideoEncoder` (prefer-hardware,
  `latencyMode: quality`, avc/hevc format), RGBA→I420 with the built-in BT.709 conversion,
  `VideoFrame` tagged BT.709 limited, sample entry from `decoderConfig.description`, queue ≤ 8,
  errors → `ExportError` naming `encoder: software`; start-up probe; `?softenc` disables.
  `export_sink.rs` + `web/export-sink.js`: OPFS (`createWritable`, positioned writes) or JS-side
  chunks; the finished OPFS `File` is registered in the virtual fs and downloaded without a copy
  through wasm. `filmcraft.info().export` reports backend, queue max, errors, sink, compositor, thread.

## How to run things

```sh
# Chrome for benchmarks: headed, own profile, CDP on 9222 (do not restart mid-series; note the version)
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --remote-debugging-port=9222 \
  --user-data-dir="$HOME/.chrome-filmcraft-bench" --no-first-run --no-default-browser-check &
cargo xtask web --threads --serve 8766 &          # 8765 was taken by an old server on the bench machine
node apps/filmcraft-web/tests/fixtures.mjs        # re-run after every build: xtask wipes dist/
node apps/filmcraft-web/tests/make-ref.mjs --projects P1s,P2s,P3s
export FFMPEG=/opt/homebrew/bin/ffmpeg FFPROBE=/opt/homebrew/bin/ffprobe
node apps/filmcraft-web/tests/bench-matrix.mjs --label <change> --url http://127.0.0.1:8766/ --matrix short
node apps/filmcraft-web/tests/bench-compare.mjs --baseline baseline --doc docs/benchmarks/web-export.md
FILMCRAFT_FFMPEG=/opt/homebrew/bin/ffmpeg FILMCRAFT_FFPROBE=/opt/homebrew/bin/ffprobe cargo xtask ci
```

## Known unrelated failure

`cargo test --workspace` fails one ffmpeg oracle test with Homebrew ffmpeg/ffprobe 8.x:
`filmcraft-matroska` `tests/oracle.rs` `mjpeg_ac3` ("packet 56 (track 1): duration ffprobe 32 vs 8",
AC-3 lace durations). The crate is untouched by this work; the fast CI tier does not run it unless
matroska changes or `Cargo.lock` changes (which forces the full suite).

## Gotchas (each cost time once)

- **Never compile while a benchmark runs**: it skews the timings (the harness reruns when the
  wall-time CV > 5 %, but a single long run is not protected).
- **Disk**: the bench machine had 3.5 GiB free; Chrome spools fetched media to disk, and imports
  failed with "Failed to fetch". `export-bench` now warns below 5 GiB. Full-length references are
  ~5 GB: generate them only for end-of-phase runs.
- **ffmpeg on PATH** (`~/.local/ffmpeg-libass`) lacks libx265/libvpx/libopus: fixture generation
  picks `/opt/homebrew/bin/ffmpeg`; oracle tests need `FILMCRAFT_FFMPEG`/`FILMCRAFT_FFPROBE`.
- **Fixture determinism**: ffmpeg's `gradients` source is not deterministic even with a seed; the
  fixtures use a `geq` gradient and single-threaded bitexact encodes (`manifest.json` has sha256s).
- **`pgrep -f bench-matrix.mjs` in a wait loop matches the loop's own shell**: wait on the
  process id, or on a line in the log, instead.
- **A killed harness leaves its tab exporting** (fixed: SIGINT/SIGTERM close it; check
  `/json/list` for stray `127.0.0.1:8766` tabs after crashes).
- **Nightly**: the `mt` build needs nightly + `wasm32-unknown-unknown` + `rust-src`; `wasm-opt`
  comes from Homebrew `binaryen` (xtask skips it silently when missing: 41 MB vs 32.5 MB wasm).
- `info().threads` is hard-coded `false` even in the `mt` build (cosmetic; `filmcraftLoad.threaded`
  is the truth).
- Element rects from `ui.elements` are in canvas backing pixels (2× CSS here).
- The baseline uses `--legacy` (no `encoder`/`sink` params, which did not exist yet); later runs of
  the software path pass `--encoder software --compositor cpu` and must stay byte-identical.

## Next steps (in order)

1. `bench-compare.mjs` pairs results by exact config name, so baseline (`software-cpu-auto`) and
   new (`hardware-cpu-opfs`) configs do not line up: add a `--map` / project-level comparison.
2. Quick loop on P1s/P2s with `--encoder hardware` vs `software`; validate (PSNR gate ≥ 38 dB vs
   the CLI reference is the colour-matrix check for the WebCodecs path); log the iteration.
3. Phase 3 measurement: `--sink opfs` vs `memory`, wasm peak; then investigate the decode caches
   (`memoryDetail`): the 1 GiB `FRAME_BUDGET` and WebCodecs' 40-frame per-source cache are sized
   for playback, not for a one-pass export; P4 (60 min) cannot finish without streaming (5.4 GB
   output > 4 GiB wasm).
4. Phase 2 (the big one for P2/P3): `ExportRenderer::render` reads back synchronously, which
   cannot work on the browser's main thread. Needed: an asynchronous render path in the stepped
   exporter (submit frame N, `mapAsync`, collect on a later step, same `Step::Pending` pattern as
   the encoder), using eframe's WebGPU device; then the zero-copy variant (render the output
   picture into a WebGPU `OffscreenCanvas` → `new VideoFrame(canvas)` → hardware encoder), which
   needs the post-render CPU steps (placement, overlays, limiter) on the GPU. Transitions
   (`FramePlan::Composite`) still go to the CPU.
5. Phase 4: in the `mt` build the FrameServer already renders previews on worker threads;
   offloading export rendering there (main thread polls, never blocks: `Atomics.wait` is not
   allowed on it) parallelizes the CPU path too. WebCodecs decode sessions are UI-thread-only.
6. Phase 5 (`simd128` kernels in `h264enc`, bit-exact vs scalar) for the software fallback.
