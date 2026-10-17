// The export's I420 picture straight from the compositor's accumulator: what the CPU computes
// with `Image::over_black_rgba8` (linear premultiplied → sRGB 8-bit through the same 4096-entry
// table) followed by `filmcraft_export::rgba_to_yuv420_8` (BT.709 limited range, chroma the mean
// of each 2×2 block, rows then columns). Output: Y (w·h bytes), U, V (cw·ch each) back to back,
// packed little-endian four bytes to a word; one invocation writes one word, so any size works
// without two invocations sharing a word.

struct P {
    w: u32,
    h: u32,
    cw: u32,
    ch: u32,
    words: u32,
    _p0: u32,
    _p1: u32,
    _p2: u32,
};

@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var accum: texture_2d<f32>;
// `filmcraft_color::linear_to_srgb_u8_table`
@group(0) @binding(2) var<storage, read> srgb: array<u32>;
@group(0) @binding(3) var<storage, read_write> out: array<u32>;

fn enc8(v: f32) -> f32 {
    let i = u32(clamp(v, 0.0, 1.0) * 4095.0 + 0.5);
    return f32(srgb[min(i, 4095u)]) / 255.0;
}

fn rgb(x: u32, y: u32) -> vec3<f32> {
    let c = textureLoad(accum, vec2<i32>(i32(x), i32(y)), 0);
    return vec3(enc8(c.r), enc8(c.g), enc8(c.b));
}

fn luma(c: vec3<f32>) -> f32 {
    return 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
}

// `(x).round().clamp(1, 254)` for the non-negative values here (round half away from zero; WGSL's
// `round` would round half to even)
fn code(x: f32) -> u32 {
    return u32(clamp(floor(x + 0.5), 1.0, 254.0));
}

// Chroma sample (cx, cy): U when `v_plane` is false.
fn chroma(cx: u32, cy: u32, v_plane: bool) -> u32 {
    var s = 0.0;
    var n = 0.0;
    for (var dy = 0u; dy < 2u; dy++) {
        let y = cy * 2u + dy;
        if y >= p.h {
            break;
        }
        for (var dx = 0u; dx < 2u; dx++) {
            let x = cx * 2u + dx;
            if x >= p.w {
                break;
            }
            let c = rgb(x, y);
            let yy = luma(c);
            if v_plane {
                s += (c.r - yy) / 1.5748;
            } else {
                s += (c.b - yy) / 1.8556;
            }
            n += 1.0;
        }
    }
    return code(128.0 + 224.0 * s / max(n, 1.0));
}

fn byte_at(i: u32) -> u32 {
    let ysz = p.w * p.h;
    let csz = p.cw * p.ch;
    if i < ysz {
        return code(16.0 + 219.0 * luma(rgb(i % p.w, i / p.w)));
    }
    if i < ysz + csz {
        let c = i - ysz;
        return chroma(c % p.cw, c / p.cw, false);
    }
    if i < ysz + 2u * csz {
        let c = i - ysz - csz;
        return chroma(c % p.cw, c / p.cw, true);
    }
    return 0u;
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>, @builtin(num_workgroups) n: vec3<u32>) {
    let k = id.x + id.y * n.x * 64u;
    if k >= p.words {
        return;
    }
    let b = k * 4u;
    out[k] = byte_at(b) | (byte_at(b + 1u) << 8u) | (byte_at(b + 2u) << 16u) | (byte_at(b + 3u) << 24u);
}
