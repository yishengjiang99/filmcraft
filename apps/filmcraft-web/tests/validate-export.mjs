// Correctness oracle for web exports (docs/export-optimize-gpu.md §2.2). ffmpeg/ffprobe are used
// only as external test oracles.
//
//   node apps/filmcraft-web/tests/validate-export.mjs --file out.mp4 --fps 30 --duration 60 \
//     --width 1920 --height 1080 [--codec h264] [--ref ref.mov] [--quality hw|sw] [--sha <expected>]
//
// Checks: stream layout and colour tags (ffprobe), duration ±1 frame, exact frame count, zero
// decode errors (-xerror), A/V sync from the fixtures' flash + beep markers (≤ one AAC frame,
// 1024/48000 s), PSNR/SSIM against a reference render (when given), and sha256 (for the
// byte-identical software path). Exits non-zero when any check fails; `validate()` is the API.
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { arg } from "./cdp.mjs";

export const FFMPEG = process.env.FFMPEG ?? "ffmpeg";
export const FFPROBE = process.env.FFPROBE ?? "ffprobe";
const SYNC_TOLERANCE_S = 1024 / 48000;
// §2.2 thresholds — never loosen these to make a run pass.
export const QUALITY = { psnrMin: 38, psnrFrameMin: 30, ssimMin: 0.97 };

const sh = (cmd, args, opts = {}) => {
  const r = spawnSync(cmd, args, { maxBuffer: 1 << 30, ...opts });
  return { code: r.status, stdout: r.stdout, stderr: r.stderr?.toString() ?? "" };
};
const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length ? s[Math.floor(s.length / 2)] : 0;
};

export function sha256(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

export function probe(file) {
  const r = sh(FFPROBE, ["-v", "error", "-count_frames", "-show_streams", "-show_format", "-of", "json", file]);
  if (r.code !== 0) throw new Error(`ffprobe: ${r.stderr}`);
  return JSON.parse(r.stdout.toString());
}

/// Per-frame mean luma (0–255) and timestamps, from a 32×18 grey thumbnail of every frame.
function lumaTrack(file) {
  // showinfo logs at info level: keep ffmpeg at info, without banner/stats noise
  const r = sh(FFMPEG, ["-hide_banner", "-nostats", "-v", "info", "-i", file, "-an", "-vf", "scale=32:18:flags=area,format=gray,showinfo", "-f", "rawvideo", "-"]);
  if (r.code !== 0) throw new Error(`luma: ${r.stderr.slice(-500)}`);
  const n = 32 * 18;
  const times = [...r.stderr.matchAll(/pts_time:\s*([\d.]+)/g)].map((m) => Number(m[1]));
  const lum = [];
  for (let i = 0; i + n <= r.stdout.length; i += n) {
    let s = 0;
    for (let j = i; j < i + n; j++) s += r.stdout[j];
    lum.push(s / n);
  }
  return { times, lum };
}

/// Flash frames: a single frame much brighter than its neighbourhood.
function flashes(file) {
  const { times, lum } = lumaTrack(file);
  const out = [];
  for (let i = 0; i < lum.length; i++) {
    const around = lum.slice(Math.max(0, i - 8), i).concat(lum.slice(i + 1, i + 9));
    const base = median(around);
    if (lum[i] - base > Math.max(40, (255 - base) * 0.35) && times[i] != null) out.push(times[i]);
  }
  return out;
}

/// Beep onsets: the 2 kHz full-scale bursts (mono mix, 48 kHz) rising out of the quiet tone bed.
function beeps(file) {
  const r = sh(FFMPEG, ["-v", "error", "-i", file, "-vn", "-ac", "1", "-ar", "48000", "-f", "f32le", "-"]);
  if (r.code !== 0) throw new Error(`audio: ${r.stderr.slice(-500)}`);
  const b = r.stdout;
  const n = Math.floor(b.length / 4);
  const s = new Float32Array(b.buffer, b.byteOffset, n);
  // window peak envelope, 1 ms windows
  const W = 48;
  const env = [];
  for (let i = 0; i + W <= n; i += W) {
    let m = 0;
    for (let j = i; j < i + W; j++) m = Math.max(m, Math.abs(s[j]));
    env.push(m);
  }
  const quiet = median(env);
  const thr = Math.max(0.25, quiet * 3);
  const out = [];
  let last = -1e9;
  for (let w = 0; w < env.length; w++) {
    if (env[w] > thr && w - last > 1000) {
      // refine to the first sample over threshold in this window
      let k = w * W;
      while (k < (w + 1) * W && Math.abs(s[k]) <= thr) k++;
      out.push(k / 48000);
      last = w;
    }
    if (env[w] > thr) last = w;
  }
  return out;
}

export function avSync(file) {
  const f = flashes(file);
  const a = beeps(file);
  const pairs = f.map((t) => {
    const near = a.reduce((best, x) => (Math.abs(x - t) < Math.abs(best - t) ? x : best), Infinity);
    return { flash: t, beep: near, offsetMs: (near - t) * 1000 };
  });
  return { flashes: f.length, beeps: a.length, pairs, maxAbsOffsetMs: pairs.length ? Math.max(...pairs.map((p) => Math.abs(p.offsetMs))) : null };
}

/// PSNR (Y, per frame) and SSIM (All) of `file` against `ref`, scaled to the same size.
export function quality(file, ref, worstDir) {
  const psnrLog = join(worstDir ?? dirname(file), `${basename(file)}.psnr.log`);
  const r = sh(FFMPEG, ["-hide_banner", "-nostats", "-v", "info", "-i", file, "-i", ref, "-lavfi",
    `[0:v]setpts=PTS-STARTPTS[a];[1:v]setpts=PTS-STARTPTS[b];[a][b]scale2ref[a2][b2];[a2]split[a3][a4];[b2]split[b3][b4];[a3][b3]psnr=shortest=1:stats_file=${psnrLog.replace(/:/g, "\\:")};[a4][b4]ssim=shortest=1`, "-f", "null", "-"]);
  if (r.code !== 0) throw new Error(`psnr/ssim: ${r.stderr.slice(-500)}`);
  const frames = readFileSync(psnrLog, "utf8").trim().split("\n").map((l) => {
    const n = Number(l.match(/n:(\d+)/)?.[1]);
    const y = l.match(/psnr_y:(\S+)/)?.[1];
    return { n, y: y === "inf" ? 99 : Number(y) };
  });
  const ys = frames.map((f) => f.y);
  const mean = ys.reduce((s, v) => s + v, 0) / Math.max(1, ys.length);
  const worst = frames.reduce((w, f) => (f.y < w.y ? f : w), { n: -1, y: Infinity });
  const ssim = Number(r.stderr.match(/SSIM .*All:([\d.]+)/)?.[1]);
  return { psnrY: Math.round(mean * 100) / 100, psnrYMin: worst.y, worstFrame: worst.n, ssim, frames: ys.length };
}

/// Side-by-side PNG of frame `n` (export | reference) for a failed quality check.
function worstFramePng(file, ref, n, out) {
  sh(FFMPEG, ["-v", "error", "-y", "-i", file, "-i", ref, "-lavfi", `[0:v]select=eq(n\\,${n})[a];[1:v]select=eq(n\\,${n})[b];[a][b]scale2ref[a2][b2];[a2][b2]hstack`, "-frames:v", "1", out]);
  return out;
}

/// All checks; `expect` = { fps, durationS, width, height, codec?, audio? (default true),
/// colour? ("bt709"), ref?, quality? ("hw"|"sw"), sha? }.
export function validate(file, expect) {
  const checks = [];
  const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
  const info = { file, bytes: readFileSync(file).length, sha256: sha256(file) };
  let p;
  try {
    p = probe(file);
  } catch (e) {
    check("ffprobe", false, String(e.message));
    return { ok: false, checks, info };
  }
  const v = p.streams.find((s) => s.codec_type === "video");
  const a = p.streams.find((s) => s.codec_type === "audio");
  info.video = v && { codec: v.codec_name, profile: v.profile, pix_fmt: v.pix_fmt, width: v.width, height: v.height, r_frame_rate: v.r_frame_rate, frames: Number(v.nb_read_frames ?? v.nb_frames), color: [v.color_primaries, v.color_transfer, v.color_space, v.color_range] };
  info.audio = a && { codec: a.codec_name, sample_rate: Number(a.sample_rate), channels: a.channels };
  info.duration = Number(p.format.duration);
  check("video stream", v, info.video);
  if (v) {
    if (expect.codec) check("codec", v.codec_name === expect.codec, v.codec_name);
    check("pix_fmt 4:2:0", /^yuv420p/.test(v.pix_fmt) || v.pix_fmt === "nv12", v.pix_fmt);
    check("size", v.width === expect.width && v.height === expect.height, `${v.width}x${v.height}`);
    const [num, den] = String(v.r_frame_rate).split("/").map(Number);
    check("frame rate", den > 0 && Math.abs(num / den - expect.fps) < 1e-3, v.r_frame_rate);
    const frames = Math.round(expect.durationS * expect.fps);
    check("frame count", info.video.frames === frames, `${info.video.frames} (expected ${frames})`);
    const colour = expect.colour ?? "bt709";
    if (colour === "bt709") {
      check("colour tags", v.color_primaries === "bt709" && v.color_transfer === "bt709" && v.color_space === "bt709" && (v.color_range ?? "tv") === "tv", info.video.color.join("/"));
    }
  }
  check("duration ±1 frame", Math.abs(info.duration - expect.durationS) <= 1 / expect.fps + 1e-6, info.duration);
  if (expect.audio !== false) {
    check("audio stream", a && a.codec_name === "aac" && Number(a.sample_rate) === 48000 && a.channels === 2, info.audio);
  }
  const dec = sh(FFMPEG, ["-v", "error", "-xerror", "-i", file, "-f", "null", "-"]);
  check("decodes cleanly (-xerror)", dec.code === 0 && !dec.stderr.trim(), dec.stderr.trim().slice(0, 500));
  if (expect.audio !== false && v && a) {
    try {
      const sync = avSync(file);
      const want = Math.floor(expect.durationS / 5 - 1e-9) + 1;
      info.sync = { flashes: sync.flashes, beeps: sync.beeps, maxAbsOffsetMs: sync.maxAbsOffsetMs, offsets: sync.pairs.map((x) => Math.round(x.offsetMs * 10) / 10) };
      check("sync markers found", sync.flashes >= want * 0.8 && sync.flashes <= want, `${sync.flashes} flashes / ${sync.beeps} beeps, expected ${want}`);
      check("A/V sync ≤ 21.3 ms", sync.maxAbsOffsetMs != null && sync.maxAbsOffsetMs <= SYNC_TOLERANCE_S * 1000, `max |offset| ${sync.maxAbsOffsetMs?.toFixed(1)} ms`);
    } catch (e) {
      check("A/V sync", false, String(e.message));
    }
  }
  if (expect.ref) {
    try {
      const q = quality(file, expect.ref, expect.workDir);
      info.quality = q;
      if (expect.quality === "hw" || expect.quality === "sw") {
        const ok = q.psnrY >= QUALITY.psnrMin && q.psnrYMin >= QUALITY.psnrFrameMin && q.ssim >= QUALITY.ssimMin;
        check(`quality (PSNR ≥ ${QUALITY.psnrMin}, min frame ≥ ${QUALITY.psnrFrameMin}, SSIM ≥ ${QUALITY.ssimMin})`, ok, q);
        if (!ok && q.worstFrame >= 0) {
          const png = join(expect.workDir ?? dirname(file), `${basename(file)}.worst-${q.worstFrame}.png`);
          info.worstFramePng = worstFramePng(file, expect.ref, q.worstFrame, png);
        }
      }
    } catch (e) {
      check("quality", false, String(e.message));
    }
  }
  if (expect.sha) check("byte-identical to baseline", info.sha256 === expect.sha, `${info.sha256.slice(0, 16)} vs ${expect.sha.slice(0, 16)}`);
  return { ok: checks.every((c) => c.ok), checks, info };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const file = arg("file");
  if (!file) {
    console.error("usage: validate-export.mjs --file out.mp4 --fps 30 --duration 60 --width 1920 --height 1080 [--codec h264] [--ref ref] [--quality hw|sw] [--sha hex] [--no-audio]");
    process.exit(2);
  }
  const r = validate(file, {
    fps: Number(arg("fps", "30")), durationS: Number(arg("duration")), width: Number(arg("width", "1920")), height: Number(arg("height", "1080")),
    codec: arg("codec"), ref: arg("ref"), quality: arg("quality"), sha: arg("sha"), audio: !process.argv.includes("--no-audio"),
  });
  for (const c of r.checks) console.log(`${c.ok ? "ok  " : "FAIL"} ${c.name}: ${typeof c.detail === "object" ? JSON.stringify(c.detail) : c.detail ?? ""}`);
  console.log(JSON.stringify(r.info));
  process.exit(r.ok ? 0 : 1);
}
