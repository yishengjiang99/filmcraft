// Deterministic benchmark media for the web export bench (docs/export-optimize-gpu.md §0.5),
// generated with ffmpeg (allowed as a fixture generator; never linked or shipped) into a cache,
// target/web-bench/fixtures/, then hard-linked into target/web/dist/bench/ so the served page
// can fetch them (`cargo xtask web` wipes dist/ on every build: rerun this after a build; it only
// re-links unless the recipe or ffmpeg changed). Never commit the output.
//
//   node apps/filmcraft-web/tests/fixtures.mjs [--out target/web-bench/fixtures] [--dist target/web/dist]
//                                             [--ffmpeg PATH] [--force] [--only a.mp4,b.webm]
//
// Every fixture carries a sync marker every 5 s: one full-white video frame and, at the same
// instant, a 1/30 s full-scale 2 kHz beep over a quiet 440 Hz (left) / 1 kHz (right) tone.
// validate-export.mjs finds both in an export to measure A/V offset.
// Encoders run single-threaded with bitexact flags, so the same ffmpeg build gives the same
// bytes; manifest.json records each file's sha256 and the ffmpeg version that made it.
import { execFileSync, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFileSync, existsSync, linkSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { arg, flag } from "./cdp.mjs";

const out = arg("out", "target/web-bench/fixtures");
const dist = arg("dist", "target/web/dist");
const force = flag("force");
mkdirSync(out, { recursive: true });

const encodersOf = (ff) => {
  try { return execFileSync(ff, ["-hide_banner", "-encoders"], { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }); } catch { return ""; }
};
// Prefer an ffmpeg that has every encoder we use; otherwise the first one found.
const candidates = [arg("ffmpeg"), "/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg", "ffmpeg"].filter(Boolean);
const NEED = ["libx264", "libx265", "libvpx-vp9", "libopus", "prores_ks", " aac "];
let ffmpeg = null;
let encoders = "";
for (const c of candidates) {
  const e = encodersOf(c);
  if (!e) continue;
  if (!ffmpeg || NEED.every((n) => e.includes(n))) { ffmpeg = c; encoders = e; }
  if (NEED.every((n) => e.includes(n))) break;
}
if (!ffmpeg) throw new Error("ffmpeg not found (pass --ffmpeg PATH)");
const ffVersion = execFileSync(ffmpeg, ["-hide_banner", "-version"], { encoding: "utf8" }).split("\n")[0];
const has = (e) => encoders.includes(e);

const BEEP = "lt(mod(t\\,5)\\,1/30)";
const audio = (d) => `aevalsrc=exprs='0.05*sin(2*PI*440*t)+0.8*sin(2*PI*2000*t)*${BEEP}|0.05*sin(2*PI*1000*t)+0.8*sin(2*PI*2000*t)*${BEEP}':s=48000:d=${d}`;
// testsrc2 blended with a moving diagonal gradient (geq: ffmpeg's `gradients` source is not
// deterministic even with a seed), plus the white flash frame every 5 s. Colour tags go through
// setparams so the encoders write them into the bitstream / sample entry.
const grad = "geq=r='abs(mod(X+Y/2+6*N\\,512)-256)':g='abs(mod(X/2+Y+3*N\\,512)-256)':b='128+Y*96/H'";
const video = (w, h, fps, d, pixfmt, tags) => [
  "-f", "lavfi", "-i", `testsrc2=s=${w}x${h}:r=${fps}:d=${d}`,
  "-f", "lavfi", "-i", `color=c=black:s=${w}x${h}:r=${fps}:d=${d},format=gbrp,${grad}`,
  "-f", "lavfi", "-i", audio(d),
  "-filter_complex", `[1:v]format=${pixfmt}[g];[0:v]format=${pixfmt}[t];[t][g]blend=all_expr='A*0.75+B*0.25',drawbox=x=0:y=0:w=iw:h=ih:color=white:t=fill:enable='eq(mod(n\\,${5 * fps})\\,0)',setparams=${tags}[v]`,
  "-map", "[v]", "-map", "2:a",
];
const BT709 = "range=tv:color_primaries=bt709:color_trc=bt709:colorspace=bt709";
const PQ = "range=tv:color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc";
const exact = ["-fflags", "+bitexact", "-flags:v", "+bitexact", "-flags:a", "+bitexact", "-map_metadata", "-1"];

const FIXTURES = [
  {
    name: "f1080p30_h264.mp4", need: ["libx264", " aac "],
    args: [...video(1920, 1080, 30, 60, "yuv420p", BT709), "-c:v", "libx264", "-preset", "medium", "-crf", "20", "-g", "60", "-threads", "1",
      "-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart"],
  },
  {
    name: "f4k30_hevc.mp4", need: ["libx265", " aac "],
    args: [...video(3840, 2160, 30, 20, "yuv420p", BT709), "-c:v", "libx265", "-preset", "fast", "-crf", "24", "-tag:v", "hvc1",
      "-x265-params", "pools=1:frame-threads=1:log-level=error:keyint=60", "-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart"],
  },
  {
    name: "f720p60_vp9.webm", need: ["libvpx-vp9", "libopus"],
    args: [...video(1280, 720, 60, 30, "yuv420p", BT709), "-c:v", "libvpx-vp9", "-b:v", "4M", "-deadline", "good", "-cpu-used", "4", "-row-mt", "0", "-threads", "1",
      "-g", "120", "-c:a", "libopus", "-b:a", "128k"],
  },
  {
    // 10-bit PQ (BT.2020 / SMPTE ST 2084) ProRes 422 HQ; the pattern is SDR content tagged as HDR,
    // enough to exercise the 10-bit/HDR decode and export paths.
    name: "f1080p_10bit_pq.mov", need: ["prores_ks"],
    args: [...video(1920, 1080, 30, 10, "yuv422p10le", PQ), "-c:v", "prores_ks", "-profile:v", "3", "-threads", "1",
      "-c:a", "pcm_s24le"],
  },
];

const sha256 = (p) => createHash("sha256").update(readFileSync(p)).digest("hex");
const manifestPath = join(out, "manifest.json");
const old = existsSync(manifestPath) ? JSON.parse(readFileSync(manifestPath, "utf8")) : { files: {} };
const manifest = { ffmpeg: ffVersion, ffmpegPath: ffmpeg, files: {}, skipped: {} };

const run = (f) => new Promise((res) => {
  const path = join(out, f.name);
  const argsKey = JSON.stringify(f.args);
  const prev = old.files?.[f.name];
  if (!force && prev && prev.args === argsKey && prev.ffmpeg === ffVersion && existsSync(path) && sha256(path) === prev.sha256) {
    manifest.files[f.name] = prev;
    return res(`${f.name}: up to date`);
  }
  const missing = f.need.filter((n) => !has(n));
  if (missing.length) {
    manifest.skipped[f.name] = `ffmpeg lacks ${missing.join(", ").trim()}`;
    return res(`${f.name}: SKIPPED (${manifest.skipped[f.name]})`);
  }
  const t0 = Date.now();
  const p = spawn(ffmpeg, ["-hide_banner", "-v", "error", "-y", ...f.args, ...exact, path], { stdio: ["ignore", "ignore", "pipe"] });
  let err = "";
  p.stderr.on("data", (d) => { err += d; });
  p.on("close", (code) => {
    if (code !== 0) {
      manifest.skipped[f.name] = `ffmpeg failed: ${err.slice(-500)}`;
      return res(`${f.name}: FAILED ${err.slice(-500)}`);
    }
    manifest.files[f.name] = { sha256: sha256(path), bytes: readFileSync(path).length, ffmpeg: ffVersion, args: argsKey, ms: Date.now() - t0 };
    res(`${f.name}: ${((Date.now() - t0) / 1000).toFixed(1)} s`);
  });
});

const only = arg("only");
for (const line of await Promise.all(FIXTURES.filter((f) => !only || only.split(",").includes(f.name)).map(run))) console.log(line);
for (const [k, v] of Object.entries(old.files ?? {})) if (!manifest.files[k] && !manifest.skipped[k] && existsSync(join(out, k))) manifest.files[k] = v;
writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
if (Object.keys(manifest.skipped).length) console.log("skipped:", manifest.skipped);
// publish into the served directory
if (existsSync(dist)) {
  const pub = join(dist, "bench");
  mkdirSync(pub, { recursive: true });
  for (const name of [...Object.keys(manifest.files), "manifest.json"]) {
    const dst = join(pub, name);
    rmSync(dst, { force: true });
    try { linkSync(join(out, name), dst); } catch { copyFileSync(join(out, name), dst); }
  }
  console.log(`published to ${pub}`);
} else {
  console.log(`${dist} does not exist yet: run cargo xtask web, then this again to publish`);
}
