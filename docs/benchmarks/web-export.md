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

## Iteration log

| # | Label | Hypothesis | Result | Decision |
|---|---|---|---|---|
| 0 | `baseline` | – (harness + reference numbers) | table above | reference for every later claim |
| 1 | `phase1` | WebCodecs hardware encode removes the ~100 ms/frame encode: P1s ≥ 3×, P2s ~1.3× (render-bound); streaming cuts wasm memory | P1s 3.3×, P2s 1.26×, wasm −14 % / −30 %, PSNR +0.6–0.7 dB, all valid | kept (`bd13e6c8`) |
