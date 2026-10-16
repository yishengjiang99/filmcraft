//! Padded picture planes, reference pictures and motion-compensated prediction (§8.4.2.2).

use crate::mbinfo::{MbInfo, Mv};

pub const LUMA_PAD: usize = 64;
pub const CHROMA_PAD: usize = 32;

/// A plane with `pad` replicated samples on every side.
#[derive(Clone)]
pub struct Plane {
    pub data: Vec<u8>,
    pub stride: usize,
    pub w: usize,
    pub h: usize,
    pub pad: usize,
}

impl Plane {
    pub fn new(w: usize, h: usize, pad: usize) -> Self {
        let stride = w + 2 * pad;
        Plane { data: vec![0; stride * (h + 2 * pad)], stride, w, h, pad }
    }
    #[inline]
    pub fn idx(&self, x: isize, y: isize) -> usize {
        ((y + self.pad as isize) as usize) * self.stride + (x + self.pad as isize) as usize
    }
    /// Replicate edge samples into the padding.
    pub fn extend_edges(&mut self) {
        let (w, h, pad, stride) = (self.w, self.h, self.pad, self.stride);
        for y in 0..h {
            let r = (y + pad) * stride;
            let l = self.data[r + pad];
            let rr = self.data[r + pad + w - 1];
            self.data[r..r + pad].fill(l);
            self.data[r + pad + w..r + stride].fill(rr);
        }
        let first = pad * stride;
        let last = (pad + h - 1) * stride;
        for y in 0..pad {
            self.data.copy_within(first..first + stride, y * stride);
            self.data.copy_within(last..last + stride, (pad + h + y) * stride);
        }
    }
    /// Copy the picture area out as a tightly packed plane of `cw x ch` (cropping).
    pub fn copy_out(&self, cw: usize, ch: usize, out: &mut Vec<u8>) {
        for y in 0..ch {
            let o = self.idx(0, y as isize);
            out.extend_from_slice(&self.data[o..o + cw]);
        }
    }
}

/// A 4:2:0 frame with padded planes.
#[derive(Clone)]
pub struct Frame {
    pub y: Plane,
    pub u: Plane,
    pub v: Plane,
}

impl Frame {
    pub fn new(w: usize, h: usize) -> Self {
        Frame { y: Plane::new(w, h, LUMA_PAD), u: Plane::new(w / 2, h / 2, CHROMA_PAD), v: Plane::new(w / 2, h / 2, CHROMA_PAD) }
    }
    pub fn extend_edges(&mut self) {
        self.y.extend_edges();
        self.u.extend_edges();
        self.v.extend_edges();
    }
}

/// A decoded reference picture with half-sample planes for fast sub-pel access.
pub struct RefPic {
    pub frame: Frame,
    /// Horizontal half (b), vertical half (h) and centre (j) planes, same geometry as luma.
    pub hpel: [Plane; 3],
    pub mbs: Vec<MbInfo>,
    pub poc: i32,
    pub id: u32,
}

#[inline]
fn clip8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Compute the H, V and centre half-sample planes over the whole padded area (clamped at the buffer edge,
/// which equals the normative picture-edge clamping because the padding is replicated).
pub fn build_hpel(f: &Plane, reuse: Option<[Plane; 3]>) -> [Plane; 3] {
    let [mut hp, mut vp, mut cp] = reuse.filter(|r| r[0].data.len() == f.data.len()).unwrap_or_else(|| {
        let mk = || Plane { data: vec![0; f.data.len()], stride: f.stride, w: f.w, h: f.h, pad: f.pad };
        [mk(), mk(), mk()]
    });
    let rows = f.data.len() / f.stride;
    let band = 16usize;
    let work = |b: usize, hrows: &mut [u8], vrows: &mut [u8], crows: &mut [u8]| {
        let r0 = b * band;
        let r1 = (r0 + band).min(rows);
        hpel_rows(f, r0, r1, hrows, vrows, crows);
    };
    let chunk = band * f.stride;
    #[cfg(feature = "threads")]
    {
        use rayon::prelude::*;
        hp.data
            .par_chunks_mut(chunk)
            .zip(vp.data.par_chunks_mut(chunk))
            .zip(cp.data.par_chunks_mut(chunk))
            .enumerate()
            .for_each(|(b, ((h, v), c))| work(b, h, v, c));
    }
    #[cfg(not(feature = "threads"))]
    {
        for (b, ((h, v), c)) in hp.data.chunks_mut(chunk).zip(vp.data.chunks_mut(chunk)).zip(cp.data.chunks_mut(chunk)).enumerate() {
            work(b, h, v, c);
        }
    }
    [hp, vp, cp]
}

fn hpel_rows(f: &Plane, r0: usize, r1: usize, hout: &mut [u8], vout: &mut [u8], cout: &mut [u8]) {
    let stride = f.stride;
    let rows = f.data.len() / stride;
    let clampr = |r: isize| r.clamp(0, rows as isize - 1) as usize;
    // Horizontal intermediate (unclipped, unshifted) for rows r0-2 .. r1+3.
    let nrows = r1 - r0 + 5;
    let mut hs = vec![0i16; nrows * stride];
    let mut ext = vec![0u8; stride + 5];
    for k in 0..nrows {
        let r = clampr(r0 as isize - 2 + k as isize);
        let row = &f.data[r * stride..r * stride + stride];
        ext[0] = row[0];
        ext[1] = row[0];
        ext[2..2 + stride].copy_from_slice(row);
        ext[stride + 2] = row[stride - 1];
        ext[stride + 3] = row[stride - 1];
        ext[stride + 4] = row[stride - 1];
        let o = &mut hs[k * stride..k * stride + stride];
        for x in 0..stride {
            let e = &ext[x..x + 6];
            o[x] = (e[0] as i16 + e[5] as i16) - 5 * (e[1] as i16 + e[4] as i16) + 20 * (e[2] as i16 + e[3] as i16);
        }
    }
    for r in r0..r1 {
        let lo = (r - r0) * stride;
        let k = r - r0 + 2;
        let hrow = &hs[k * stride..k * stride + stride];
        for x in 0..stride {
            hout[lo + x] = clip8((hrow[x] as i32 + 16) >> 5);
        }
        // vertical
        let rr: [usize; 6] = std::array::from_fn(|i| clampr(r as isize - 2 + i as isize) * stride);
        for x in 0..stride {
            let s = f.data[rr[0] + x] as i32 + f.data[rr[5] + x] as i32 - 5 * (f.data[rr[1] + x] as i32 + f.data[rr[4] + x] as i32)
                + 20 * (f.data[rr[2] + x] as i32 + f.data[rr[3] + x] as i32);
            vout[lo + x] = clip8((s + 16) >> 5);
        }
        // centre from horizontal intermediates
        let hk: [&[i16]; 6] = std::array::from_fn(|i| &hs[(k - 2 + i) * stride..(k - 1 + i) * stride]);
        for x in 0..stride {
            let s = hk[0][x] as i32 + hk[5][x] as i32 - 5 * (hk[1][x] as i32 + hk[4][x] as i32) + 20 * (hk[2][x] as i32 + hk[3][x] as i32);
            cout[lo + x] = clip8((s + 512) >> 10);
        }
    }
}

/// Plane selection per quarter-sample position: two (plane, dx, dy) sources to average (identical = no average).
/// Plane indices: 0 full, 1 horizontal half, 2 vertical half, 3 centre.
const QPEL_SRC: [[(u8, u8, u8); 2]; 16] = [
    // fy = 0
    [(0, 0, 0), (0, 0, 0)],
    [(0, 0, 0), (1, 0, 0)],
    [(1, 0, 0), (1, 0, 0)],
    [(1, 0, 0), (0, 1, 0)],
    // fy = 1
    [(0, 0, 0), (2, 0, 0)],
    [(1, 0, 0), (2, 0, 0)],
    [(1, 0, 0), (3, 0, 0)],
    [(1, 0, 0), (2, 1, 0)],
    // fy = 2
    [(2, 0, 0), (2, 0, 0)],
    [(2, 0, 0), (3, 0, 0)],
    [(3, 0, 0), (3, 0, 0)],
    [(3, 0, 0), (2, 1, 0)],
    // fy = 3
    [(2, 0, 0), (0, 0, 1)],
    [(2, 0, 0), (1, 0, 1)],
    [(3, 0, 0), (1, 0, 1)],
    [(2, 1, 0), (1, 0, 1)],
];

/// Rows of a quarter-sample luma prediction: a copy of plane `a` from `ia`, or the rounded average of `a` and `b`
/// (from `ib`), for `W`-wide rows. Same arithmetic as [`mc_rows_scalar`].
#[inline(always)]
fn mc_rows<const W: usize>(a: &[u8], ia: usize, b: Option<(&[u8], usize)>, stride: usize, h: usize, out: &mut [u8], ostride: usize) {
    for r in 0..h {
        let ra = a.get(ia + r * stride..).and_then(|x| x.first_chunk::<W>());
        let o = out.get_mut(r * ostride..).and_then(|x| x.first_chunk_mut::<W>());
        let (Some(ra), Some(o)) = (ra, o) else {
            return mc_rows_scalar(a, ia, b, stride, W, h, out, ostride);
        };
        match b {
            None => *o = *ra,
            Some((b, ib)) => {
                let Some(rb) = b.get(ib + r * stride..).and_then(|x| x.first_chunk::<W>()) else {
                    return mc_rows_scalar(a, ia, Some((b, ib)), stride, W, h, out, ostride);
                };
                for i in 0..W {
                    o[i] = ((ra[i] as u16 + rb[i] as u16 + 1) >> 1) as u8;
                }
            }
        }
    }
}

/// Scalar reference of [`mc_rows`] for any width.
#[allow(clippy::too_many_arguments)]
fn mc_rows_scalar(a: &[u8], ia: usize, b: Option<(&[u8], usize)>, stride: usize, w: usize, h: usize, out: &mut [u8], ostride: usize) {
    for r in 0..h {
        let ra = &a[ia + r * stride..ia + r * stride + w];
        let o = &mut out[r * ostride..r * ostride + w];
        match b {
            None => o.copy_from_slice(ra),
            Some((b, ib)) => {
                let rb = &b[ib + r * stride..ib + r * stride + w];
                for i in 0..w {
                    o[i] = ((ra[i] as u16 + rb[i] as u16 + 1) >> 1) as u8;
                }
            }
        }
    }
}

impl RefPic {
    #[inline]
    fn plane(&self, p: u8) -> &Plane {
        match p {
            0 => &self.frame.y,
            n => &self.hpel[n as usize - 1],
        }
    }

    /// Luma prediction of a w x h block at picture position (x, y) displaced by `mv` (quarter samples).
    pub fn mc_luma(&self, x: usize, y: usize, mv: Mv, w: usize, h: usize, out: &mut [u8], ostride: usize) {
        let xi = x as isize + (mv.x as isize >> 2);
        let yi = y as isize + (mv.y as isize >> 2);
        let q = ((mv.y & 3) * 4 + (mv.x & 3)) as usize;
        let [(pa, dxa, dya), (pb, dxb, dyb)] = QPEL_SRC[q];
        let a = self.plane(pa);
        let stride = a.stride;
        let ia = a.idx(xi + dxa as isize, yi + dya as isize);
        let b = self.plane(pb);
        let ib = if pa == pb && dxa == dxb && dya == dyb { None } else { Some(b.idx(xi + dxb as isize, yi + dyb as isize)) };
        // fixed-width rows for the common widths: inlined copies and a vectorised rounding average
        match w {
            16 => mc_rows::<16>(&a.data, ia, ib.map(|i| (&b.data[..], i)), stride, h, out, ostride),
            8 => mc_rows::<8>(&a.data, ia, ib.map(|i| (&b.data[..], i)), stride, h, out, ostride),
            4 => mc_rows::<4>(&a.data, ia, ib.map(|i| (&b.data[..], i)), stride, h, out, ostride),
            _ => mc_rows_scalar(&a.data, ia, ib.map(|i| (&b.data[..], i)), stride, w, h, out, ostride),
        }
    }

    /// Chroma prediction (both planes) of a w x h chroma block at chroma position (x, y); mv in luma quarter units.
    pub fn mc_chroma(&self, x: usize, y: usize, mv: Mv, w: usize, h: usize, out_u: &mut [u8], out_v: &mut [u8], ostride: usize) {
        let xi = x as isize + (mv.x as isize >> 3);
        let yi = y as isize + (mv.y as isize >> 3);
        let fx = (mv.x & 7) as u32;
        let fy = (mv.y & 7) as u32;
        let w00 = (8 - fx) * (8 - fy);
        let w01 = fx * (8 - fy);
        let w10 = (8 - fx) * fy;
        let w11 = fx * fy;
        for (p, out) in [(&self.frame.u, out_u), (&self.frame.v, out_v)] {
            let s = p.stride;
            let i0 = p.idx(xi, yi);
            for r in 0..h {
                let a = &p.data[i0 + r * s..i0 + r * s + w + 1];
                let b = &p.data[i0 + (r + 1) * s..i0 + (r + 1) * s + w + 1];
                let o = &mut out[r * ostride..r * ostride + w];
                for i in 0..w {
                    o[i] = ((w00 * a[i] as u32 + w01 * a[i + 1] as u32 + w10 * b[i] as u32 + w11 * b[i + 1] as u32 + 32) >> 6) as u8;
                }
            }
        }
    }
}

/// Average two predictions in place (default bi-prediction).
#[inline]
pub fn avg_into(dst: &mut [u8], src: &[u8]) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = ((*d as u16 + s as u16 + 1) >> 1) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Random planes with runs of 0 and 255 (extremes for the clipping).
    fn plane(w: usize, h: usize, pad: usize, seed: u32) -> Plane {
        let mut p = Plane::new(w, h, pad);
        let mut st = seed;
        for v in p.data.iter_mut() {
            st = st.wrapping_mul(1103515245).wrapping_add(12345);
            *v = match (st >> 24) % 4 {
                0 => 0,
                1 => 255,
                _ => (st >> 16) as u8,
            };
        }
        p
    }

    #[test]
    fn mc_rows_match_scalar() {
        let (a, b) = (plane(48, 32, 8, 5), plane(48, 32, 8, 6));
        let s = a.stride;
        for w in [16, 8, 4, 12, 2] {
            for h in [16, 8, 4, 1] {
                for (ia, ib) in [(0, 3), (s + 5, 2 * s + 1), (7 * s + 9, 5)] {
                    for avg in [false, true] {
                        let bb = if avg { Some((&b.data[..], ib)) } else { None };
                        let mut x = vec![0u8; 16 * 16];
                        let mut y = vec![0u8; 16 * 16];
                        match w {
                            16 => mc_rows::<16>(&a.data, ia, bb, s, h, &mut x, 16),
                            8 => mc_rows::<8>(&a.data, ia, bb, s, h, &mut x, 16),
                            4 => mc_rows::<4>(&a.data, ia, bb, s, h, &mut x, 16),
                            _ => mc_rows_scalar(&a.data, ia, bb, s, w, h, &mut x, 16),
                        }
                        mc_rows_scalar(&a.data, ia, bb, s, w, h, &mut y, 16);
                        assert_eq!(x, y, "w {w} h {h} at {ia}/{ib} avg {avg}");
                    }
                }
            }
        }
    }
}
