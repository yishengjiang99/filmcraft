//! Speed benchmark of the encoder with the export's settings (High, Balanced, 2 B-frames, VBR, one slice per
//! four macroblock rows), self-contained so it also runs as WebAssembly (wasm32-wasip1 under node's WASI).
//!
//! `cargo run --release -p filmcraft-h264enc --no-default-features --example h264enc_bench -- [W H FRAMES [yuv-file [kbps]]]`
//!
//! The input is a raw yuv420p file (its first 16 frames, cycled) or, without one, a synthetic moving picture. Prints the
//! time per frame and an FNV-1a hash of the output (equal hashes = byte-identical streams).
//! `--no-default-features` gives the single-threaded encoder the web build uses.

use filmcraft_h264enc::*;
use std::time::Instant;

fn synth(w: usize, h: usize, t: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut seed = 0x9e37_79b9u32 ^ t as u32;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed % 9) as i32 - 4
    };
    let mut y = vec![0u8; w * h];
    for r in 0..h {
        for c in 0..w {
            let v = ((c + 3 * t) as i32 * 255 / w.max(1) as i32 + ((r / 8 + (c + 2 * t) / 8) % 2) as i32 * 40 + noise()).clamp(0, 255);
            y[r * w + c] = v as u8;
        }
    }
    let u = (0..w * h / 4).map(|i| (96 + (i + t) % 64) as u8).collect();
    let v = (0..w * h / 4).map(|i| (160 - (i / 7 + t) % 48) as u8).collect();
    (y, u, v)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    let num = |i: usize, d: usize| a.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let (w, h, n) = (num(1, 1920), num(2, 1080), num(3, 30));
    let kbps = num(5, 12_000) as u32;
    let fs = w * h * 3 / 2;
    let frames: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = match a.get(4).filter(|s| !s.is_empty() && *s != "synth") {
        Some(path) => {
            // at most 16 distinct pictures, read one at a time (keeps the wasm heap small)
            use std::io::Read;
            let mut file = std::fs::File::open(path)?;
            let mut out = Vec::new();
            let mut c = vec![0u8; fs];
            while out.len() < n.min(16) && file.read_exact(&mut c).is_ok() {
                out.push((c[..w * h].to_vec(), c[w * h..w * h * 5 / 4].to_vec(), c[w * h * 5 / 4..].to_vec()));
            }
            out
        }
        None => (0..n.min(8)).map(|t| synth(w, h, t)).collect(),
    };
    if frames.is_empty() {
        return Err("no input frames".into());
    }
    let mut cfg = EncoderConfig::new(w as u32, h as u32, 30, 1);
    cfg.format = PacketFormat::LengthPrefixed;
    cfg.keyint = 60;
    cfg.slices = h.div_ceil(16).div_ceil(4).max(1);
    cfg.rate = RateControl::Vbr { target_kbps: kbps, max_kbps: kbps * 3 / 2 };
    let mut enc = Encoder::new(cfg)?;
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut bytes = 0usize;
    let mut eat = |ps: Vec<Packet>| {
        for p in ps {
            bytes += p.data.len();
            for &b in &p.data {
                hash = (hash ^ b as u64).wrapping_mul(0x100_0000_01b3);
            }
        }
    };
    let t0 = Instant::now();
    for i in 0..n {
        let (y, u, v) = &frames[i % frames.len()];
        let f = YuvFrame { y, u, v, y_stride: w, uv_stride: w / 2 };
        eat(enc.try_encode(&f, i as i64)?);
    }
    eat(enc.flush());
    let s = t0.elapsed().as_secs_f64();
    println!("{n} frames {w}x{h}: {:.1} ms/frame, {bytes} bytes, hash {hash:016x}", s * 1000.0 / n as f64);
    Ok(())
}
