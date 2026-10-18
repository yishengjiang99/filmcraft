# Handoff: GPU-first, hardware-encoded, streaming web export

Status of the work on [`export-optimize-gpu.md`](export-optimize-gpu.md) (the plan) as of
2026-10-17 (night). Numbers: [`benchmarks/web-export.md`](benchmarks/web-export.md); every number there
cites a `target/web-bench/results.jsonl` line (label + git sha) on the bench machine.

## Where it stands

| Plan step | State |
|---|---|
| §0 setup (env probe, fixtures, projects) | done, committed |
| §2 harness (`export-bench`, `validate-export`, `bench-compare`, `bench-matrix`, `make-ref`) | done, committed |
| §2.4 baseline | short projects (P1s/P2s/P3s) + full P1 measured; full P2/P3/P4 not run (see "Baseline") |
| Phase 1 WebCodecs hardware encoder | done, committed (`bd13e6c8`); measured on P1s/P2s (below) |
| Phase 3 streaming output (OPFS / Blob) | streaming done (`bd13e6c8`; OPFS + Blob sinks validated); in the revised plan this is part of **3a**, whose missing piece is backpressure (`export-sink.js` queues writes on an unbounded promise chain) and fMP4 for unseekable sinks |
| Phase 2 GPU compositing for export | **working and timed on `main`** (`2ffd57eb`..`32baa127`, labels `phase2b`, `i420`): with the GPU RGB→YUV pass P1s 2.54 s (9.3×), P2s 4.80 s (35×), P3s 10.09 s (111×) vs baseline, all frames on the GPU, all outputs valid; remaining items under "Phase 2 state" |
| Phase 4 off the main thread | not started; the revised plan splits it into **4a** (cooperative `scheduler.yield` pump) and **4b** (export worker) |
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
# one correctness run (fails within --stall-s 60 when an export stops moving)
node apps/filmcraft-web/tests/export-bench.mjs --url http://127.0.0.1:8766/ --project P3s --encoder hardware --compositor gpu --sink opfs --build mt --runs 1 --warmup 0 --label <change> --validate --ref target/web-bench/ref/P3s.mov
node apps/filmcraft-web/tests/bench-compare.mjs --baseline baseline --doc docs/benchmarks/web-export.md
FILMCRAFT_FFMPEG=/opt/homebrew/bin/ffmpeg FILMCRAFT_FFPROBE=/opt/homebrew/bin/ffprobe cargo xtask ci
```

## CI

Export CI's three failures were fixed in `f124b8c3`: the flaky
`async_encoder_tests::a_blocking_export_explains_an_asynchronous_encoder` (a process-wide
`FAIL_AFTER` static shared with the failing-encoder test; now per encoder), the Linux dev desktop
entry missing the `MimeType` line the packaged one gained in `69a6c4b6`, and
`graphics::windows_default_leaves_out_gl` assuming eframe's default backends while Export CI sets
`WGPU_BACKEND=vulkan`.
Then `10b04aee` moved `hardware_decoding_setting_drives_the_decoder_switch` to its own test binary
(`crates/engine/tests/`: it checks a process-wide switch that every `set_prefs` writes), and
Export CI's concurrency group is now per event, so a push no longer cancels a running full suite
(it had cancelled the dispatched run 39065995768).

## Known unrelated failures

- `filmcraft-ui-egui` `tests/graphics_notice_ui.rs` `the_notice_follows_the_interface_language`
  (expects Spanish "renderizado por software", gets "procesamiento por software") and
  `tests/timeline_marquee_ui.rs` `alt_box_selects_only_video_and_delete_keeps_linked_audio`
  ("release preserves the individual selection") fail on `2ffd57eb` without this work too.

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
- **The fast CI tier picks tests from unpushed changes**: after a push, `cargo xtask ci` runs only
  the never-crash suite ("no package changed"). Run the changed crates' tests (or `--full`)
  before pushing.
- **A hang costs a 30–40 min run**: `export-bench` now fails a run whose frame count does not move
  for `--stall-s` (60) seconds and stores `stall` (export + WebCodecs session state) and
  `lastSamples` in `runs.json`; page calls are bounded and a trap / renderer crash fails at once.
- **Open-GOP HEVC** (x265's and iPhone's CRA + RASL leading pictures): a decoder started at the
  CRA drops the leading pictures. The WebCodecs source must start one GOP earlier (it does now,
  as `filmcraft_codecs`' `decode_start`); P3s's 4K fixture has such GOPs (sample 116 is a CRA
  with pts of frame 120; frames 116–119 follow it).
- **Disk again**: the first `phase2b` matrix failed with "Failed to fetch" at 4.7 GiB free
  (Chrome spools media; one matrix used ~1.6 GiB). Check `df` before a matrix and print it
  before each config.
- **Do not attach a second CDP client to the benchmark tab**: a probe script's connection may
  have closed the harness's socket once (it exited with "unsettled top-level await"; sends now
  fail on a closed connection instead).
- The baseline uses `--legacy` (no `encoder`/`sink` params, which did not exist yet); later runs of
  the software path pass `--encoder software --compositor cpu` and must stay byte-identical.

## Phase 2 state (2026-10-17)

All on `main`, pushed. Commits: `b40586f3` asynchronous frame renderers in the stepped exporter;
`2ffd57eb` async read-back ring (`crates/gpu/src/async_export.rs`) + web renderer factory
(`apps/filmcraft-web/src/gpu_export.rs`); `16eec44e` lean caches during web exports
(`filmcraft_media::lean`: decode pool 256 MiB / 12 frames per source, WebCodecs 16 frames,
monitors 128 MiB, released when the job is dropped), `mt` heap max 4 GiB, harness trap handling;
`6fe735d5` Lumetri Look LUT on the GPU effect stage (own pass, storage-buffer 3D table, parity
3e-7); `34cab1fc` WebCodecs open-GOP start + recently asked frames pinned
(`filmcraft_media::cache::evict_far`); `36fdfadf` harness stall detection; `61362d26` WebCodecs
pins the decoder's run-ahead of the newest asks (`run_ahead_pins`: the CPU compositor had
regressed to 22.5 s on P1s, 3061 frames decoded for 180). Also `9fca3178` (wasm
clippy in `proxies.rs`, unrelated).

Timed (`phase2b`, 3 runs + warmup, Chrome 155.0.8059.40, `mt`, hardware encoder, OPFS; full
table in [`benchmarks/web-export.md`](benchmarks/web-export.md)):

| project | GPU compositor s | CPU compositor s | baseline s | vs baseline | wasm peak MiB GPU (baseline) | UI p95 ms GPU / CPU | Y-PSNR dB GPU (min) |
|---|---|---|---|---|---|---|---|
| P1s | 5.76 ± 0.02 | 7.14 ± 0.03 | 23.6 | 4.1× | 237 (619) | 116 / 42 | 48.05 (43.78) |
| P2s | 12.83 ± 0.01 | 134.49 ± 0.38 | 168.2 | 13.1× | 472 (1400) | 102 / 658 | 46.38 (43.18) |
| P3s | 24.51 ± 0.46 | — (browser exited at frame 165) | 1118.0 | 45.6× | 813 (1933) | 357 / — | 51.32 (48.65) |

Plan targets: P2 ≥ 4× and P3 ≥ 6× met (on the short cuts); UI p95 ≤ 20 ms not met.

Then `32baa127` (label `i420`) moved RGB→YUV onto the GPU: `crates/gpu/src/i420.wgsl` converts
the accumulator to BT.709 I420 (the CPU's sRGB table, round half away from zero; 0 bytes differ
from the CPU in `async_i420_matches_cpu_conversion`), the staging ring reads back 1.5 B/px
instead of 8, and encoders that say `accepts_i420` (WebCodecs) take it as it is. The exporter
turns it on only when nothing follows compositing (no resize/placement, overlay, limiter,
alpha, HDR); frames the GPU declines are converted on the CPU, so an export never mixes forms.
Timed on **Chrome 155.0.8059.84** (the bench Chrome auto-updated from .40; the harness warns):

| project | `phase2b` s | `i420` s | gain | UI p95 ms | wasm peak MiB |
|---|---|---|---|---|---|
| P1s | 5.76 | 2.54 ± 0.00 | 2.27× | 116 → 16 | 237 → 160 |
| P2s | 12.83 | 4.80 ± 0.10 | 2.68× | 102 → 25 | 472 → 387 |
| P3s | 24.51 | 10.09 ± 0.04 | 2.43× | 357 → 91 | 813 → 655 |

P3s stage times (GPU, median): render 10.2 s, encode 6.6 s, RGB→YUV 6.3 s; GPU read-back latency
~0.2 s.

How P3s got there (each step a run): with the 2 GiB `mt` heap it trapped; lean caches + 4 GiB
kept it at 1.6–1.7 GB but at ~19 s/frame, because the Look LUT made the planner render the whole
4K Blur + Lumetri + Vignette layer on the CPU (the async renderer's prefetch planned 3 such frames
per step: 20–25 s main-thread stalls); with the LUT on the GPU every frame went to the GPU, then
the export livelocked at frame 116 (WebCodecs restarting at a CRA, ~185 decodes/s, nothing
delivered); the open-GOP start fixed that.

Still to do for Phase 2:
- P3s on the CPU compositor: timed (label `i420`, one run): 1003.4 s, valid, PSNR 54.42 dB; the
  GPU path is 99× faster. Needs `--timeout-min 30` (the default 10 min cuts it at frame 110).
  Agents may launch the bench Chrome themselves when CDP on 9222 is down.
- UI p95 on the GPU path (P2s 25 ms, P3s 91 ms; P1s meets 20 ms): steps render a batch of
  several frames; size batches by measured step time.
- I420 for exports that resize, place or overlay (the Pipeline then keeps RGBA), and NV12 /
  P010 for HDR (the plan's other conversions).
- Device rule from the revised plan: done. `gpu_export::install` takes plain `wgpu` handles
  (`filmcraft_gpu::wgpu`, re-exported so a worker needs no UI crate).
- GPU coverage: Lumetri input LUT, curves, wheels, HSL secondary and Sharpen still send a layer to
  the CPU; transitions other than dissolve / push (`FramePlan::Composite`) too.
- Step length: a frame whose plan has a CPU layer still costs seconds inside a step, times the
  prefetch batch; cap prefetch planning per step by measured cost (or leave it to 4a).
- Zero-copy (`new VideoFrame(webgpuCanvas)`): not started; the plan says measure, don't assume
  (with I420 read back, the read-back is 1.5 B/px and no longer the obvious cost).

### Working rules that held up
- Agents work in git worktrees and commit small steps on their branch; the coordinator merges to
  `main` (fast-forward / rebase) after a full gate run and pushes.
- Disk is tight (bench machine ~17 GB free after deleting iOS simulators; was 2.2 GB): share
  `CARGO_TARGET_DIR=/Users/yishengj/filmcraft/target`, `CARGO_INCREMENTAL=0`, `--release`
  everywhere; a worktree at another path rebuilds every workspace crate, so agents run per-crate
  gates and the fast-tier `cargo xtask ci` before each push; the full suite runs in the
  background in Export CI (`.github/workflows/export-ci.yml`, nightly / on demand) and once at
  phase end. Nothing waits on it.
- Only one timed benchmark at a time, never while anything compiles; agents do single
  correctness runs only.

## Next steps (in order)

The plan was revised on 2026-10-17 (PR #1, `7b9939be` + `b200e580`; loop ladder `66af744f`): after
Phase 2 the order is **3a → 4a → 4b → 3b → 5**; 3a and 4a do not depend on Phase 2.

1. Chrome is 155.0.8059.84 now: consider re-timing `phase2b` (`61362d26`) on it for a clean A/B.
2. **Phase 2 leftovers**, cheapest first: batch size by step time (UI p95); remaining
   GPU coverage (Lumetri input LUT / curves / wheels / HSL / Sharpen, adjustment layers, complex
   transitions, HDR); `importExternalTexture` for decoded frames; zero-copy `new
   VideoFrame(webgpuCanvas)` (measure first); `timestamp-query` timings.
3. **Harness pieces the revised §4 loop names** (not built): `xtask web --no-opt`; a 60-frame P2
   smoke project; first-error abort on console / WebGPU errors; `--stall-s` default 15 and a
   timeout of 3× the baseline wall time; sampled-frame PSNR; A/B interleaving on two ports with
   early stop; `bench-compare --map` for differently named configs.
4. **3a**: bounded chunk queue with backpressure in front of the existing `StreamTarget`; fMP4 for
   unseekable sinks, reserved-`moov` progressive MP4 for seekable ones; A/V interleaving per
   chunk; remove `sink: "memory"` and the `write_output` copies; `storage.estimate()` /
   `persist()`; peak tab memory in the criteria.
5. **4a**: adaptive `scheduler.yield` pump; GIF, image sequences and audio-only stepped.
6. **4b**: export worker with its own wasm instance, `wgpu` device and WebCodecs decoders (keep
   the open-GOP start and run-ahead pinning); the OPFS sync-handle sink; parallel CPU rendering on
   the `mt` build.
7. **3b**: save picker sink.
8. **5**, only if a WebCodecs `prefer-software` vs `h264enc` bench justifies it: software path in
   Chrome (`--encoder software`, P1s must keep sha `97aad066…`); slice threads; remaining hot
   spots (16x16 SATD in the sub-pel search, CABAC `renorm`, `mc_luma`, half-sample
   interpolation, deblocking). `Preset::Speed` stays opt-in (it changes the output).
9. **Not started from Phase 1**: a WebCodecs `AudioEncoder` (must signal priming in `edts`), JSPI
   bridging. **Later** (per the plan): memory64, Wake Lock, Web Locks, notifications, badges.
10. **Full-length projects**: P2, P3 and P4 (60 min) have never been run; P4 is the streaming /
    memory criterion. Needs ~5 GB of references and more free disk.
11. **Housekeeping**: Export CI (`.github/workflows/export-ci.yml`) is green on the push jobs; its
    nightly full suite will show the two known ui-egui failures until they are fixed. Bench disk:
    ~7.7 GiB free after deleting `target/debug` and two scratch dirs; `target/release` is 19 GB
    of build cache (`cargo clean --release` frees it, at the cost of a rebuild).

## Phase 5 (software H.264 fallback, `crates/h264enc`)

`wide` SIMD kernels (SATD, SAD, SSD on wasm, 4x4/8x8 transforms + quant + dequant, residual
gather/reconstruction) plus output-identical algorithmic savings; details and per-kernel numbers in
[`crates/h264enc/README.md`](../crates/h264enc/README.md#speed). Single-threaded 1080p P1s footage with
the export's settings: native 79.8 → 45.7 ms/frame, wasm `simd128` (node 22, `h264enc_bench` as
wasm32-wasip1) 109–117 → 58–61 ms/frame (1.9x; target ≥ 1.8x). Bit-exact: per-kernel tests vs the
scalar code, `golden_bytes` (also run as wasm), export determinism tests.
