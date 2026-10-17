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
| Phase 2 GPU compositing for export | **in progress** on branch `worktree-agent-a88eed9bd193a763f` (worktree `.claude/worktrees/agent-a88eed9bd193a763f`): 1 commit + uncommitted work; P2s works on the GPU, P3s blocked on wasm memory (see "Phase 2 in progress") |
| Phase 4 off the main thread | not started; design notes below |
| Phase 5 simd128 in `h264enc` | done, on `main` (`5a5e4e79`..`64348228`): software encoder 1.9x faster on wasm (node), 1.75x native, bytes unchanged; Chrome export + P1s sha check not yet run |

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

## Phase 2 in progress (state at handoff, 2026-10-16 ~17:00)

Work is on branch `worktree-agent-a88eed9bd193a763f` (git worktree
`.claude/worktrees/agent-a88eed9bd193a763f`, based on `19b98cef`; `main` has since moved on with
Phase 5 and an xtask fix, so rebase before merging). The agent stopped at a usage limit.

- **Committed** `e78fa927` "export: asynchronous frame renderers (Ready / Pending / Declined),
  partial batches, prefetch" (crates/export).
- **Uncommitted in the worktree** (check `git -C <worktree> status`): `crates/gpu/src/async_export.rs`
  (async read-back ring on the existing device), `apps/filmcraft-web/src/gpu_export.rs` (frame-renderer
  factory on eframe's WebGPU device), edits to crates/gpu (lib.rs, export_renderer.rs, Cargo.toml),
  crates/export/src/lib.rs, crates/engine/src/export_tools.rs, apps/filmcraft-web (api.rs, lib.rs,
  Cargo.toml, tests/export-bench.mjs), docs/web.md, Cargo.lock.
- **Measured (single indicative run, not a benchmark):** P2s on the GPU 10.7 s (phase 1: 133.8 s,
  baseline 168.2 s), 360/360 frames on the GPU, Y-PSNR 46.38 dB (min 43.18), SSIM 0.985.
- **Bug found and fixed in the work:** on WebGPU the compositor's effect stage was always disabled
  (an error-scope future polled once, `Pending` treated as failure), so Lumetri / blur / etc. ran
  on the CPU even in previews.
- **Blocker: P3s (4K)** traps (`RuntimeError: unreachable`, allocation failure) in the `mt` build at
  ~1.95–2.14 GB of wasm heap: `mt` is linked with `--max-memory=2147483648`; the phase-1 CPU path
  already peaked at 1.93 GB (1 GiB decode `FRAME_BUDGET` + 4K f32 intermediates). Approved
  fixes (coordinator decision): **(b) first** — cap the decoded-frame cache (and other playback-sized
  caches, e.g. WebCodecs' 40-frame per-source cache) while a web export runs, restore it when the
  export ends however it ends, report wasm peaks before/after; **(a)** raise the `mt` link max to
  4 GiB (`--max-memory=4294967296`) in xtask, confirming the `mt` build still instantiates and a
  failed threaded start still falls back to `st` (index.html), noted in docs/web.md. The agent was
  also trimming its own path (upload cache 24 → 8, banded f16 conversion instead of a 66 MB copy,
  30 s read-back timeout).
- **Harness bug to fix:** `export-bench.mjs` hangs forever when the page traps mid-export (ignores
  `--timeout-min`): wrap each `page.js`/`send` in the poll loop with a timeout and fail the run on
  `Inspector.targetCrashed` or a `RuntimeError`/`unreachable` console line.
- **Still to do for Phase 2:** the above, then gates (per-crate tests, clippy incl. wasm, fmt, smoke),
  commit, then the coordinator runs the timed matrix (`--compositor gpu` vs `cpu`, P1s/P2s/P3s) on a
  quiet machine, merges to `main`, pushes, and logs the iteration in `docs/benchmarks/web-export.md`.
  `export-bench --compositor gpu|cpu` sends `gpuRendering: auto|off` in that work.

### Working rules that held up
- Agents work in git worktrees and commit small steps on their branch; the coordinator merges to
  `main` (fast-forward / rebase) after a full gate run and pushes.
- Disk is tight (bench machine ~17 GB free after deleting iOS simulators; was 2.2 GB): share
  `CARGO_TARGET_DIR=/Users/yishengj/filmcraft/target`, `CARGO_INCREMENTAL=0`, `--release`
  everywhere; a worktree at another path rebuilds every workspace crate, so agents run per-crate
  gates and the coordinator runs the full `cargo xtask ci` once after merging.
- Only one timed benchmark at a time, never while anything compiles; agents do single
  correctness runs only.

## Next steps (in order)

1. `bench-compare.mjs` pairs results by exact config name, so baseline (`software-cpu-auto`) and
   new (`hardware-cpu-opfs`) configs do not line up: add a `--map` / project-level comparison.
2. Quick loop on P1s/P2s with `--encoder hardware` vs `software`; validate (PSNR gate ≥ 38 dB vs
   the CLI reference is the colour-matrix check for the WebCodecs path); log the iteration.
3. Phase 3 measurement: `--sink opfs` vs `memory`, wasm peak; then investigate the decode caches
   (`memoryDetail`): the 1 GiB `FRAME_BUDGET` and WebCodecs' 40-frame per-source cache are sized
   for playback, not for a one-pass export; P4 (60 min) cannot finish without streaming (5.4 GB
   output > 4 GiB wasm).
4. Phase 2: finish per "Phase 2 in progress" above. Original design note: `ExportRenderer::render` reads back synchronously, which
   cannot work on the browser's main thread. Needed: an asynchronous render path in the stepped
   exporter (submit frame N, `mapAsync`, collect on a later step, same `Step::Pending` pattern as
   the encoder), using eframe's WebGPU device; then the zero-copy variant (render the output
   picture into a WebGPU `OffscreenCanvas` → `new VideoFrame(canvas)` → hardware encoder), which
   needs the post-render CPU steps (placement, overlays, limiter) on the GPU. Transitions
   (`FramePlan::Composite`) still go to the CPU.
5. Phase 4: in the `mt` build the FrameServer already renders previews on worker threads;
   offloading export rendering there (main thread polls, never blocks: `Atomics.wait` is not
   allowed on it) parallelizes the CPU path too. WebCodecs decode sessions are UI-thread-only.
6. Phase 5 follow-ups: measure the software path in Chrome (`--encoder software`, P1s must keep
   sha `97aad066…`; encode stage was 17.9 s); next hot spots on wasm are 16x16 SATD in the sub-pel
   search, CABAC `renorm` (bit-at-a-time; a carry-propagating byte writer would give the same bits),
   `mc_luma` and the half-sample interpolation. `Preset::Speed` was *not* made the wasm default (it
   changes the output); offer it as an explicit opt-in if wanted.

## Phase 5 (software H.264 fallback, `crates/h264enc`)

`wide` SIMD kernels (SATD, SAD, SSD on wasm, 4x4/8x8 transforms + quant + dequant, residual
gather/reconstruction) plus output-identical algorithmic savings; details and per-kernel numbers in
[`crates/h264enc/README.md`](../crates/h264enc/README.md#speed). Single-threaded 1080p P1s footage with
the export's settings: native 79.8 → 45.7 ms/frame, wasm `simd128` (node 22, `h264enc_bench` as
wasm32-wasip1) 109–117 → 58–61 ms/frame (1.9x; target ≥ 1.8x). Bit-exact: per-kernel tests vs the
scalar code, `golden_bytes` (also run as wasm), export determinism tests.
