// Web export benchmark (docs/export-optimize-gpu.md §2.1), driven over CDP against a running,
// headed Chrome (benchmarks) or a spawned headless one (--launch, correctness only).
//
//   node apps/filmcraft-web/tests/export-bench.mjs --attach http://127.0.0.1:9222 \
//     --url http://127.0.0.1:8765/ --project P2 --codec h264 \
//     --encoder auto|hardware|software --compositor auto|gpu|cpu --sink auto|opfs|blob \
//     --build st|mt --runs 5 --warmup 1 --label "<what changed>" --out target/web-bench \
//     [--validate] [--ref target/web-bench/ref/P2.mov] [--expect-sha hex] [--trace] [--no-scrub] [--legacy]
//
// Each run: a fresh tab, OPFS cleared, the project built through window.filmcraft, then
// `file.exportMedia` while the harness scrubs the timeline ruler every 500 ms. Collected: wall
// time, fps, realtime factor, time to first frame, frame times (rAF), long animation frames /
// long tasks, wasm heap and JS heap samples, encoder/export counters, console failures; the
// output is fetched from the download directory. Warmup runs are discarded; medians and MADs
// of the measured runs go to <out>/results.jsonl (one line per configuration).
import { execSync } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { cpus, platform, release } from "node:os";
import { join, resolve } from "node:path";
import { arg, attach, flag, launch, sleep } from "./cdp.mjs";
import { buildProject } from "./bench-projects.mjs";
import { validate } from "./validate-export.mjs";

const opts = {
  attach: arg("attach"),
  launch: flag("launch"),
  url: arg("url", "http://127.0.0.1:8765/"),
  project: arg("project", "P2"),
  codec: arg("codec", "h264"),
  encoder: arg("encoder", "auto"),
  compositor: arg("compositor", "auto"),
  sink: arg("sink", "auto"),
  build: arg("build", "st"),
  runs: Number(arg("runs", "3")),
  warmup: Number(arg("warmup", "1")),
  label: arg("label", "unlabelled"),
  out: resolve(arg("out", "target/web-bench")),
  validate: flag("validate"),
  ref: arg("ref"),
  expectSha: arg("expect-sha"),
  trace: flag("trace"),
  scrub: !flag("no-scrub"),
  extraFlags: arg("flags", ""),
  keep: flag("keep"),
  // pre-Phase-1 code: no encoder/sink parameters or flags exist; the configuration name still
  // says what that path is (software encoder, CPU compositor, blob sink)
  legacy: flag("legacy"),
};
if (!opts.attach && !opts.launch) opts.attach = "http://127.0.0.1:9222";
const PROJECT_TIMEOUT_MS = opts.project === "P4" ? 90 * 60000 : 10 * 60000;
const CODEC_FORMAT = { h264: "h264", hevc: "hevc" };
// Pinned rate control so every encoder path is compared at equal bitrate.
const BITRATE_KBPS = (height) => (height > 1080 ? 40000 : 12000);

const sh = (cmd) => {
  try { return execSync(cmd, { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim(); } catch { return null; }
};
const median = (xs) => {
  const s = xs.filter((x) => typeof x === "number" && Number.isFinite(x)).sort((a, b) => a - b);
  if (!s.length) return null;
  const m = Math.floor(s.length / 2);
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
};
const mad = (xs) => {
  const m = median(xs);
  return m == null ? null : median(xs.filter((x) => typeof x === "number").map((x) => Math.abs(x - m)));
};
const cv = (xs) => {
  const v = xs.filter((x) => typeof x === "number");
  if (v.length < 2) return 0;
  const mean = v.reduce((s, x) => s + x, 0) / v.length;
  const sd = Math.sqrt(v.reduce((s, x) => s + (x - mean) ** 2, 0) / (v.length - 1));
  return mean ? sd / mean : 0;
};
const quantile = (xs, q) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length ? s[Math.min(s.length - 1, Math.floor(q * s.length))] : null;
};
const round = (x, d = 1) => (typeof x === "number" ? Math.round(x * 10 ** d) / 10 ** d : x);

// Installed before the app loads: long animation frames, long tasks and rAF frame deltas.
const PAGE_PROBE = `
window.__fcBench = { loaf: [], longtasks: [], frames: [], recording: false, last: 0 };
try { new PerformanceObserver((l) => { for (const e of l.getEntries()) if (window.__fcBench.recording) window.__fcBench.loaf.push({ d: e.duration, b: e.blockingDuration ?? 0 }); }).observe({ type: "long-animation-frame" }); } catch {}
try { new PerformanceObserver((l) => { for (const e of l.getEntries()) if (window.__fcBench.recording) window.__fcBench.longtasks.push(e.duration); }).observe({ type: "longtask" }); } catch {}
requestAnimationFrame(function tick(t) {
  const b = window.__fcBench;
  if (b.recording && b.last) b.frames.push(t - b.last);
  b.last = t;
  requestAnimationFrame(tick);
});
addEventListener("unhandledrejection", (e) => console.error("unhandledrejection: " + (e.reason?.stack ?? e.reason)));
`;

function appUrl() {
  const u = new URL(opts.url);
  const flags = ["empty", "fresh", "norecover"];
  if (opts.build === "st") flags.push("nothreads");
  if (!opts.legacy && opts.compositor === "cpu") flags.push("cpu");
  if (!opts.legacy && opts.encoder === "software") flags.push("softenc");
  for (const f of opts.extraFlags.split(",").filter(Boolean)) flags.push(f);
  u.search = "?" + flags.join("&");
  return u.href;
}

async function clearOrigin(page) {
  // a same-origin page that is not the app, so nothing writes OPFS meanwhile
  await page.navigate(new URL("bench/manifest.json", opts.url).href);
  await page.until("document.readyState === 'complete'", 30000);
  await page.js(`(async () => { const root = await navigator.storage.getDirectory(); for await (const [n] of root.entries()) await root.removeEntry(n, { recursive: true }); return true; })()`);
}

async function oneRun(browser, project, runDir, i) {
  const downloads = join(runDir, "downloads");
  mkdirSync(downloads, { recursive: true });
  const page = await browser.newPage("about:blank");
  const r = { run: i };
  try {
    await page.send("Page.addScriptToEvaluateOnNewDocument", { source: PAGE_PROBE });
    await page.send("Log.enable").catch(() => {});
    await page.send("Performance.enable").catch(() => {});
    await browser.send("Browser.setDownloadBehavior", { behavior: "allowAndName", downloadPath: downloads, eventsEnabled: true });
    await browser.send("Target.activateTarget", { targetId: page.targetId }).catch(() => {});
    await page.send("Emulation.setFocusEmulationEnabled", { enabled: true }).catch(() => {});
    await page.send("Page.setWebLifecycleState", { state: "active" }).catch(() => {});
    await clearOrigin(page);
    r.load = await page.loadApp(appUrl());
    if ((await page.js("document.visibilityState")) !== "visible") throw new Error("page is not visible: background throttling would distort the run");
    r.app = await page.js("filmcraft.info()");
    const built = await buildProject(page, project);
    r.projectInfo = built;
    await sleep(1000);

    // ruler geometry (element rects are in canvas backing pixels)
    const geo = await page.js(`(async () => {
      const el = await filmcraft.request('ui.elements', {});
      const ruler = (el.elements ?? el).find((e) => e.id === 'timeline.ruler');
      const c = document.querySelector('canvas');
      return { ruler: ruler?.rect ?? null, scale: c ? c.width / c.clientWidth : 1 };
    })()`);
    const domBefore = await page.send("Memory.getDOMCounters").catch(() => null);
    const uaMemBefore = await page.js("performance.measureUserAgentSpecificMemory ? performance.measureUserAgentSpecificMemory().then((m) => m.bytes) : null", 60000).catch(() => null);
    if (opts.trace) {
      await browser.send("Tracing.start", { categories: "gpu,v8,devtools.timeline,disabled-by-default-devtools.timeline,media", transferMode: "ReturnAsStream" }).catch(() => {});
    }

    const params = {
      format: CODEC_FORMAT[opts.codec],
      path: `/exports/bench-${project}.mp4`,
      bitrateMode: "vbr1Pass",
      bitrateKbps: BITRATE_KBPS(built.height),
    };
    if (!params.format) throw new Error(`codec ${opts.codec} is not exportable yet`);
    if (!opts.legacy && opts.encoder !== "auto") params.encoder = opts.encoder;
    if (!opts.legacy && opts.sink !== "auto") params.sink = opts.sink;
    r.params = params;

    let downloadDone = null;
    const downloadSeen = new Promise((res) => {
      const off = browser.on((method, p) => {
        if (method === "Browser.downloadProgress" && (p.state === "completed" || p.state === "canceled")) {
          downloadDone = p;
          off();
          res(p);
        }
      });
    });

    await page.js("(window.__fcBench.recording = true, window.__fcBench.frames.length = 0, true)");
    const t0 = Date.now();
    const ex = await page.js(`filmcraft.execute("file.exportMedia", ${JSON.stringify(params)})`);
    const samples = [];
    let firstFrameMs = null;
    let job = null;
    let scrubbing = false;
    let scrubX = 0;
    const scrub = setInterval(async () => {
      if (!opts.scrub || scrubbing || !geo.ruler) return;
      scrubbing = true;
      try {
        const [x, y, w, h] = geo.ruler.map((v) => v / geo.scale);
        const px = x + w * (0.15 + 0.7 * ((scrubX++ % 8) / 7));
        const py = y + h / 2;
        await page.send("Input.dispatchMouseEvent", { type: "mousePressed", x: px, y: py, button: "left", clickCount: 1 });
        await page.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: px + w * 0.05, y: py, button: "left", buttons: 1 });
        await page.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: px + w * 0.05, y: py, button: "left", clickCount: 1 });
      } catch {}
      scrubbing = false;
    }, 500);
    let lastMetrics = 0;
    try {
      while (Date.now() - t0 < PROJECT_TIMEOUT_MS) {
        const [jobs, info] = await page.js(`Promise.all([filmcraft.execute("jobs.list"), Promise.resolve(filmcraft.info())])`);
        job = jobs.find((j) => j.id === ex.job);
        const s = { t: Date.now() - t0, done: job?.done ?? null, wasm: info.memory?.wasmBytes ?? null };
        if (Date.now() - lastMetrics > 1000) {
          const m = await page.send("Performance.getMetrics").catch(() => null);
          s.jsHeap = m?.metrics.find((x) => x.name === "JSHeapUsedSize")?.value ?? null;
          lastMetrics = Date.now();
        }
        samples.push(s);
        if (firstFrameMs == null && (job?.done ?? 0) > 0) firstFrameMs = s.t;
        if (!job || job.finished) break;
        await sleep(firstFrameMs == null ? 50 : 250);
      }
    } finally {
      clearInterval(scrub);
    }
    const wallMs = Date.now() - t0;
    await page.js("(window.__fcBench.recording = false, true)");
    if (!job?.finished) throw new Error(`export did not finish in ${PROJECT_TIMEOUT_MS / 60000} min (${JSON.stringify(job)})`);
    if (!job.result) throw new Error(`export failed: ${job.status}`);

    if (opts.trace) {
      await browser.send("Tracing.end").catch(() => {});
    }
    const pb = await page.js(`(() => {
      const b = window.__fcBench;
      return { frames: b.frames, loaf: b.loaf, longtasks: b.longtasks };
    })()`);
    const perf = await page.js(`filmcraft.request("perf.stats", {})`).catch(() => null);
    const infoAfter = await page.js("filmcraft.info()");
    const domAfter = await page.send("Memory.getDOMCounters").catch(() => null);
    const uaMemAfter = await page.js("performance.measureUserAgentSpecificMemory ? performance.measureUserAgentSpecificMemory().then((m) => m.bytes) : null", 60000).catch(() => null);

    // output file
    await Promise.race([downloadSeen, sleep(60000)]);
    let file = null;
    if (downloadDone?.state === "completed") {
      const src = join(downloads, downloadDone.guid);
      if (existsSync(src)) {
        file = join(runDir, `run${i}.mp4`);
        renameSync(src, file);
      }
    }
    rmSync(downloads, { recursive: true, force: true });

    const wasm = samples.map((s) => s.wasm).filter((x) => typeof x === "number");
    const js = samples.map((s) => s.jsHeap).filter((x) => typeof x === "number");
    const fr = pb.frames;
    const frames = job.result.frames ?? job.total;
    Object.assign(r, {
      wallMs,
      exportMs: Math.round((job.result.seconds ?? wallMs / 1000) * 1000),
      frames,
      exportFps: frames / (wallMs / 1000),
      realtimeFactor: built.durationS / (wallMs / 1000),
      timeToFirstFrameMs: firstFrameMs,
      bytes: job.result.bytes,
      ui: { frames: fr.length, p50: quantile(fr, 0.5), p95: quantile(fr, 0.95), max: fr.length ? Math.max(...fr) : null, over50: fr.filter((d) => d > 50).length },
      loaf: { count: pb.loaf.length, over50: pb.loaf.filter((l) => l.d > 50).length, max: pb.loaf.length ? Math.max(...pb.loaf.map((l) => l.d)) : 0, blockingMs: pb.loaf.reduce((s, l) => s + l.b, 0) },
      longtasks: { count: pb.longtasks.length, max: pb.longtasks.length ? Math.max(...pb.longtasks) : 0, tbtMs: pb.longtasks.reduce((s, d) => s + Math.max(0, d - 50), 0) },
      perfStatsUi: perf?.ui ?? null,
      memory: {
        wasmStart: wasm[0] ?? null, wasmPeak: wasm.length ? Math.max(...wasm) : null, wasmGrowth: wasm.length ? Math.max(...wasm) - wasm[0] : null,
        jsHeapPeak: js.length ? Math.max(...js) : null, uaBefore: uaMemBefore, uaAfter: uaMemAfter,
      },
      export: {
        backend: infoAfter.export?.backend ?? (perf?.export?.hardware?.sessions ? "hardware" : "h264enc"),
        compositor: infoAfter.export?.compositor ?? "cpu",
        sink: infoAfter.export?.sink ?? "blob",
        thread: infoAfter.export?.thread ?? "main",
        gpu: infoAfter.export?.gpu ?? null,
        encodeQueueMax: infoAfter.export?.encodeQueueMax ?? null,
        encoderErrors: infoAfter.export?.encoderErrors ?? null,
        stages: perf?.export?.stages ?? null,
        hardware: perf?.export?.hardware ?? null,
      },
      leaks: {
        domBefore, domAfter,
        openVideoFrames: infoAfter.export?.openVideoFrames ?? null,
        gpuBuffers: infoAfter.export?.gpuBuffers ?? null,
        webcodecs: infoAfter.webcodecsStats?.sessions?.length ?? null,
      },
      threaded: r.load?.threaded ?? null,
      file,
    });
  } catch (e) {
    r.error = String(e?.message ?? e);
  }
  r.fatal = page.fatalLogs();
  r.consoleTail = page.logs.slice(-20);
  await page.close();
  return r;
}

function aggregate(runs) {
  const keys = {
    wallMs: (r) => r.wallMs, exportFps: (r) => r.exportFps, realtimeFactor: (r) => r.realtimeFactor,
    timeToFirstFrameMs: (r) => r.timeToFirstFrameMs, uiP50: (r) => r.ui?.p50, uiP95: (r) => r.ui?.p95, uiMax: (r) => r.ui?.max,
    loafCount: (r) => r.loaf?.count, loafOver50: (r) => r.loaf?.over50, loafBlockingMs: (r) => r.loaf?.blockingMs, longtaskTbtMs: (r) => r.longtasks?.tbtMs,
    wasmPeak: (r) => r.memory?.wasmPeak, wasmGrowth: (r) => r.memory?.wasmGrowth, jsHeapPeak: (r) => r.memory?.jsHeapPeak, bytes: (r) => r.bytes,
  };
  const out = {};
  for (const [k, f] of Object.entries(keys)) {
    const xs = runs.map(f);
    out[k] = { median: round(median(xs), 2), mad: round(mad(xs), 2) };
  }
  out.wallCv = round(cv(runs.map((r) => r.wallMs)), 4);
  return out;
}

// ---- main
mkdirSync(opts.out, { recursive: true });
const browser = opts.launch ? await launch({ headless: true }) : await attach(opts.attach);
const env = existsSync(join(opts.out, "env.json")) ? JSON.parse(readFileSync(join(opts.out, "env.json"), "utf8")) : null;
const chromeVersion = browser.version.Browser;
const chromeChanged = env && env.chrome?.Browser !== chromeVersion;
if (chromeChanged) console.error(`WARNING: Chrome is ${chromeVersion}, env.json was recorded with ${env.chrome?.Browser}: results are not comparable`);
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const config = `${opts.project}-${opts.codec}-${opts.encoder}-${opts.compositor}-${opts.sink}-${opts.build}`;
const runDir = join(opts.out, "runs", `${stamp}-${config}`);
mkdirSync(runDir, { recursive: true });

const all = [];
const total = opts.warmup + opts.runs;
for (let i = 0; i < total; i++) {
  const r = await oneRun(browser, opts.project, runDir, i);
  r.warmup = i < opts.warmup;
  all.push(r);
  console.error(`${config} run ${i}${r.warmup ? " (warmup)" : ""}: ${r.error ? "ERROR " + r.error : `${(r.wallMs / 1000).toFixed(1)} s, ${r.exportFps.toFixed(2)} fps, ui p95 ${round(r.ui.p95)} ms, wasm peak ${round(r.memory.wasmPeak / 2 ** 20)} MiB`}${r.fatal.length ? " FATAL " + r.fatal.join(" | ").slice(0, 300) : ""}`);
  if (r.warmup && r.file && !opts.keep) rmSync(r.file, { force: true });
}
let measured = all.filter((r) => !r.warmup && !r.error);
let reruns = 0;
while (measured.length >= 2 && cv(measured.map((r) => r.wallMs)) > 0.05 && reruns < 3) {
  reruns++;
  const r = await oneRun(browser, opts.project, runDir, total + reruns - 1);
  all.push(r);
  console.error(`${config} rerun ${reruns} (CV ${round(cv(measured.map((x) => x.wallMs)) * 100)}%): ${r.error ?? (r.wallMs / 1000).toFixed(1) + " s"}`);
  if (!r.error) measured.push(r);
}
browser.disconnect();

// validation of the measured outputs (after timing, so the oracle's CPU load never overlaps a run)
const validations = [];
if (opts.validate) {
  for (const r of measured.filter((r) => r.file)) {
    const v = validate(r.file, {
      fps: r.projectInfo.fps, durationS: r.projectInfo.durationS, width: r.projectInfo.width, height: r.projectInfo.height, codec: opts.codec === "hevc" ? "hevc" : "h264",
      ref: opts.ref, quality: opts.ref ? (opts.encoder === "software" || opts.encoder === "auto" ? "sw" : "hw") : undefined, sha: opts.expectSha, workDir: runDir,
    });
    validations.push({ run: r.run, ok: v.ok, failed: v.checks.filter((c) => !c.ok), sha256: v.info.sha256, sync: v.info.sync?.maxAbsOffsetMs ?? null, quality: v.info.quality ?? null });
  }
}
// keep the first measured output (for inspection / the determinism check), drop the rest
if (!opts.keep) for (const r of measured) if (r.file && existsSync(r.file) && r !== measured[0]) rmSync(r.file, { force: true });

const failed = all.filter((r) => !r.warmup && (r.error || r.fatal.length));
const result = {
  time: new Date().toISOString(),
  label: opts.label,
  git: { sha: sh("git rev-parse HEAD"), dirty: !!sh("git status --porcelain --untracked-files=no") },
  chrome: chromeVersion,
  chromeChanged,
  gpu: env?.chromeGpu?.glRenderer ?? null,
  os: `${platform()} ${release()}${env?.host?.macos ? " (macOS " + env.host.macos + ")" : ""}`,
  cpu: cpus()[0]?.model ?? null,
  power: sh("pmset -g batt | head -1"),
  config, project: opts.project, codec: opts.codec, encoder: opts.encoder, compositor: opts.compositor, sink: opts.sink, build: opts.build,
  thread: measured[0]?.export?.thread ?? null,
  flags: appUrl().split("?")[1],
  runs: measured.length, warmup: opts.warmup, reruns,
  noisy: cv(measured.map((r) => r.wallMs)) > 0.05,
  metrics: aggregate(measured),
  perRun: measured.map((r) => ({ wallMs: r.wallMs, exportFps: round(r.exportFps, 3), uiP95: round(r.ui?.p95), wasmPeak: r.memory?.wasmPeak, threaded: r.threaded })),
  backend: measured[0]?.export ?? null,
  leaks: measured.at(-1)?.leaks ?? null,
  validations,
  errors: failed.map((r) => ({ run: r.run, error: r.error ?? null, fatal: r.fatal })),
  ok: failed.length === 0 && measured.length > 0 && validations.every((v) => v.ok),
};
appendFileSync(join(opts.out, "results.jsonl"), JSON.stringify(result) + "\n");
writeFileSync(join(runDir, "runs.json"), JSON.stringify(all, null, 2));
console.log(JSON.stringify({ label: result.label, config, ok: result.ok, noisy: result.noisy, metrics: result.metrics, validations, errors: result.errors }, null, 2));
process.exit(result.ok ? 0 : 1);
