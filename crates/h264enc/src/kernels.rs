//! Lane-parallel ("structure of arrays") residual kernels.
//!
//! The macroblock residual pipeline (residual → forward transform → quantisation → dequantisation → inverse
//! transform → reconstruction) runs on several independent blocks at once: a value array `v[pos][lane]` holds
//! coefficient `pos` (raster order inside the block) of block `lane`. Every step is then a loop over lanes
//! whose body is the scalar per-block code, with unit-stride accesses only, which LLVM vectorises to 128-bit
//! SIMD (wasm `simd128`, NEON, SSE2) without any `unsafe` or target-specific code.
//!
//! Each kernel performs exactly the integer operations of the scalar functions in [`crate::transform`]
//! (which stay the reference), so results are bit-identical; the tests below check that on random and
//! extreme blocks.

use crate::transform::{Deadzone, QuantTables, fdct4, fdct8, idct4, idct8};

/// `L` blocks of 16 coefficients, `[pos][lane]`.
pub type Lanes4<const L: usize> = [[i32; L]; 16];
/// `L` blocks of 64 coefficients, `[pos][lane]`.
pub type Lanes8<const L: usize> = [[i32; L]; 64];

/// Residual `src - pred` of the sixteen 4x4 blocks of a 16x16 block (stride 16); lane = `by * 4 + bx`.
#[inline]
pub fn residual4_16x16(src: &[u8; 256], pred: &[u8; 256]) -> Lanes4<16> {
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
pub fn residual4_2x8x8(src: [&[u8; 64]; 2], pred: [&[u8; 64]; 2]) -> Lanes4<8> {
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
pub fn residual8_16x16(src: &[u8; 256], pred: &[u8; 256]) -> Lanes8<4> {
    let mut d = [[0i32; 4]; 64];
    for y in 0..16 {
        for x in 0..16 {
            d[(y & 7) * 8 + (x & 7)][(y >> 3) * 2 + (x >> 3)] = src[y * 16 + x] as i32 - pred[y * 16 + x] as i32;
        }
    }
    d
}

/// [`fdct4`] of every lane.
#[inline]
pub fn fdct4_lanes<const L: usize>(d: &Lanes4<L>) -> Lanes4<L> {
    let mut o = [[0i32; L]; 16];
    for l in 0..L {
        let t = fdct4(&std::array::from_fn(|k| d[k][l]));
        for k in 0..16 {
            o[k][l] = t[k];
        }
    }
    o
}

/// [`idct4`] of every lane.
#[inline]
pub fn idct4_lanes<const L: usize>(d: &Lanes4<L>) -> Lanes4<L> {
    let mut o = [[0i32; L]; 16];
    for l in 0..L {
        let t = idct4(&std::array::from_fn(|k| d[k][l]));
        for k in 0..16 {
            o[k][l] = t[k];
        }
    }
    o
}

/// [`fdct8`] of every lane.
#[inline]
pub fn fdct8_lanes<const L: usize>(d: &Lanes8<L>) -> Lanes8<L> {
    let mut o = [[0i32; L]; 64];
    for l in 0..L {
        let t = fdct8(&std::array::from_fn(|k| d[k][l]));
        for k in 0..64 {
            o[k][l] = t[k];
        }
    }
    o
}

/// [`idct8`] of every lane.
#[inline]
pub fn idct8_lanes<const L: usize>(d: &Lanes8<L>) -> Lanes8<L> {
    let mut o = [[0i32; L]; 64];
    for l in 0..L {
        let t = idct8(&std::array::from_fn(|k| d[k][l]));
        for k in 0..64 {
            o[k][l] = t[k];
        }
    }
    o
}

/// Deadzone quantisation of one coefficient row (all lanes share the multiplier): `sign(v) · ((|v|·mf + f) >> qbits)`,
/// as [`crate::transform::quant4`] / [`crate::transform::quant8`] (all terms are non-negative, so unsigned 64-bit
/// arithmetic gives the same value as their `i64` arithmetic).
#[inline(always)]
fn quant_row<const L: usize>(row: &mut [i32; L], mf: u64, f: u64, qbits: u32) {
    for v in row.iter_mut() {
        let a = ((u64::from(v.unsigned_abs()) * mf + f) >> qbits) as i32;
        *v = if *v < 0 { -a } else { a };
    }
}

/// [`crate::transform::quant4`] of every lane (without the non-zero count).
#[inline]
pub fn quant4_lanes<const L: usize>(c: &mut Lanes4<L>, qt: &QuantTables, qp: u8, dz: Deadzone, skip_dc: bool) {
    let qbits = 15 + (qp / 6) as u32;
    let f = ((1i64 << qbits) * dz.f256) >> 8;
    let mf = &qt.mf4[(qp % 6) as usize];
    let start = if skip_dc { 1 } else { 0 };
    for k in start..16 {
        quant_row(&mut c[k], mf[k] as u64, f as u64, qbits);
    }
}

/// [`crate::transform::quant8`] of every lane (without the non-zero count).
#[inline]
pub fn quant8_lanes<const L: usize>(c: &mut Lanes8<L>, qt: &QuantTables, qp: u8, dz: Deadzone) {
    let qbits = 22 + (qp / 6) as u32;
    let f = ((1i64 << qbits) * dz.f256) >> 8;
    let mf = &qt.mf8[(qp % 6) as usize];
    for k in 0..64 {
        quant_row(&mut c[k], mf[k] as u64, f as u64, qbits);
    }
}

/// [`crate::transform::dequant4`] of every lane.
#[inline]
pub fn dequant4_lanes<const L: usize>(c: &mut Lanes4<L>, qt: &QuantTables, qp: u8, skip_dc: bool) {
    let v = &qt.v4[(qp % 6) as usize];
    let sh = (qp / 6) as u32;
    let start = if skip_dc { 1 } else { 0 };
    for k in start..16 {
        for x in c[k].iter_mut() {
            *x = (*x * v[k]) << sh;
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
        if q6 >= 6 {
            for x in c[k].iter_mut() {
                *x = (*x * ls) << (q6 - 6);
            }
        } else {
            let (add, sh) = (1 << (5 - q6), 6 - q6);
            for x in c[k].iter_mut() {
                *x = (*x * ls + add) >> sh;
            }
        }
    }
}

/// Reconstruction `clip(pred + r)` of a 16x16 block from the residual lanes of [`residual4_16x16`].
#[inline]
pub fn recon4_16x16(pred: &[u8; 256], r: &Lanes4<16>, out: &mut [u8; 256]) {
    for y in 0..16 {
        for x in 0..16 {
            out[y * 16 + x] = (pred[y * 16 + x] as i32 + r[(y & 3) * 4 + (x & 3)][(y >> 2) * 4 + (x >> 2)]).clamp(0, 255) as u8;
        }
    }
}

/// Reconstruction of a 16x16 block from the residual lanes of [`residual8_16x16`].
#[inline]
pub fn recon8_16x16(pred: &[u8; 256], r: &Lanes8<4>, out: &mut [u8; 256]) {
    for y in 0..16 {
        for x in 0..16 {
            out[y * 16 + x] = (pred[y * 16 + x] as i32 + r[(y & 7) * 8 + (x & 7)][(y >> 3) * 2 + (x >> 3)]).clamp(0, 255) as u8;
        }
    }
}

/// Reconstruction of one 8x8 chroma block (component `comp`) from the lanes of [`residual4_2x8x8`].
#[inline]
pub fn recon4_8x8(pred: &[u8; 64], r: &Lanes4<8>, comp: usize, out: &mut [u8; 64]) {
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
}
