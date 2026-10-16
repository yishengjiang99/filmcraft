//! Distortion metrics: SAD, SATD (4x4 Hadamard), SSD.

use wide::{i16x8, i32x4, u8x16, u16x8};

#[inline]
pub fn sad(a: &[u8], sa: usize, b: &[u8], sb: usize, w: usize, h: usize) -> u32 {
    let mut s = 0u32;
    for r in 0..h {
        let ra = &a[r * sa..r * sa + w];
        let rb = &b[r * sb..r * sb + w];
        s += ra.iter().zip(rb).map(|(&x, &y)| x.abs_diff(y) as u32).sum::<u32>();
    }
    s
}

/// SAD of a 16-wide block (fast path): `|a - b| = max - min` on 16 bytes at a time, widened to `u16` lanes
/// (at most 510 per lane and row, flushed to `u32` every 64 rows). Stops at the first row out of range,
/// like [`sad16_scalar`].
#[inline]
pub fn sad16(a: &[u8], sa: usize, b: &[u8], sb: usize, h: usize) -> u32 {
    let mut total = 0u32;
    let mut acc = u16x8::ZERO;
    for r in 0..h {
        let (Some(ra), Some(rb)) = (a.get(r * sa..).and_then(|x| x.first_chunk::<16>()), b.get(r * sb..).and_then(|x| x.first_chunk::<16>())) else {
            break;
        };
        let (va, vb) = (u8x16::new(*ra), u8x16::new(*rb));
        let d = va.max(vb) - va.min(vb);
        acc += u16x8::from_u8x16_low(d) + u16x8::from_u8x16_high(d);
        if r % 64 == 63 {
            total += acc.to_array().iter().map(|&v| v as u32).sum::<u32>();
            acc = u16x8::ZERO;
        }
    }
    total + acc.to_array().iter().map(|&v| v as u32).sum::<u32>()
}

/// Scalar reference of [`sad16`].
#[cfg(test)]
pub fn sad16_scalar(a: &[u8], sa: usize, b: &[u8], sb: usize, h: usize) -> u32 {
    let mut s = 0u32;
    for r in 0..h {
        let (Some(ra), Some(rb)) = (a.get(r * sa..).and_then(|x| x.first_chunk::<16>()), b.get(r * sb..).and_then(|x| x.first_chunk::<16>())) else {
            break;
        };
        let mut acc = 0u16;
        for i in 0..16 {
            acc += ra[i].abs_diff(rb[i]) as u16;
        }
        s += acc as u32;
    }
    s
}

#[cfg(test)]
fn hadamard4x4_sum(d: &[i32; 16]) -> u32 {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let a0 = d[i * 4] + d[i * 4 + 1];
        let a1 = d[i * 4] - d[i * 4 + 1];
        let a2 = d[i * 4 + 2] + d[i * 4 + 3];
        let a3 = d[i * 4 + 2] - d[i * 4 + 3];
        t[i * 4] = a0 + a2;
        t[i * 4 + 1] = a1 + a3;
        t[i * 4 + 2] = a0 - a2;
        t[i * 4 + 3] = a1 - a3;
    }
    let mut s = 0u32;
    for i in 0..4 {
        let a0 = t[i] + t[4 + i];
        let a1 = t[i] - t[4 + i];
        let a2 = t[8 + i] + t[12 + i];
        let a3 = t[8 + i] - t[12 + i];
        s += (a0 + a2).unsigned_abs() + (a1 + a3).unsigned_abs() + (a0 - a2).unsigned_abs() + (a1 - a3).unsigned_abs();
    }
    s
}

/// SATD of a strip of `N` columns (N/4 side-by-side 4x4 blocks) and 4 rows, computed lane-wise so the compiler
/// can vectorise it. Uses |p+q| + |p-q| = 2 max(|p|, |q|) for the last butterfly, which also absorbs the /2.
#[inline(always)]
fn satd_strip<const N: usize, const H: usize>(a: &[u8], sa: usize, b: &[u8], sb: usize) -> u32 {
    let mut d = [[0i16; N]; 4];
    for r in 0..4 {
        let ra = &a[r * sa..r * sa + N];
        let rb = &b[r * sb..r * sb + N];
        for i in 0..N {
            d[r][i] = ra[i] as i16 - rb[i] as i16;
        }
    }
    let mut v = [[0i16; N]; 4];
    for i in 0..N {
        let s01 = d[0][i] + d[1][i];
        let d01 = d[0][i] - d[1][i];
        let s23 = d[2][i] + d[3][i];
        let d23 = d[2][i] - d[3][i];
        v[0][i] = s01 + s23;
        v[1][i] = s01 - s23;
        v[2][i] = d01 + d23;
        v[3][i] = d01 - d23;
    }
    let mut total = 0u32;
    for row in &v {
        let mut hs = [0i16; H];
        let mut hd = [0i16; H];
        for k in 0..H {
            hs[k] = row[2 * k] + row[2 * k + 1];
            hd[k] = row[2 * k] - row[2 * k + 1];
        }
        for k in 0..H / 2 {
            total += hs[2 * k].unsigned_abs().max(hs[2 * k + 1].unsigned_abs()) as u32;
            total += hd[2 * k].unsigned_abs().max(hd[2 * k + 1].unsigned_abs()) as u32;
        }
    }
    total
}

/// Eight samples of a row as `i16` lanes.
#[inline(always)]
fn row8(r: &[u8; 8]) -> i16x8 {
    i16x8::new(std::array::from_fn(|i| r[i] as i16))
}

/// Vertical 4-point Hadamard of four difference rows (lane-wise).
#[inline(always)]
fn hadamard_v(d: [i16x8; 4]) -> [i16x8; 4] {
    let (s01, d01, s23, d23) = (d[0] + d[1], d[0] - d[1], d[2] + d[3], d[2] - d[3]);
    [s01 + s23, s01 - s23, d01 + d23, d01 - d23]
}

/// Horizontal stage of two 4x4 blocks per vector, after an 8x8 transpose put column `x` of eight
/// vertically transformed rows into `t[x]`: the first butterfly lane-wise, the last one as
/// `|p+q| + |p-q| = 2 max(|p|, |q|)` (which also halves), then widened and summed in pairs.
#[inline(always)]
fn hadamard_h_sum(t: [i16x8; 8]) -> i32x4 {
    let half = |a: i16x8, b: i16x8, c: i16x8, e: i16x8| {
        let (s0, d0, s1, d1) = (a + b, a - b, c + e, c - e);
        s0.abs().max(s1.abs()) + d0.abs().max(d1.abs())
    };
    // |vertical| <= 1020, so every term is <= 4080 and the sum of the two halves fits i16
    (half(t[0], t[1], t[2], t[3]) + half(t[4], t[5], t[6], t[7])).dot(i16x8::splat(1))
}

/// SATD of a 16-wide block, `h` a multiple of 4; `None` if a row is out of range.
#[inline]
fn satd_w16(a: &[u8], sa: usize, b: &[u8], sb: usize, h: usize) -> Option<u32> {
    let mut acc = i32x4::ZERO;
    for by in (0..h).step_by(4) {
        let mut lo = [i16x8::ZERO; 4];
        let mut hi = [i16x8::ZERO; 4];
        for r in 0..4 {
            let ra = a.get((by + r) * sa..)?.first_chunk::<16>()?;
            let rb = b.get((by + r) * sb..)?.first_chunk::<16>()?;
            let (ra, rb) = (ra.as_chunks::<8>().0, rb.as_chunks::<8>().0);
            lo[r] = row8(&ra[0]) - row8(&rb[0]);
            hi[r] = row8(&ra[1]) - row8(&rb[1]);
        }
        let (l, h) = (hadamard_v(lo), hadamard_v(hi));
        acc += hadamard_h_sum(i16x8::transpose([l[0], l[1], l[2], l[3], h[0], h[1], h[2], h[3]]));
    }
    Some(acc.reduce_add() as u32)
}

/// SATD of an 8-wide block, `h` a multiple of 8; `None` if a row is out of range.
#[inline]
fn satd_w8(a: &[u8], sa: usize, b: &[u8], sb: usize, h: usize) -> Option<u32> {
    let mut acc = i32x4::ZERO;
    for by in (0..h).step_by(8) {
        let mut d = [i16x8::ZERO; 8];
        for (r, d) in d.iter_mut().enumerate() {
            let ra = a.get((by + r) * sa..)?.first_chunk::<8>()?;
            let rb = b.get((by + r) * sb..)?.first_chunk::<8>()?;
            *d = row8(ra) - row8(rb);
        }
        let (t, u) = (hadamard_v([d[0], d[1], d[2], d[3]]), hadamard_v([d[4], d[5], d[6], d[7]]));
        acc += hadamard_h_sum(i16x8::transpose([t[0], t[1], t[2], t[3], u[0], u[1], u[2], u[3]]));
    }
    Some(acc.reduce_add() as u32)
}

/// Sum of absolute 4x4 Hadamard-transformed differences (halved), for w, h multiples of 4.
/// 16- and 8-wide blocks use `wide` SIMD kernels, identical to [`satd_scalar`] (which handles the rest).
#[inline]
pub fn satd(a: &[u8], sa: usize, b: &[u8], sb: usize, w: usize, h: usize) -> u32 {
    let fast = match w {
        16 if h.is_multiple_of(4) => satd_w16(a, sa, b, sb, h),
        8 if h.is_multiple_of(8) => satd_w8(a, sa, b, sb, h),
        _ => None,
    };
    match fast {
        Some(v) => v,
        None => satd_scalar(a, sa, b, sb, w, h),
    }
}

/// Scalar SATD (strip-wise): the reference for [`satd`] and its path for other sizes.
pub fn satd_scalar(a: &[u8], sa: usize, b: &[u8], sb: usize, w: usize, h: usize) -> u32 {
    let mut total = 0;
    for by in (0..h).step_by(4) {
        let (ao, bo) = (by * sa, by * sb);
        match w {
            16 => total += satd_strip::<16, 8>(&a[ao..], sa, &b[bo..], sb),
            8 => total += satd_strip::<8, 4>(&a[ao..], sa, &b[bo..], sb),
            4 => total += satd_strip::<4, 2>(&a[ao..], sa, &b[bo..], sb),
            _ => {
                for bx in (0..w).step_by(4) {
                    total += satd_strip::<4, 2>(&a[ao + bx..], sa, &b[bo + bx..], sb);
                }
            }
        }
    }
    total
}

/// Sum of squared differences. On wasm, 16-wide blocks use [`ssd_simd`] (1.7x faster there); natively the
/// scalar loop already vectorises better (widening multiplies) than the `wide` version.
#[inline]
pub fn ssd(a: &[u8], sa: usize, b: &[u8], sb: usize, w: usize, h: usize) -> u64 {
    #[cfg(target_arch = "wasm32")]
    if let Some(v) = ssd_simd(a, sa, b, sb, w, h) {
        return v;
    }
    ssd_scalar(a, sa, b, sb, w, h)
}

/// SSD of a 16-wide block of up to 64 rows with `wide`: differences in `i16`, squared and pair-summed by
/// `dot` into `i32` lanes (at most 2·255² per lane and row); `None` for other sizes or rows out of range.
#[inline]
#[cfg_attr(not(any(test, target_arch = "wasm32")), allow(dead_code))]
pub fn ssd_simd(a: &[u8], sa: usize, b: &[u8], sb: usize, w: usize, h: usize) -> Option<u64> {
    if w != 16 || h > 64 {
        return None;
    }
    let mut acc = i32x4::ZERO;
    for r in 0..h {
        let ra = a.get(r * sa..)?.first_chunk::<16>()?;
        let rb = b.get(r * sb..)?.first_chunk::<16>()?;
        let (ra, rb) = (ra.as_chunks::<8>().0, rb.as_chunks::<8>().0);
        let lo = row8(&ra[0]) - row8(&rb[0]);
        let hi = row8(&ra[1]) - row8(&rb[1]);
        acc += lo.dot(lo) + hi.dot(hi);
    }
    Some(acc.to_array().iter().map(|&v| v as u64).sum())
}

/// Scalar SSD: the reference for [`ssd`] and its path for other sizes.
pub fn ssd_scalar(a: &[u8], sa: usize, b: &[u8], sb: usize, w: usize, h: usize) -> u64 {
    let mut s = 0u64;
    for r in 0..h {
        let ra = &a[r * sa..r * sa + w];
        let rb = &b[r * sb..r * sb + w];
        s += ra.iter().zip(rb).map(|(&x, &y)| (x as i32 - y as i32).pow(2) as u32).sum::<u32>() as u64;
    }
    s
}

/// Variance*N (sum of squares minus square of sum / N) of a block.
pub fn block_var(a: &[u8], sa: usize, w: usize, h: usize) -> u32 {
    let mut s = 0u32;
    let mut ss = 0u32;
    for r in 0..h {
        for &v in &a[r * sa..r * sa + w] {
            s += v as u32;
            ss += (v as u32) * (v as u32);
        }
    }
    ss - ((s as u64 * s as u64) / (w * h) as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn satd_of_dc_difference() {
        let a = [10u8; 16];
        let b = [0u8; 16];
        // DC-only difference of 10: hadamard DC = 160, halved = 80.
        assert_eq!(satd(&a, 4, &b, 4, 4, 4), 80);
        assert_eq!(sad(&a, 4, &b, 4, 4, 4), 160);
    }

    #[test]
    fn satd_matches_reference() {
        let mut seed = 7u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            (seed >> 16) as u8
        };
        let a: Vec<u8> = (0..16 * 16).map(|_| rnd()).collect();
        let b: Vec<u8> = (0..16 * 16).map(|_| rnd()).collect();
        for (w, h) in [(16, 16), (8, 8), (4, 4), (16, 8), (8, 16), (12, 4)] {
            let mut want = 0;
            for by in (0..h).step_by(4) {
                for bx in (0..w).step_by(4) {
                    let mut d = [0i32; 16];
                    for r in 0..4 {
                        for c in 0..4 {
                            d[r * 4 + c] = a[(by + r) * 16 + bx + c] as i32 - b[(by + r) * 16 + bx + c] as i32;
                        }
                    }
                    want += hadamard4x4_sum(&d);
                }
            }
            assert_eq!(satd(&a, 16, &b, 16, w, h), want / 2, "{w}x{h}");
        }
    }
    /// Test planes with odd strides: random, 0/255 extremes, checkerboards and near-flat content.
    fn planes(seed: u32, stride: usize, rows: usize) -> Vec<Vec<u8>> {
        let mut st = seed;
        let mut rnd = move || {
            st = st.wrapping_mul(1103515245).wrapping_add(12345);
            st >> 8
        };
        let n = stride * rows;
        let mut out = Vec::new();
        for kind in 0..6 {
            out.push(
                (0..n)
                    .map(|i| match kind {
                        0 => rnd() as u8,
                        1 => 0,
                        2 => 255,
                        3 => (((i % stride) + i / stride) % 2 * 255) as u8,
                        4 => (126 + rnd() % 5) as u8,
                        _ => (rnd() % 2 * 255) as u8,
                    })
                    .collect(),
            );
        }
        out
    }

    #[test]
    fn satd_kernel_matches_scalar() {
        let (sa, sb) = (37, 23);
        let pa = planes(1, sa, 40);
        let pb = planes(2, sb, 40);
        for (i, a) in pa.iter().enumerate() {
            for (j, b) in pb.iter().enumerate() {
                for (w, h) in [(16, 16), (16, 8), (8, 16), (8, 8), (4, 4), (4, 8), (12, 4)] {
                    for (oa, ob) in [(0, 0), (sa + 3, 5), (2 * sa + 21, sb + 7)] {
                        let want = satd_scalar(&a[oa..], sa, &b[ob..], sb, w, h);
                        assert_eq!(satd(&a[oa..], sa, &b[ob..], sb, w, h), want, "{w}x{h} planes {i}/{j} offsets {oa}/{ob}");
                    }
                }
            }
        }
        for (i, a) in pa.iter().enumerate() {
            for (j, b) in pb.iter().enumerate() {
                for h in [1, 4, 8, 16, 37] {
                    for (oa, ob) in [(0, 0), (sa + 3, 5), (2 * sa + 21, sb + 7)] {
                        let want = sad16_scalar(&a[oa..], sa, &b[ob..], sb, h);
                        assert_eq!(sad16(&a[oa..], sa, &b[ob..], sb, h), want, "sad16 h {h} planes {i}/{j}");
                    }
                }
            }
        }
        for (i, a) in pa.iter().enumerate() {
            for (j, b) in pb.iter().enumerate() {
                for (w, h) in [(16, 16), (16, 8), (16, 37), (8, 8), (4, 4)] {
                    for (oa, ob) in [(0, 0), (sa + 3, 5), (2 * sa + 21, sb + 7)] {
                        let want = ssd_scalar(&a[oa..], sa, &b[ob..], sb, w, h);
                        assert_eq!(ssd(&a[oa..], sa, &b[ob..], sb, w, h), want, "ssd {w}x{h} planes {i}/{j}");
                        if let Some(v) = ssd_simd(&a[oa..], sa, &b[ob..], sb, w, h) {
                            assert_eq!(v, want, "ssd_simd {w}x{h} planes {i}/{j}");
                        }
                    }
                }
            }
        }
        // a 200-row block exercises the u16 flush; rows past the end stop the sum like the scalar code
        let tall: Vec<u8> = (0..16 * 200).map(|i| if i % 3 == 0 { 255 } else { 0 }).collect();
        let zero = vec![0u8; 16 * 200];
        assert_eq!(sad16(&tall, 16, &zero, 16, 200), sad16_scalar(&tall, 16, &zero, 16, 200));
        assert_eq!(sad16(&tall, 16, &zero, 16, 260), sad16_scalar(&tall, 16, &zero, 16, 260));
        // the largest possible block sum (alternating 0/255 against its inverse) stays exact
        let a: Vec<u8> = (0..256).map(|i| ((i % 16 + i / 16) % 2 * 255) as u8).collect();
        let b: Vec<u8> = a.iter().map(|&v| 255 - v).collect();
        assert_eq!(satd(&a, 16, &b, 16, 16, 16), satd_scalar(&a, 16, &b, 16, 16, 16));
    }

    /// `cargo test --release -p filmcraft-h264enc --lib bench_dsp -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_dsp() {
        let pa = planes(3, 64, 64);
        let (a, b) = (&pa[0], &pa[4]);
        let time = |name: &str, f: &dyn Fn(usize) -> u32| {
            let n = 2_000_000;
            let t = std::time::Instant::now();
            let mut acc = 0u32;
            for i in 0..n {
                acc = acc.wrapping_add(f(std::hint::black_box(i & 31)));
            }
            println!("{name}: {:.1} ns/call ({acc})", t.elapsed().as_nanos() as f64 / n as f64);
        };
        time("satd 16x16 scalar", &|o| satd_scalar(&a[o..], 64, &b[o * 2..], 64, 16, 16));
        time("satd 16x16 simd  ", &|o| satd(&a[o..], 64, &b[o * 2..], 64, 16, 16));
        time("satd 8x8 scalar  ", &|o| satd_scalar(&a[o..], 64, &b[o * 2..], 64, 8, 8));
        time("satd 8x8 simd    ", &|o| satd(&a[o..], 64, &b[o * 2..], 64, 8, 8));
        time("satd 16x8 scalar ", &|o| satd_scalar(&a[o..], 64, &b[o * 2..], 64, 16, 8));
        time("satd 16x8 simd   ", &|o| satd(&a[o..], 64, &b[o * 2..], 64, 16, 8));
        time("satd 8x16 scalar ", &|o| satd_scalar(&a[o..], 64, &b[o * 2..], 64, 8, 16));
        time("satd 8x16 simd   ", &|o| satd(&a[o..], 64, &b[o * 2..], 64, 8, 16));
        time("ssd 16x16 scalar ", &|o| ssd_scalar(&a[o..], 64, &b[o * 2..], 64, 16, 16) as u32);
        time("ssd 16x16 simd   ", &|o| ssd_simd(&a[o..], 64, &b[o * 2..], 64, 16, 16).unwrap_or(0) as u32);
        time("sad16 16x16 scal ", &|o| sad16_scalar(&a[o..], 64, &b[o * 2..], 64, 16));
        time("sad16 16x16 simd ", &|o| sad16(&a[o..], 64, &b[o * 2..], 64, 16));
    }
}
