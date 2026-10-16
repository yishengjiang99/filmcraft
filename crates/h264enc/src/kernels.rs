//! Lane-parallel ("structure of arrays") residual kernels.
//!
//! The macroblock residual pipeline (residual → forward transform → quantisation → dequantisation → inverse
//! transform → reconstruction) runs on several independent blocks at once: a value array `v[pos][lane]` holds
//! coefficient `pos` (raster order inside the block) of block `lane`. The transforms, quantisation and
//! dequantisation then run the scalar butterflies on four blocks at a time with `wide::i32x4` (safe portable
//! SIMD: wasm `simd128`, NEON, SSE2, or plain arrays elsewhere); `L` is a multiple of 4. Gathering the
//! residual into lanes and scattering the reconstruction are plain loops.
//!
//! Each kernel performs the same integer operations as the scalar functions in [`crate::transform`]
//! (which stay the reference), so results are bit-identical; the tests below check that on random and
//! extreme blocks.

use wide::i32x4;

use crate::transform::{Deadzone, QuantTables};
#[cfg(test)]
use crate::transform::{fdct4, fdct8, idct4, idct8};

/// `L` blocks of 16 coefficients, `[pos][lane]`.
pub type Lanes4<const L: usize> = [[i32; L]; 16];
/// `L` blocks of 64 coefficients, `[pos][lane]`.
pub type Lanes8<const L: usize> = [[i32; L]; 64];

/// `src - pred` of one 16-sample row as four vectors.
#[inline(always)]
fn diff16(s: &[u8; 16], p: &[u8; 16]) -> [i32x4; 4] {
    let d = |k: usize| i32x4::new([0, 1, 2, 3].map(|i| s[4 * k + i] as i32 - p[4 * k + i] as i32));
    [d(0), d(1), d(2), d(3)]
}

/// `src - pred` of one 8-sample row as two vectors.
#[inline(always)]
fn diff8(s: &[u8; 8], p: &[u8; 8]) -> [i32x4; 2] {
    let d = |k: usize| i32x4::new([0, 1, 2, 3].map(|i| s[4 * k + i] as i32 - p[4 * k + i] as i32));
    [d(0), d(1)]
}

/// `clip(pred + r)` of one row of `4 * N` samples.
#[inline(always)]
fn add_clip<const N: usize, const W: usize>(p: &[u8; W], r: [i32x4; N], out: &mut [u8; W]) {
    let r: [i32; W] = std::array::from_fn(|i| r[i / 4].as_array_ref()[i % 4]);
    for i in 0..W {
        out[i] = (p[i] as i32 + r[i]).clamp(0, 255) as u8;
    }
}

/// Residual `src - pred` of the sixteen 4x4 blocks of a 16x16 block (stride 16); lane = `by * 4 + bx`.
/// Each row's four 4-sample groups are transposed into the four lanes of one block row.
#[inline]
pub fn residual4_16x16(src: &[u8; 256], pred: &[u8; 256]) -> Lanes4<16> {
    let (s, p) = (src.as_chunks::<16>().0, pred.as_chunks::<16>().0);
    let mut d = [[0i32; 16]; 16];
    for (y, (s, p)) in s.iter().zip(p).enumerate() {
        let t = i32x4::transpose(diff16(s, p));
        for xx in 0..4 {
            store(&mut d[(y & 3) * 4 + xx], (y >> 2) * 4, t[xx]);
        }
    }
    d
}

/// Residual of the four 4x4 blocks of each of two 8x8 blocks (stride 8); lane = `comp * 4 + by * 2 + bx`.
#[inline]
pub fn residual4_2x8x8(src: [&[u8; 64]; 2], pred: [&[u8; 64]; 2]) -> Lanes4<8> {
    let mut d = [[0i32; 8]; 16];
    for comp in 0..2 {
        let (s, p) = (src[comp].as_chunks::<8>().0, pred[comp].as_chunks::<8>().0);
        for yy in 0..4 {
            let (r0, r1) = (diff8(&s[yy], &p[yy]), diff8(&s[yy + 4], &p[yy + 4]));
            let t = i32x4::transpose([r0[0], r0[1], r1[0], r1[1]]);
            for xx in 0..4 {
                store(&mut d[yy * 4 + xx], comp * 4, t[xx]);
            }
        }
    }
    d
}

/// Residual of the four 8x8 blocks of a 16x16 block (stride 16); lane = `by * 2 + bx`.
#[inline]
pub fn residual8_16x16(src: &[u8; 256], pred: &[u8; 256]) -> Lanes8<4> {
    let (s, p) = (src.as_chunks::<16>().0, pred.as_chunks::<16>().0);
    let mut d = [[0i32; 4]; 64];
    for yy in 0..8 {
        let (a, b) = (diff16(&s[yy], &p[yy]), diff16(&s[yy + 8], &p[yy + 8]));
        let t = i32x4::transpose([a[0], a[2], b[0], b[2]]);
        let u = i32x4::transpose([a[1], a[3], b[1], b[3]]);
        for xx in 0..4 {
            d[yy * 8 + xx] = t[xx].to_array();
            d[yy * 8 + 4 + xx] = u[xx].to_array();
        }
    }
    d
}

/// Residual `src - pred` of the sixteen 4x4 blocks of a 16x16 block (stride 16); lane = `by * 4 + bx`.
#[inline]
#[cfg(test)]
pub fn residual4_16x16_scalar(src: &[u8; 256], pred: &[u8; 256]) -> Lanes4<16> {
    let mut d = [[0i32; 16]; 16];
    for y in 0..16 {
        for x in 0..16 {
            d[(y & 3) * 4 + (x & 3)][(y >> 2) * 4 + (x >> 2)] = src[y * 16 + x] as i32 - pred[y * 16 + x] as i32;
        }
    }
    d
}

/// Residual of the four 4x4 blocks of each of two 8x8 blocks (stride 8); lane = `comp * 4 + by * 2 + bx`.
#[inline]
#[cfg(test)]
pub fn residual4_2x8x8_scalar(src: [&[u8; 64]; 2], pred: [&[u8; 64]; 2]) -> Lanes4<8> {
    let mut d = [[0i32; 8]; 16];
    for comp in 0..2 {
        let (s, p) = (src[comp], pred[comp]);
        for y in 0..8 {
            for x in 0..8 {
                d[(y & 3) * 4 + (x & 3)][comp * 4 + (y >> 2) * 2 + (x >> 2)] = s[y * 8 + x] as i32 - p[y * 8 + x] as i32;
            }
        }
    }
    d
}

/// Residual of the four 8x8 blocks of a 16x16 block (stride 16); lane = `by * 2 + bx`.
#[inline]
#[cfg(test)]
pub fn residual8_16x16_scalar(src: &[u8; 256], pred: &[u8; 256]) -> Lanes8<4> {
    let mut d = [[0i32; 4]; 64];
    for y in 0..16 {
        for x in 0..16 {
            d[(y & 7) * 8 + (x & 7)][(y >> 3) * 2 + (x >> 3)] = src[y * 16 + x] as i32 - pred[y * 16 + x] as i32;
        }
    }
    d
}

/// Lanes `c..c + 4` of coefficient row `k`.
#[inline(always)]
fn load<const L: usize>(row: &[i32; L], c: usize) -> i32x4 {
    i32x4::new(std::array::from_fn(|i| row[c + i]))
}

#[inline(always)]
fn store<const L: usize>(row: &mut [i32; L], c: usize, v: i32x4) {
    row[c..c + 4].copy_from_slice(v.as_array_ref());
}

/// [`fdct4`]'s butterfly on four vectors (one coefficient of four blocks each).
#[inline(always)]
fn fdct4_v(a: i32x4, b: i32x4, c: i32x4, e: i32x4) -> [i32x4; 4] {
    let (s03, d03, s12, d12) = (a + e, a - e, b + c, b - c);
    [s03 + s12, (d03 << 1) + d12, s03 - s12, d03 - (d12 << 1)]
}

/// [`idct4`]'s butterfly on four vectors.
#[inline(always)]
fn idct4_v(d0: i32x4, d1: i32x4, d2: i32x4, d3: i32x4) -> [i32x4; 4] {
    let (e, f) = (d0 + d2, d0 - d2);
    let (g, h) = ((d1 >> 1) - d3, d1 + (d3 >> 1));
    [e + h, f + g, f - g, e - h]
}

/// [`fdct4`] of every lane (`L` a multiple of 4): the scalar row and column butterflies, four blocks per vector.
#[inline]
pub fn fdct4_lanes<const L: usize>(d: &Lanes4<L>) -> Lanes4<L> {
    let mut out = [[0i32; L]; 16];
    for c in (0..L).step_by(4) {
        let x: [i32x4; 16] = std::array::from_fn(|k| load(&d[k], c));
        let mut t = [i32x4::ZERO; 16];
        for i in 0..4 {
            let o = fdct4_v(x[i * 4], x[i * 4 + 1], x[i * 4 + 2], x[i * 4 + 3]);
            t[i * 4..i * 4 + 4].copy_from_slice(&o);
        }
        for i in 0..4 {
            let o = fdct4_v(t[i], t[4 + i], t[8 + i], t[12 + i]);
            for k in 0..4 {
                store(&mut out[k * 4 + i], c, o[k]);
            }
        }
    }
    out
}

/// [`idct4`] of every lane, including the final `(x + 32) >> 6`.
#[inline]
pub fn idct4_lanes<const L: usize>(d: &Lanes4<L>) -> Lanes4<L> {
    let round = i32x4::splat(32);
    let mut out = [[0i32; L]; 16];
    for c in (0..L).step_by(4) {
        let x: [i32x4; 16] = std::array::from_fn(|k| load(&d[k], c));
        let mut f = [i32x4::ZERO; 16];
        for i in 0..4 {
            let o = idct4_v(x[i * 4], x[i * 4 + 1], x[i * 4 + 2], x[i * 4 + 3]);
            f[i * 4..i * 4 + 4].copy_from_slice(&o);
        }
        for j in 0..4 {
            let o = idct4_v(f[j], f[4 + j], f[8 + j], f[12 + j]);
            for k in 0..4 {
                store(&mut out[k * 4 + j], c, (o[k] + round) >> 6);
            }
        }
    }
    out
}

/// [`fdct8`]'s 1-D transform on eight vectors. The constant multiplies are written as shifts and adds
/// (the same values; `wide`'s multiply by a scalar rebuilds the splat on every call).
#[inline(always)]
fn fdct8_v(s: [i32x4; 8]) -> [i32x4; 8] {
    let (a0, a1, a2, a3) = (s[0] + s[7], s[1] + s[6], s[2] + s[5], s[3] + s[4]);
    let (b0, b1, b2, b3) = (s[0] - s[7], s[1] - s[6], s[2] - s[5], s[3] - s[4]);
    let (e0, e1, e2, e3) = (a0 + a3, a1 + a2, a0 - a3, a1 - a2);
    let x3 = |v: i32x4| (v << 1) + v;
    let x6 = |v: i32x4| (v << 2) + (v << 1);
    let x10 = |v: i32x4| (v << 3) + (v << 1);
    let x12 = |v: i32x4| (v << 3) + (v << 2);
    [
        (e0 + e1) << 3,
        x12(b0) + x10(b1) + x6(b2) + x3(b3),
        (e2 << 3) + (e3 << 2),
        x10(b0) - x3(b1) - x12(b2) - x6(b3),
        (e0 - e1) << 3,
        x6(b0) - x12(b1) + x3(b2) + x10(b3),
        (e2 << 2) - (e3 << 3),
        x3(b0) - x6(b1) + x10(b2) - x12(b3),
    ]
}

/// [`idct8`]'s 1-D transform on eight vectors.
#[inline(always)]
fn idct8_v(s: [i32x4; 8]) -> [i32x4; 8] {
    let a0 = s[0] + s[4];
    let a4 = s[0] - s[4];
    let a2 = (s[2] >> 1) - s[6];
    let a6 = s[2] + (s[6] >> 1);
    let b0 = a0 + a6;
    let b2 = a4 + a2;
    let b4 = a4 - a2;
    let b6 = a0 - a6;
    let a1 = s[5] - s[3] - s[7] - (s[7] >> 1);
    let a3 = s[1] + s[7] - s[3] - (s[3] >> 1);
    let a5 = s[7] - s[1] + s[5] + (s[5] >> 1);
    let a7 = s[3] + s[5] + s[1] + (s[1] >> 1);
    let b1 = a1 + (a7 >> 2);
    let b7 = a7 - (a1 >> 2);
    let b3 = a3 + (a5 >> 2);
    let b5 = (a3 >> 2) - a5;
    [b0 + b7, b2 + b5, b4 + b3, b6 + b1, b6 - b1, b4 - b3, b2 - b5, b0 - b7]
}

/// [`fdct8`] of every lane (`L` a multiple of 4).
#[inline]
pub fn fdct8_lanes<const L: usize>(d: &Lanes8<L>) -> Lanes8<L> {
    let mut out = [[0i32; L]; 64];
    for c in (0..L).step_by(4) {
        let mut t = [i32x4::ZERO; 64];
        for y in 0..8 {
            let o = fdct8_v(std::array::from_fn(|k| load(&d[y * 8 + k], c)));
            t[y * 8..y * 8 + 8].copy_from_slice(&o);
        }
        for x in 0..8 {
            let o = fdct8_v(std::array::from_fn(|k| t[k * 8 + x]));
            for k in 0..8 {
                store(&mut out[k * 8 + x], c, o[k]);
            }
        }
    }
    out
}

/// [`idct8`] of every lane, including the final `(x + 32) >> 6`.
#[inline]
pub fn idct8_lanes<const L: usize>(d: &Lanes8<L>) -> Lanes8<L> {
    let round = i32x4::splat(32);
    let mut out = [[0i32; L]; 64];
    for c in (0..L).step_by(4) {
        let mut g = [i32x4::ZERO; 64];
        for y in 0..8 {
            let o = idct8_v(std::array::from_fn(|k| load(&d[y * 8 + k], c)));
            g[y * 8..y * 8 + 8].copy_from_slice(&o);
        }
        for x in 0..8 {
            let o = idct8_v(std::array::from_fn(|k| g[k * 8 + x]));
            for k in 0..8 {
                store(&mut out[k * 8 + x], c, (o[k] + round) >> 6);
            }
        }
    }
    out
}

/// `sign(v) · a` for `a >= 0`: `(a ^ m) - m` with `m = v >> 31` (all ones for negative `v`).
#[inline(always)]
fn apply_sign(a: i32x4, v: i32x4) -> i32x4 {
    let m = v >> 31;
    (a ^ m) - m
}

/// [`crate::transform::quant4`] of every lane (without the non-zero count).
///
/// The scalar code computes `(|v|·mf + f) >> qbits` in `i64`; here it is `i32`, which is exact: a 4x4
/// coefficient of an 8-bit residual is at most 9180 in magnitude, `mf <= 13107` and `f < 2^22`, so the sum
/// stays below 2^27.
#[inline]
pub fn quant4_lanes<const L: usize>(c: &mut Lanes4<L>, qt: &QuantTables, qp: u8, dz: Deadzone, skip_dc: bool) {
    let qbits = 15 + (qp / 6) as u32;
    let f = (((1i64 << qbits) * dz.f256) >> 8) as i32;
    let mf = &qt.mf4[(qp % 6) as usize];
    let start = if skip_dc { 1 } else { 0 };
    for k in start..16 {
        let (m, fv) = (i32x4::splat(mf[k]), i32x4::splat(f));
        for l in (0..L).step_by(4) {
            let v = load(&c[k], l);
            store(&mut c[k], l, apply_sign((v.abs() * m + fv) >> qbits, v));
        }
    }
}

/// [`crate::transform::quant8`] of every lane (without the non-zero count), with the same 64-bit products:
/// `|v| < 2^21` and `mf < 2^16`, so the signed widening multiply of the non-negative operands is exact.
#[inline]
pub fn quant8_lanes<const L: usize>(c: &mut Lanes8<L>, qt: &QuantTables, qp: u8, dz: Deadzone) {
    let qbits = 22 + (qp / 6) as u32;
    let f = ((1i64 << qbits) * dz.f256) >> 8;
    let mf = &qt.mf8[(qp % 6) as usize];
    for k in 0..64 {
        let m = i32x4::splat(mf[k].clamp(0, i32::MAX as i64) as i32);
        for l in (0..L).step_by(4) {
            let v = load(&c[k], l);
            let p = (v.abs().mul_widen(m) + f) >> qbits;
            let p = p.to_array();
            let a = i32x4::new([p[0] as i32, p[1] as i32, p[2] as i32, p[3] as i32]);
            store(&mut c[k], l, apply_sign(a, v));
        }
    }
}

/// [`crate::transform::dequant4`] of every lane.
#[inline]
pub fn dequant4_lanes<const L: usize>(c: &mut Lanes4<L>, qt: &QuantTables, qp: u8, skip_dc: bool) {
    let v = &qt.v4[(qp % 6) as usize];
    let sh = (qp / 6) as u32;
    let start = if skip_dc { 1 } else { 0 };
    for k in start..16 {
        for l in (0..L).step_by(4) {
            let x = load(&c[k], l);
            store(&mut c[k], l, (x * v[k]) << sh);
        }
    }
}

/// [`crate::transform::dequant8`] of every lane.
#[inline]
pub fn dequant8_lanes<const L: usize>(c: &mut Lanes8<L>, qt: &QuantTables, qp: u8) {
    let v = &qt.v8[(qp % 6) as usize];
    let q6 = (qp / 6) as i32;
    for k in 0..64 {
        let ls = 16 * v[k];
        for l in (0..L).step_by(4) {
            let x = load(&c[k], l) * ls;
            let y = if q6 >= 6 { x << (q6 - 6) as u32 } else { (x + (1 << (5 - q6))) >> (6 - q6) as u32 };
            store(&mut c[k], l, y);
        }
    }
}

/// Reconstruction `clip(pred + r)` of a 16x16 block from the residual lanes of [`residual4_16x16`].
#[inline]
pub fn recon4_16x16(pred: &[u8; 256], r: &Lanes4<16>, out: &mut [u8; 256]) {
    let p = pred.as_chunks::<16>().0;
    for (y, o) in out.as_chunks_mut::<16>().0.iter_mut().enumerate() {
        let a = i32x4::transpose(std::array::from_fn(|xx| load(&r[(y & 3) * 4 + xx], (y >> 2) * 4)));
        add_clip(&p[y], a, o);
    }
}

/// Reconstruction of a 16x16 block from the residual lanes of [`residual8_16x16`].
#[inline]
pub fn recon8_16x16(pred: &[u8; 256], r: &Lanes8<4>, out: &mut [u8; 256]) {
    let p = pred.as_chunks::<16>().0;
    let o = out.as_chunks_mut::<16>().0;
    for yy in 0..8 {
        let t = i32x4::transpose(std::array::from_fn(|xx| i32x4::new(r[yy * 8 + xx])));
        let u = i32x4::transpose(std::array::from_fn(|xx| i32x4::new(r[yy * 8 + 4 + xx])));
        add_clip(&p[yy], [t[0], u[0], t[1], u[1]], &mut o[yy]);
        add_clip(&p[yy + 8], [t[2], u[2], t[3], u[3]], &mut o[yy + 8]);
    }
}

/// Reconstruction of one 8x8 chroma block (component `comp`) from the lanes of [`residual4_2x8x8`].
#[inline]
pub fn recon4_8x8(pred: &[u8; 64], r: &Lanes4<8>, comp: usize, out: &mut [u8; 64]) {
    let p = pred.as_chunks::<8>().0;
    let o = out.as_chunks_mut::<8>().0;
    for yy in 0..4 {
        let a = i32x4::transpose(std::array::from_fn(|xx| load(&r[yy * 4 + xx], comp * 4)));
        add_clip(&p[yy], [a[0], a[1]], &mut o[yy]);
        add_clip(&p[yy + 4], [a[2], a[3]], &mut o[yy + 4]);
    }
}

/// Reconstruction `clip(pred + r)` of a 16x16 block from the residual lanes of [`residual4_16x16`].
#[inline]
#[cfg(test)]
pub fn recon4_16x16_scalar(pred: &[u8; 256], r: &Lanes4<16>, out: &mut [u8; 256]) {
    for y in 0..16 {
        for x in 0..16 {
            out[y * 16 + x] = (pred[y * 16 + x] as i32 + r[(y & 3) * 4 + (x & 3)][(y >> 2) * 4 + (x >> 2)]).clamp(0, 255) as u8;
        }
    }
}

/// Reconstruction of a 16x16 block from the residual lanes of [`residual8_16x16`].
#[inline]
#[cfg(test)]
pub fn recon8_16x16_scalar(pred: &[u8; 256], r: &Lanes8<4>, out: &mut [u8; 256]) {
    for y in 0..16 {
        for x in 0..16 {
            out[y * 16 + x] = (pred[y * 16 + x] as i32 + r[(y & 7) * 8 + (x & 7)][(y >> 3) * 2 + (x >> 3)]).clamp(0, 255) as u8;
        }
    }
}

/// Reconstruction of one 8x8 chroma block (component `comp`) from the lanes of [`residual4_2x8x8`].
#[inline]
#[cfg(test)]
pub fn recon4_8x8_scalar(pred: &[u8; 64], r: &Lanes4<8>, comp: usize, out: &mut [u8; 64]) {
    for y in 0..8 {
        for x in 0..8 {
            out[y * 8 + x] = (pred[y * 8 + x] as i32 + r[(y & 3) * 4 + (x & 3)][comp * 4 + (y >> 2) * 2 + (x >> 2)]).clamp(0, 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::{dequant4, dequant8, quant4, quant8};

    /// Deterministic generator of test blocks: random, extremes, flat and checkerboard content.
    struct Gen(u64);
    impl Gen {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn pixels<const N: usize>(&mut self, kind: u32) -> [u8; N] {
            let mut out = [0u8; N];
            for (i, o) in out.iter_mut().enumerate() {
                *o = match kind % 6 {
                    0 => self.next() as u8,
                    1 => 0,
                    2 => 255,
                    3 => {
                        if (i + i / 16) % 2 == 0 {
                            255
                        } else {
                            0
                        }
                    }
                    4 => (128 + (self.next() % 7) as i32 - 3) as u8,
                    _ => {
                        if self.next().is_multiple_of(2) {
                            0
                        } else {
                            255
                        }
                    }
                };
            }
            out
        }
    }

    fn pairs<const N: usize>(n: usize) -> Vec<([u8; N], [u8; N])> {
        let mut g = Gen(0x5eed);
        let mut v = Vec::new();
        for i in 0..n as u32 {
            let a = g.pixels::<N>(i);
            let b = g.pixels::<N>(i / 6 + 1);
            v.push((a, b));
            v.push((b, a));
        }
        v
    }

    /// The scalar per-4x4-block pipeline of the encoder, as reference: (levels, reconstruction).
    fn scalar4(src: &[u8; 256], pred: &[u8; 256], qt: &QuantTables, qp: u8, dz: Deadzone) -> ([[i32; 16]; 16], [u8; 256]) {
        let mut lv = [[0i32; 16]; 16];
        let mut rec = [0u8; 256];
        for by in 0..4 {
            for bx in 0..4 {
                let mut d = [0i32; 16];
                for y in 0..4 {
                    for x in 0..4 {
                        let i = (by * 4 + y) * 16 + bx * 4 + x;
                        d[y * 4 + x] = src[i] as i32 - pred[i] as i32;
                    }
                }
                let mut t = fdct4(&d);
                quant4(&mut t, qt, qp, dz, false);
                lv[by * 4 + bx] = t;
                let nz = t.iter().any(|&v| v != 0);
                dequant4(&mut t, qt, qp, false);
                let r = idct4(&t);
                for y in 0..4 {
                    for x in 0..4 {
                        let i = (by * 4 + y) * 16 + bx * 4 + x;
                        // the encoder copies the prediction for an all-zero block
                        rec[i] = if nz { (pred[i] as i32 + r[y * 4 + x]).clamp(0, 255) as u8 } else { pred[i] };
                    }
                }
            }
        }
        (lv, rec)
    }

    fn scalar8(src: &[u8; 256], pred: &[u8; 256], qt: &QuantTables, qp: u8, dz: Deadzone) -> ([[i32; 64]; 4], [u8; 256]) {
        let mut lv = [[0i32; 64]; 4];
        let mut rec = [0u8; 256];
        for b in 0..4 {
            let (ox, oy) = ((b & 1) * 8, (b >> 1) * 8);
            let mut d = [0i32; 64];
            for y in 0..8 {
                for x in 0..8 {
                    d[y * 8 + x] = src[(oy + y) * 16 + ox + x] as i32 - pred[(oy + y) * 16 + ox + x] as i32;
                }
            }
            let mut t = fdct8(&d);
            quant8(&mut t, qt, qp, dz);
            lv[b] = t;
            let nz = t.iter().any(|&v| v != 0);
            dequant8(&mut t, qt, qp);
            let r = idct8(&t);
            for y in 0..8 {
                for x in 0..8 {
                    let i = (oy + y) * 16 + ox + x;
                    rec[i] = if nz { (pred[i] as i32 + r[y * 8 + x]).clamp(0, 255) as u8 } else { pred[i] };
                }
            }
        }
        (lv, rec)
    }

    #[test]
    fn luma4_lanes_match_scalar() {
        let qt = QuantTables::new();
        for (n, (src, pred)) in pairs::<256>(60).iter().enumerate() {
            for qp in [0u8, 1, 12, 17, 26, 33, 40, 51] {
                for dz in [Deadzone::INTRA, Deadzone::INTER] {
                    let (want_lv, want_rec) = scalar4(src, pred, &qt, qp, dz);
                    let mut c = fdct4_lanes(&residual4_16x16(src, pred));
                    quant4_lanes(&mut c, &qt, qp, dz, false);
                    for l in 0..16 {
                        let got: [i32; 16] = std::array::from_fn(|k| c[k][l]);
                        assert_eq!(got, want_lv[l], "case {n} qp {qp} block {l}");
                    }
                    dequant4_lanes(&mut c, &qt, qp, false);
                    let mut rec = [0u8; 256];
                    recon4_16x16(pred, &idct4_lanes(&c), &mut rec);
                    assert_eq!(rec, want_rec, "case {n} qp {qp}");
                }
            }
        }
    }

    #[test]
    fn luma8_lanes_match_scalar() {
        let qt = QuantTables::new();
        for (n, (src, pred)) in pairs::<256>(60).iter().enumerate() {
            for qp in [0u8, 1, 12, 17, 26, 33, 35, 36, 40, 51] {
                for dz in [Deadzone::INTRA, Deadzone::INTER] {
                    let (want_lv, want_rec) = scalar8(src, pred, &qt, qp, dz);
                    let mut c = fdct8_lanes(&residual8_16x16(src, pred));
                    quant8_lanes(&mut c, &qt, qp, dz);
                    for l in 0..4 {
                        let got: [i32; 64] = std::array::from_fn(|k| c[k][l]);
                        assert_eq!(got, want_lv[l], "case {n} qp {qp} block {l}");
                    }
                    dequant8_lanes(&mut c, &qt, qp);
                    let mut rec = [0u8; 256];
                    recon8_16x16(pred, &idct8_lanes(&c), &mut rec);
                    assert_eq!(rec, want_rec, "case {n} qp {qp}");
                }
            }
        }
    }

    #[test]
    fn chroma_lanes_match_scalar() {
        let qt = QuantTables::new();
        let px = pairs::<64>(80);
        for (n, w) in px.windows(2).enumerate() {
            let (su, pu) = &w[0];
            let (sv, pv) = &w[1];
            for qp in [0u8, 7, 20, 29, 39, 51] {
                let mut c = fdct4_lanes(&residual4_2x8x8([su, sv], [pu, pv]));
                let raw = c;
                quant4_lanes(&mut c, &qt, qp, Deadzone::INTER, true);
                for comp in 0..2 {
                    let (s, p) = if comp == 0 { (su, pu) } else { (sv, pv) };
                    for b in 0..4 {
                        let (bx, by) = ((b & 1) * 4, (b >> 1) * 4);
                        let mut d = [0i32; 16];
                        for y in 0..4 {
                            for x in 0..4 {
                                d[y * 4 + x] = s[(by + y) * 8 + bx + x] as i32 - p[(by + y) * 8 + bx + x] as i32;
                            }
                        }
                        let mut t = fdct4(&d);
                        let l = comp * 4 + b;
                        assert_eq!(t, std::array::from_fn(|k| raw[k][l]), "fdct case {n} lane {l}");
                        quant4(&mut t, &qt, qp, Deadzone::INTER, true);
                        assert_eq!(t, std::array::from_fn(|k| c[k][l]), "quant case {n} qp {qp} lane {l}");
                        dequant4(&mut t, &qt, qp, true);
                        let mut dq = c;
                        dequant4_lanes(&mut dq, &qt, qp, true);
                        assert_eq!(t, std::array::from_fn(|k| dq[k][l]), "dequant case {n} qp {qp} lane {l}");
                        let r = idct4(&t);
                        let ri = idct4_lanes(&dq);
                        assert_eq!(r, std::array::from_fn(|k| ri[k][l]), "idct case {n} qp {qp} lane {l}");
                        let mut rec = [0u8; 64];
                        recon4_8x8(p, &ri, comp, &mut rec);
                        for y in 0..4 {
                            for x in 0..4 {
                                let i = (by + y) * 8 + bx + x;
                                assert_eq!(rec[i], (p[i] as i32 + r[y * 4 + x]).clamp(0, 255) as u8);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn gather_and_scatter_match_scalar() {
        let mut g = Gen(99);
        for (n, (a, b)) in pairs::<256>(40).iter().enumerate() {
            assert_eq!(residual4_16x16(a, b), residual4_16x16_scalar(a, b), "case {n}");
            assert_eq!(residual8_16x16(a, b), residual8_16x16_scalar(a, b), "case {n}");
            let (a0, b0) = (a.first_chunk::<64>().unwrap(), b.first_chunk::<64>().unwrap());
            let (a1, b1) = (a.last_chunk::<64>().unwrap(), b.last_chunk::<64>().unwrap());
            assert_eq!(residual4_2x8x8([a0, a1], [b0, b1]), residual4_2x8x8_scalar([a0, a1], [b0, b1]), "case {n}");
            // residuals up to ±600 so the clipping at 0 and 255 is exercised
            let mut r4 = [[0i32; 16]; 16];
            let mut r8 = [[0i32; 4]; 64];
            let mut rc = [[0i32; 8]; 16];
            for v in r4.iter_mut().flatten().chain(r8.iter_mut().flatten()).chain(rc.iter_mut().flatten()) {
                *v = (g.next() % 1201) as i32 - 600;
            }
            let (mut x, mut y) = ([0u8; 256], [0u8; 256]);
            recon4_16x16(a, &r4, &mut x);
            recon4_16x16_scalar(a, &r4, &mut y);
            assert_eq!(x, y, "case {n}");
            recon8_16x16(a, &r8, &mut x);
            recon8_16x16_scalar(a, &r8, &mut y);
            assert_eq!(x, y, "case {n}");
            for comp in 0..2 {
                let (mut x, mut y) = ([0u8; 64], [0u8; 64]);
                recon4_8x8(a0, &rc, comp, &mut x);
                recon4_8x8_scalar(a0, &rc, comp, &mut y);
                assert_eq!(x, y, "case {n} comp {comp}");
            }
        }
    }

    /// The transforms on arbitrary (not only residual-derived) coefficients, including the largest
    /// magnitudes the dequantiser produces.
    #[test]
    fn inverse_lanes_match_scalar_on_wide_coefficients() {
        let mut g = Gen(77);
        for _ in 0..400 {
            let range = 1 + (g.next() % 3) * 20000;
            let mut c4 = [[0i32; 8]; 16];
            let mut c8 = [[0i32; 4]; 64];
            for row in c4.iter_mut() {
                for v in row.iter_mut() {
                    *v = (g.next() % (2 * range + 1)) as i32 - range as i32;
                }
            }
            for row in c8.iter_mut() {
                for v in row.iter_mut() {
                    *v = (g.next() % (2 * range + 1)) as i32 - range as i32;
                }
            }
            let (i4, i8) = (idct4_lanes(&c4), idct8_lanes(&c8));
            for l in 0..8 {
                assert_eq!(idct4(&std::array::from_fn(|k| c4[k][l])), std::array::from_fn(|k| i4[k][l]));
            }
            for l in 0..4 {
                assert_eq!(idct8(&std::array::from_fn(|k| c8[k][l])), std::array::from_fn(|k| i8[k][l]));
            }
        }
    }

    /// `cargo test --release -p filmcraft-h264enc --lib bench_kernels -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_kernels() {
        let qt = QuantTables::new();
        let px = pairs::<256>(8);
        let n = 200_000;
        let time = |name: &str, f: &dyn Fn(&[u8; 256], &[u8; 256]) -> u8| {
            let t = std::time::Instant::now();
            let mut acc = 0u8;
            for i in 0..n {
                let (s, p) = &px[i % px.len()];
                acc = acc.wrapping_add(f(std::hint::black_box(s), std::hint::black_box(p)));
            }
            println!("{name}: {:.1} ns/MB ({acc})", t.elapsed().as_nanos() as f64 / n as f64);
        };
        let (qp, dz) = (30, Deadzone::INTER);
        time("luma4 scalar", &|s, p| scalar4(s, p, &qt, qp, dz).1[37]);
        time("luma4 lanes ", &|s, p| {
            let mut c = fdct4_lanes(&residual4_16x16(s, p));
            quant4_lanes(&mut c, &qt, qp, dz, false);
            dequant4_lanes(&mut c, &qt, qp, false);
            let mut rec = [0u8; 256];
            recon4_16x16(p, &idct4_lanes(&c), &mut rec);
            rec[37]
        });
        time("luma8 scalar", &|s, p| scalar8(s, p, &qt, qp, dz).1[37]);
        time("luma8 lanes ", &|s, p| {
            let mut c = fdct8_lanes(&residual8_16x16(s, p));
            quant8_lanes(&mut c, &qt, qp, dz);
            dequant8_lanes(&mut c, &qt, qp);
            let mut rec = [0u8; 256];
            recon8_16x16(p, &idct8_lanes(&c), &mut rec);
            rec[37]
        });
        time("  residual4 ", &|s, p| residual4_16x16(s, p)[3][5] as u8);
        time("  fdct4     ", &|s, p| fdct4_lanes(&residual4_16x16(s, p))[3][5] as u8);
        time("  residual8 ", &|s, p| residual8_16x16(s, p)[3][2] as u8);
        time("  fdct8     ", &|s, p| fdct8_lanes(&residual8_16x16(s, p))[3][2] as u8);
        time("  idct8     ", &|s, p| idct8_lanes(&residual8_16x16(s, p))[3][2] as u8);
        {
            let lanes: Vec<Lanes8<4>> = px.iter().map(|(s, p)| residual8_16x16(s, p)).collect();
            let t = std::time::Instant::now();
            let mut acc = 0i32;
            for i in 0..n {
                acc = acc.wrapping_add(fdct8_lanes(std::hint::black_box(&lanes[i % lanes.len()]))[3][2]);
            }
            println!("  fdct8 only: {:.1} ns/MB ({acc})", t.elapsed().as_nanos() as f64 / n as f64);
            let t = std::time::Instant::now();
            for i in 0..n {
                let l = std::hint::black_box(&lanes[i % lanes.len()]);
                for b in 0..4 {
                    let blk: [i32; 64] = std::array::from_fn(|k| l[k][b]);
                    acc = acc.wrapping_add(fdct8(std::hint::black_box(&blk))[9]);
                }
            }
            println!("  fdct8 scalar x4: {:.1} ns/MB ({acc})", t.elapsed().as_nanos() as f64 / n as f64);
        }
        time("  quant8    ", &|s, p| {
            let mut c = residual8_16x16(s, p);
            quant8_lanes(&mut c, &qt, qp, dz);
            c[3][2] as u8
        });
        time("  dequant8  ", &|s, p| {
            let mut c = residual8_16x16(s, p);
            dequant8_lanes(&mut c, &qt, qp);
            c[3][2] as u8
        });
        time("  recon8    ", &|s, p| {
            let c = residual8_16x16(s, p);
            let mut rec = [0u8; 256];
            recon8_16x16(p, &c, &mut rec);
            rec[37]
        });
    }
}
