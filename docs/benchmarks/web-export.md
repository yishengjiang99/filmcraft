# Web export benchmark

Export in the browser build, measured with the harness in `apps/filmcraft-web/tests/`
(`export-bench.mjs`, `bench-matrix.mjs`, `bench-compare.mjs`; see `docs/web.md` § Export benchmark)
against the plan in `docs/export-optimize-gpu.md`. Every number cites a `results.jsonl` line
(label + git sha) in `target/web-bench/` on the bench machine.

**Bench machine.** Apple M4 Pro (14 cores, 24 GB), macOS 27.0.1, on AC power; Chrome
155.0.8059.40 (headed, own profile, attached over CDP on 127.0.0.1:9222); WebGPU and video
encode/decode hardware accelerated (`chrome://gpu`), WebGPU adapter `metal-3` with
`timestamp-query` and `shader-f16`. Hardware WebCodecs encoders: H.264 (Baseline/Main/High, to
4K; CBR, VBR, quantizer) and HEVC Main 8-bit; no hardware HEVC Main 10, AV1 or VP9
(`target/web-bench/env.json`).

**Projects** (built through `window.filmcraft`, `tests/bench-projects.mjs`): P1 one 1080p30 clip,
20 s; P2 1080p30, 60 s, 2 video tracks, 3 cuts, a cross-dissolve, Lumetri basic, a title, dialogue
+ music with fades; P3 4K30, 20 s, Gaussian blur, a Lumetri look, vignette, a PiP with a scale
keyframe, 3 titles; P4 P1 looped to 60 min. Media: `tests/fixtures.mjs` (deterministic; a white
flash + beep every 5 s for A/V sync).

**Method.** Release build with wasm-opt (`cargo xtask web --threads`), a fresh tab per run, OPFS
cleared, the timeline ruler scrubbed every 500 ms during the export (UI frame times are measured
under that load), H.264 at a pinned bitrate (12 Mb/s at 1080p, 40 Mb/s at 4K, VBR). One warmup
run is discarded; medians (± MAD) of 3 measured runs, rerun when the wall-time CV exceeds 5 %.
Outputs are validated (ffprobe, `-xerror`, A/V sync, PSNR/SSIM against a CLI reference render).

## 2026-10-16: baseline (label `baseline`)

The export path before any change of this plan (`--legacy`: built-in H.264 encoder, CPU
compositor, output buffered in wasm memory). `runs` = measured runs + warmups; P3s ran once
(18.6 min a run) and the single-threaded P2s twice, both without warmup (a warmup is noise
against multi-minute runs). wall ± MAD. Stage times are the exporter's own counters
(`perf.stats` `export.stages`) of one run.

| config | runs | wall s | fps | realtime | UI p95 ms | LoAF blocking s | wasm peak MiB | render / encode / convert s | Y-PSNR dB (min) | sha |
|---|---|---|---|---|---|---|---|---|---|---|
| P1-h264-software-cpu-auto-mt | 3+1w | 81.1 ± 0.3 | 7.40 | 0.250 | 325 | 53 | 686 | 15.2 / 63.0 / 5.3 | 47.31 (43.7) | 7a02034c |
| P1s-h264-software-cpu-auto-mt | 3+1w | 23.6 ± 0.0 | 7.63 | 0.250 | 309 | 15 | 619 | 4.6 / 17.9 / 1.5 | 47.48 (43.7) | 7a02034c |
| P2s-h264-software-cpu-auto-mt | 3+1w | 168.2 ± 0.2 | 2.14 | 0.070 | 667 | 148 | 1400 | 130.1 / 36.6 / 3.1 | 46.45 (41.64) | 7a02034c |
| P2s-h264-software-cpu-auto-st | 2 | 168.8 ± 0.3 | 2.13 | 0.070 | 675 | 149 | 1332 | 130.2 / 36.7 / 3.1 | 46.45 (41.64) | 788dae8c |
| P3s-h264-software-cpu-auto-mt | 1 | 1118.0 ± 0.0 | 0.16 | 0.010 | 5292 | 1100 | 1933 | 1041.2 / 73.8 / 6.3 | 54.79 (50.53) | 788dae8c |


- **Render dominates edits with effects**: P2s 77 % (361 ms a 1080p frame), P3s 93 % (5.8 s a 4K
  frame). The built-in encoder dominates plain clips (P1: 78 %, ~105 ms a 1080p frame).
- **Threads do not help export today**: `mt` and `st` P2s are the same (168.2 vs 168.8 s); the
  export runs cooperatively on the UI thread in both.
- **The UI is unusable during an export**: p95 frame 0.3–0.7 s at 1080p, 5.3 s at 4K; every animation
  frame during the export is a long one.
- **Memory**: 0.6–1.9 GiB of wasm heap; the encoded output is a few to 30 MB of it, the rest is
  decode / frame caches (to be broken down: `memoryDetail` in later results).
- **Correctness**: every output passes `validate-export.mjs` (A/V sync 0.02 ms, frame counts,
  `-xerror`, colour tags; Y-PSNR vs the CLI reference 46–55 dB, SSIM ≥ 0.98).

## 2026-10-16: phase 1 + 3 (label `phase1`, `bd13e6c8`)

WebCodecs hardware H.264 encoder, OPFS streaming sink, CPU compositor (unchanged).

| config | runs | wall s | fps | realtime | UI p95 ms | LoAF blocking s | wasm peak MiB | render / encode / convert s | Y-PSNR dB (min) |
|---|---|---|---|---|---|---|---|---|---|
| P1s-h264-hardware-cpu-opfs-mt | 3+1w | 7.1 ± 0.0 | 25.19 | 0.840 | 42 | 0 | 533 | 4.5 / 1.6 / 1.5 | 48.09 (43.77) |
| P2s-h264-hardware-cpu-opfs-mt | 2 | 133.8 ± 0.0 | 2.69 | 0.090 | 658 | 113 | 981 | 128.4 / 3.3 / 3.1 | 47.19 (43.54) |

vs baseline: P1s 23.6 → 7.1 s (3.3×), UI p95 309 → 42 ms; P2s 168.2 → 133.8 s (1.26×), wasm peak
1400 → 981 MiB. Encode per 1080p frame 99 ms → 9 ms; P2s is now 96 % CPU render. Quality is up
slightly (hardware encoder at the same bitrate). The software path (`?softenc` / `encoder:
software`) is byte-identical to the baseline.

## 2026-10-17: phase 2 (label `phase2b`, `61362d26`)

GPU compositing for export on eframe's WebGPU device (asynchronous read-back ring), lean caches
during web exports, 4 GiB `mt` heap, Lumetri Look LUT on the GPU, WebCodecs open-GOP start and
run-ahead pinning. Hardware encoder, OPFS sink; `--compositor gpu` (`gpuRendering: auto`) vs
`cpu` (`off`) in the same build. Every output validated; every GPU run drew all frames on the GPU.

| config | runs | wall s | fps | realtime | UI p95 ms | LoAF blocking s | wasm peak MiB | render / encode / convert s | Y-PSNR dB (min) |
|---|---|---|---|---|---|---|---|---|---|
| P1s-h264-hardware-gpu-opfs-mt | 3+1w | 5.76 ± 0.02 | 31.26 | 1.040 | 116 | 2 | 237 | 2.5 / 1.6 / 1.6 | 48.05 (43.78) |
| P1s-h264-hardware-cpu-opfs-mt | 3+1w | 7.14 ± 0.03 | 25.23 | 0.840 | 42 | 0 | 473 | 4.5 / 1.6 / 1.6 | 48.09 (43.77) |
| P2s-h264-hardware-gpu-opfs-mt | 3+1w | 12.83 ± 0.01 | 28.06 | 0.940 | 102 | 4 | 472 | 5.5 / 3.2 / 3.1 | 46.38 (43.18) |
| P2s-h264-hardware-cpu-opfs-mt | 3+1w | 134.49 ± 0.38 | 2.68 | 0.090 | 658 | 114 | 961 | 129.0 / 3.3 / 3.1 | 47.19 (43.54) |
| P3s-h264-hardware-gpu-opfs-mt | 3+1w | 24.51 ± 0.46 | 7.34 | 0.240 | 357 | 14 | 813 | 10.2 / 6.6 / 6.3 | 51.32 (48.65) |
| P3s-h264-hardware-cpu-opfs-mt | – | – | – | – | – | – | – | – | – |

P3s on the CPU compositor did not complete: the benchmark Chrome exited at frame 165 of 180
(~15.5 min in; no crash report). Not rerun yet.

vs baseline: P1s 23.6 → 5.76 s (4.1×); P2s 168.2 → 12.83 s (13.1×; plan target ≥ 4× met);
P3s 1118 → 24.51 s (45.6×; target ≥ 6× met). GPU vs CPU compositor in the same build: P1s
1.24×, P2s 10.5×. Wasm peak −62 % (P1s), −66 % (P2s), −58 % (P3s) vs baseline. UI p95 on the
GPU path is 102–357 ms (CPU compositor P1s: 42 ms): the GPU path steps in batches of several
frames; the 20 ms target belongs to phases 4a / 4b. The CPU-compositor rows match phase 1, so
the lean caches cost nothing there once the decoder's run-ahead is pinned (before `61362d26`
P1s on the CPU took 22.5 s: 3061 frames decoded for 180).

## 2026-10-17: GPU RGB→YUV (label `i420`, `32baa127`)

The async export renderer converts the composite to BT.709 I420 on the GPU (`crates/gpu/src/i420.wgsl`,
byte-identical to the CPU conversion in the unit test) and the WebCodecs encoder takes it as it is:
1.5 bytes a pixel read back instead of 8, no CPU half → f32 / sRGB / YUV passes. **Chrome is
155.0.8059.84** (auto-updated from 155.0.8059.40, which the earlier tables used): the numbers are
not strictly comparable, though the change is far beyond what a patch release moves. Same
machine, `mt`, hardware encoder, OPFS; every output validated, every frame on the GPU.

| config | runs | wall s | fps | realtime | UI p95 ms | LoAF blocking s | wasm peak MiB | render / encode / convert s | Y-PSNR dB (min) |
|---|---|---|---|---|---|---|---|---|---|
| P1s-h264-hardware-gpu-opfs-mt | 3+1w | 2.54 ± 0.00 | 70.84 | 2.360 | 16 | 0 | 160 | 0.1 / 0.1 / 0.0 | 48.05 (43.78) |
| P2s-h264-hardware-gpu-opfs-mt | 6+1w (noisy) | 4.80 ± 0.10 | 75.08 | 2.500 | 25 | 0 | 387 | 0.6 / 0.1 / 0.0 | 46.41 (43.18) |
| P3s-h264-hardware-gpu-opfs-mt | 3+1w | 10.09 ± 0.04 | 17.85 | 0.590 | 91 | 1 | 655 | 0.9 / 0.4 / 0.0 | 51.31 (48.65) |
| P3s-h264-hardware-cpu-opfs-mt | 1 | 1003.4 | 0.18 | 0.010 | 4050 | 985 | 1440 | 994.2 / 6.5 / 6.2 | 54.42 (51.20) |

vs `phase2b`: P1s 5.76 → 2.54 s (2.27×), P2s 12.83 → 4.80 s (2.68×), P3s 24.51 → 10.09 s (2.43×);
UI p95 116 / 102 / 357 → 16 / 25 / 91 ms (P1s meets the 20 ms target); wasm peak −18–33 %; PSNR
unchanged. vs baseline: P1s 9.3×, P2s 35×, P3s 111×. The stage times are now mostly the main
thread's share (submissions); the rest of the wall time is GPU work and read-back latency.
The P3s CPU-compositor row (the one `phase2b` could not finish) is one run: the GPU path is 99× faster than the CPU compositor on P3s. P2s's first runs varied (CV 22 %, 4.6–6.7 s); the reruns settled at 4.7–4.9 s.

## Iteration log

| # | Label | Hypothesis | Result | Decision |
|---|---|---|---|---|
| 0 | `baseline` | – (harness + reference numbers) | table above | reference for every later claim |
| 1 | `phase1` | WebCodecs hardware encode removes the ~100 ms/frame encode: P1s ≥ 3×, P2s ~1.3× (render-bound); streaming cuts wasm memory | P1s 3.3×, P2s 1.26×, wasm −14 % / −30 %, PSNR +0.6–0.7 dB, all valid | kept (`bd13e6c8`) |
| 2 | `phase2b` | GPU compositing removes the CPU render (P2s 96 % of the time): P2s ≥ 4×, P3s ≥ 2.5× vs phase 1 | P1s 4.1×, P2s 13.1×, P3s 45.6× vs baseline; GPU 10.5× the CPU compositor on P2s; wasm −58–66 %; all valid; UI p95 102–357 ms; P3s CPU row missing (browser exited) | kept (`2ffd57eb`..`61362d26`) |
| 3 | `i420` | The GPU converts to I420 (1.5 B/px read back instead of 8, no CPU convert): P3s loses its 6.3 s convert and most of the read-back | P1s 2.27×, P2s 2.68×, P3s 2.43× vs `phase2b`; UI p95 16 / 25 / 91 ms; all valid, PSNR unchanged; Chrome 155.0.8059.84 (was .40) | kept (`32baa127`) |
