# filmcraft-h264enc

A clean-room, pure-Rust H.264/AVC encoder for FilmCraft's export path, written from the public
ITU-T H.264 specification (04/2013 edition) and general encoder literature. Layer L0: it depends only on
`filmcraft-bitstream`, `thiserror`, `wide` (safe portable 128-bit SIMD for the hot kernels, see "Speed") and
(optionally, feature `threads`, on by default) `rayon`. No `unsafe`, builds for `wasm32-unknown-unknown` with or
without `threads`.

## Usage

```rust
use filmcraft_h264enc::{Encoder, EncoderConfig, PacketFormat, Preset, Profile, RateControl, YuvFrame};

let mut cfg = EncoderConfig::new(1920, 1080, 30000, 1001);
cfg.profile = Profile::High;
cfg.preset = Preset::Balanced;              // Speed / Balanced / Quality
cfg.rate = RateControl::Crf(20.0);          // Crf(f32) | Cbr { kbps } | Vbr { target_kbps, max_kbps } | Qp(u8)
cfg.bframes = 2;                            // 0..=3
cfg.keyint = 250;
cfg.format = PacketFormat::LengthPrefixed;  // MP4 samples; or AnnexB (default) for .h264
let mut enc = Encoder::new(cfg)?;
let avcc = enc.avcc();                      // AVCDecoderConfigurationRecord for the MP4 `avcC` box
for (i, pic) in pictures.iter().enumerate() {
    let frame = YuvFrame { y: &pic.y, u: &pic.u, v: &pic.v, y_stride: w, uv_stride: w / 2 };
    for p in enc.encode(&frame, i as i64) { /* p.data, p.pts, p.dts, p.keyframe, p.frame_type */ }
}
for p in enc.flush() { /* ... */ }
```

- Input: planar 4:2:0 8-bit (`YuvFrame` with strides); any even size, non-multiple-of-16 sizes are
  cropped via the SPS frame cropping. `try_encode` returns an error instead of panicking on short planes.
- Output: one `Packet` per picture in decoding order with `pts` (as given), `dts` (reorder-delayed, always
  `<= pts` and strictly increasing), `keyframe` (IDR) and the frame type/QP. Annex-B packets carry an AUD,
  SPS/PPS on every IDR and an identification SEI on the first picture.
- Two-pass: encode once with `cfg.pass = Pass::First`, take `enc.pass_stats()` (serialisable with
  `PassStats::to_text/from_text`), then encode again with `Pass::Second(stats)`.
- `set_recon_capture(true)` + `take_recon()` expose the encoder's reconstructed pictures (used by the tests to
  prove encoder/decoder match).

## Features

| Area | Implemented |
|---|---|
| Profiles | Constrained Baseline (CAVLC, I/P), Main (CABAC, I/P/B), High (CABAC, 8x8 transform, I/P/B); 4:2:0 8-bit, frame coding |
| Parameter sets | SPS (level auto-selected from size/rate/DPB/bitrate, cropping, VUI: SAR, colour description + range, timing, bitstream restriction/reorder), PPS, AUD, user-data SEI, avcC |
| Intra | 16x16 (4 modes, SATD), 4x4 (9 modes, SATD + mode-cost, sequential reconstruction), 8x8 (9 modes with reference filtering, High), chroma (4 modes) |
| Inter P | hexagon integer search from predictors, half- then quarter-sample refinement (SATD) on precomputed 6-tap half-sample planes; 16x16/16x8/8x16/8x8 partitions (Balanced/Quality); P_Skip detection with the normative skip MV |
| Inter B | 1–3 non-reference B-frames, spatial direct / B_Skip, L0/L1/Bi 16x16 and (Balanced/Quality) 16x8/8x16 with a per-partition direction; all B-frames of a mini-GOP are encoded concurrently |
| Residual | 4x4 / 8x8 integer transforms, Intra16x16 and chroma DC Hadamards, deadzone quantisation (intra 1/3, inter 1/6), adaptive 4x4/8x8 inter transform choice, coefficient decimation for inter blocks |
| Entropy | CABAC (all I and cabac_init_idc 0 context tables, verified against the spec text) and CAVLC (all coeff_token/total_zeros/run_before tables, level escapes) |
| Loop filter | normative deblocking (bS 0–4 incl. bi-predicted rules, 8x8-transform edges, chroma QP mapping) in the reconstruction loop |
| Mode decision | SATD + λ·bits for Speed/Balanced; Quality adds RD-lite (SSD + λ·CABAC-estimated bits) between the best intra/inter/skip candidates |
| Rate control | `Qp`, `Crf` (constant quality with I/P/B offsets), 1-pass ABR with complexity model + VBV (`Cbr`, 1-pass `Vbr`), closed-loop 2-pass VBR; variance adaptive quantisation (`aq_strength`), lookahead scene-cut IDR insertion |
| Threading | slice-parallel encoding (MB-row slices, `disable_deblocking_filter_idc = 2` with >1 slice so each slice is also deblocked in parallel), parallel half-sample interpolation and lookahead |

## Verification

`cargo test --release -p filmcraft-h264enc` (ffmpeg/ffprobe used only as external oracles; tests skip when
absent). Every stream is decoded with `ffmpeg -v warning -err_detect +crccheck+bitstream+buffer+explode -f rawvideo`
and must print nothing (not even a `corrupt decoded frame` warning), produce exactly the number of frames encoded,
and be **bit-identical to the encoder's reconstruction** — this covers Baseline/Main/High, all presets, 0–3
B-frames, 1–4 slices and automatic slicing at 1080p, odd sizes, `testsrc2` and `mandelbrot` sources, and
length-prefixed output re-wrapped with the avcC parameter sets. B-frame display order is checked with ffprobe. A
negative control drops and truncates slices and checks that the oracle reports them.

The oracle leaves ffmpeg's error concealment on. With `-ec 0`, ffmpeg 7.1 and 8.1 print `corrupt decoded frame`
for every picture that has more than one slice, libx264's multi-slice streams included, while the decoded pixels
are unchanged (#74). The bit-exact comparison is what rules out concealment.

Results on the synthetic content (moving gradient, grain noise, text-like edges, sub-pixel panning):

| Test | Result |
|---|---|
| 1080p, High, Speed, CRF 20, 2 B | mean luma PSNR 43.0 dB (IDR 39.1, P/B 43.2–44.5) |
| 640x360 CBR 1500 kbit/s | 1518 kbit/s (+1.2%) |
| 640x360 1-pass VBR 1000 kbit/s | 981 kbit/s (−1.9%) |
| 640x360 2-pass VBR 800 kbit/s | 840 kbit/s (+5.0%) |

Development tool: `cargo run --release -p filmcraft-h264enc --example h264enc_synth -- W H FRAMES
[profile] [preset] [bframes] [qpN|crfN|cbrN|vbrN] [slices] [synth|testsrc2|mandelbrot]` encodes, decodes with
ffmpeg and reports speed, bitrate, PSNR and any encoder/decoder mismatch (`MAP=1` prints a per-MB mismatch map,
`PERF=k` cycles k source pictures for speed runs; `yuv:FILE` encodes a raw yuv420p file).

## Speed

The hot kernels are portable 128-bit SIMD written with `wide` (safe API; wasm `simd128`, NEON, SSE2, plain arrays
elsewhere), each **bit-identical** to the scalar code, which stays in the crate as the test reference (and as the
path for unusual block sizes):

| Kernel | Where | Scalar → SIMD, ns (native M4 Pro / wasm simd128, node 22) |
|---|---|---|
| SATD 16x16 / 8x8 / 8x16 | `dsp::satd` | 33 → 26 / 28 → 6 / 53 → 12 (wasm: 58 → 31 / 49 → 8 / 95 → 15) |
| SAD 16x16 | `dsp::sad16` | 10 → 9 (wasm 27 → 14) |
| SSD 16x16 (wasm only; native scalar is faster) | `dsp::ssd` | wasm 32 → 18 |
| 4x4 residual pipeline, 16 blocks | `kernels` (residual, fdct4, quant4, dequant4, idct4, recon) | 264 → 224 (wasm 453 → 294) |
| 8x8 residual pipeline, 4 blocks | `kernels` (residual, fdct8, quant8, dequant8, idct8, recon) | 372 → 243 (wasm 693 → 375) |

`kernels` holds several blocks per vector (`value[pos][lane]`, lane = block) so the transforms are the scalar
butterflies on four blocks at a time; gathers/scatters use one byte swizzle per row; 4x4 quantisation and the 8x8
one run in exact 32-bit arithmetic (bounds in the doc comments). Output-identical algorithmic savings: the adaptive
4x4/8x8 choice keeps both coded results instead of coding the winner a third time, sub-pel refinement memoises
costs of vectors it already evaluated, all-zero blocks skip scan/decimation, and a macroblock without levels copies
its prediction instead of running the inverse transform.

Whole encoder, single-threaded (`--no-default-features`, as on the web), 1080p of P1s footage with the export's
settings (High, Balanced, 2 B-frames, VBR 12 Mb/s, 17 slices): native 79.8 → 45.7 ms/frame (1.75x), wasm simd128
under node 22 109–117 → 58–61 ms/frame (1.9x). Bytes unchanged.

```sh
cargo run --release -p filmcraft-h264enc --no-default-features --example h264enc_bench -- 1920 1080 30 clip.yuv
RUSTFLAGS="-C target-feature=+simd128" cargo build --release -p filmcraft-h264enc --no-default-features \
  --example h264enc_bench --target wasm32-wasip1    # run the .wasm with node:wasi (preopen the yuv's directory)
cargo test --release -p filmcraft-h264enc --lib bench_ -- --ignored --nocapture   # kernel timings
```

Bit-exactness: per-kernel tests against the scalar code (random, 0/255, checkerboard and near-flat blocks, odd
strides and offsets, every QP; residuals up to ±600 for the clipping), `tests/golden_bytes.rs` (six configurations
hashed with the scalar encoder before the SIMD work; slice counts pinned so hashes are machine-independent), and the
export determinism tests. The lib tests and `golden_bytes` also pass as `wasm32-wasip1` with `simd128` under node,
so the SIMD wasm build writes the same bytes as the scalar native encoder. Node 22 occasionally segfaults while
tiering up this module (V8, not the encoder); run it with `--no-liftoff`.

## Limitations / not yet implemented

- Single reference frame per list; no B-pyramid, no weighted prediction, no interlaced/MBAFF, no 4:2:2/4:4:4 or
  high bit depth, no custom scaling matrices, no sub-8x8 partitions, no B_8x8.
- CAVLC is used only for Baseline (High/Main always use CABAC); 8x8 transform is therefore CABAC-only.
- No trellis quantisation, no psy-RD; no HRD/buffering-period SEI (VBV is enforced internally only).
- Threading is slice-based only (no frame-level pipelining); with many slices, slice edges are not deblocked.
