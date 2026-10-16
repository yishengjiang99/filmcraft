// Records the benchmark environment (docs/export-optimize-gpu.md §0): Chrome version, GPU and
// driver (chrome://gpu + SystemInfo.getInfo), WebGPU adapter features/limits, the WebCodecs
// encoder/decoder support matrix and the web platform APIs the export work relies on.
// Every later decision about which paths to bench comes from the file it writes.
//
//   node apps/filmcraft-web/tests/probe-env.mjs --attach http://127.0.0.1:9222 \
//     --url http://127.0.0.1:8765/ --out target/web-bench
import { execSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { cpus, totalmem, release, platform, arch } from "node:os";
import { join } from "node:path";
import { arg, attach, sleep } from "./cdp.mjs";

const http = arg("attach", "http://127.0.0.1:9222");
const url = arg("url", "http://127.0.0.1:8765/");
const out = arg("out", "target/web-bench");
mkdirSync(out, { recursive: true });

const sh = (cmd) => {
  try { return execSync(cmd, { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim(); } catch { return null; }
};

// Codec candidates: [label, codec string, width, height, bitrate, extra config]
const VIDEO = [
  ["h264-high-1080p", "avc1.640028", 1920, 1080, 8e6, { avc: { format: "avc" } }],
  ["h264-main-1080p", "avc1.4d0028", 1920, 1080, 8e6, { avc: { format: "avc" } }],
  ["h264-baseline-1080p", "avc1.42e028", 1920, 1080, 8e6, { avc: { format: "avc" } }],
  ["h264-high-4k", "avc1.640033", 3840, 2160, 30e6, { avc: { format: "avc" } }],
  ["h264-main-4k", "avc1.4d0033", 3840, 2160, 30e6, { avc: { format: "avc" } }],
  ["h264-baseline-4k", "avc1.42e033", 3840, 2160, 30e6, { avc: { format: "avc" } }],
  ["hevc-main-1080p", "hvc1.1.6.L120.B0", 1920, 1080, 6e6, { hevc: { format: "hevc" } }],
  ["hevc-main-4k", "hvc1.1.6.L153.B0", 3840, 2160, 20e6, { hevc: { format: "hevc" } }],
  ["hevc-main10-1080p", "hvc1.2.4.L120.B0", 1920, 1080, 6e6, { hevc: { format: "hevc" } }],
  ["hevc-main10-4k", "hvc1.2.4.L153.B0", 3840, 2160, 20e6, { hevc: { format: "hevc" } }],
  ["av1-main8-1080p", "av01.0.08M.08", 1920, 1080, 5e6, {}],
  ["av1-main8-4k", "av01.0.12M.08", 3840, 2160, 16e6, {}],
  ["av1-main10-4k", "av01.0.12M.10", 3840, 2160, 16e6, {}],
  ["vp9-p0-1080p", "vp09.00.40.08", 1920, 1080, 6e6, {}],
  ["vp9-p0-4k", "vp09.00.50.08", 3840, 2160, 20e6, {}],
  ["vp9-p2-10bit-4k", "vp09.02.50.10", 3840, 2160, 20e6, {}],
];
const AUDIO = [
  ["aac-lc", { codec: "mp4a.40.2", sampleRate: 48000, numberOfChannels: 2, bitrate: 192000 }],
  ["opus", { codec: "opus", sampleRate: 48000, numberOfChannels: 2, bitrate: 128000 }],
  ["flac", { codec: "flac", sampleRate: 48000, numberOfChannels: 2 }],
];

// Runs in the app page (same origin + COOP/COEP as the real export).
const pageProbe = `(async () => {
  const VIDEO = ${JSON.stringify(VIDEO)};
  const AUDIO = ${JSON.stringify(AUDIO)};
  const r = { crossOriginIsolated: self.crossOriginIsolated === true, userAgent: navigator.userAgent,
    hardwareConcurrency: navigator.hardwareConcurrency, deviceMemory: navigator.deviceMemory ?? null };
  const sup = async (f) => { try { const s = await f(); return s.supported === true; } catch (e) { return "error: " + e.message; } };

  // WebCodecs encode / decode matrix
  r.videoEncode = {}; r.videoDecode = {};
  for (const [label, codec, width, height, bitrate, extra] of VIDEO) {
    const base = { codec, width, height, bitrate, framerate: 30, latencyMode: "quality", ...extra };
    const e = {};
    for (const hw of ["prefer-hardware", "prefer-software"]) {
      e[hw] = await sup(() => VideoEncoder.isConfigSupported({ ...base, hardwareAcceleration: hw }));
    }
    for (const mode of ["constant", "variable", "quantizer"]) {
      e["bitrateMode-" + mode + "-hw"] = await sup(() => VideoEncoder.isConfigSupported({ ...base, hardwareAcceleration: "prefer-hardware", bitrateMode: mode }));
    }
    r.videoEncode[label] = { codec, width, height, ...e };
    r.videoDecode[label] = {
      "prefer-hardware": await sup(() => VideoDecoder.isConfigSupported({ codec, codedWidth: width, codedHeight: height, hardwareAcceleration: "prefer-hardware" })),
      "prefer-software": await sup(() => VideoDecoder.isConfigSupported({ codec, codedWidth: width, codedHeight: height, hardwareAcceleration: "prefer-software" })),
    };
  }
  r.audioEncode = {};
  for (const [label, cfg] of AUDIO) r.audioEncode[label] = await sup(() => AudioEncoder.isConfigSupported(cfg));

  // WebGPU adapter
  r.webgpu = null;
  if (navigator.gpu) {
    try {
      const a = await navigator.gpu.requestAdapter({ powerPreference: "high-performance" });
      if (a) {
        const limits = {};
        for (const k in a.limits) limits[k] = a.limits[k];
        const info = a.info ?? {};
        r.webgpu = {
          features: [...a.features].sort(),
          timestampQuery: a.features.has("timestamp-query"),
          shaderF16: a.features.has("shader-f16"),
          isFallbackAdapter: a.isFallbackAdapter ?? info.isFallbackAdapter ?? null,
          info: { vendor: info.vendor, architecture: info.architecture, device: info.device, description: info.description },
          limits,
          preferredCanvasFormat: navigator.gpu.getPreferredCanvasFormat(),
          wgslLanguageFeatures: navigator.gpu.wgslLanguageFeatures ? [...navigator.gpu.wgslLanguageFeatures].sort() : null,
        };
      }
    } catch (e) { r.webgpu = { error: e.message }; }
  }

  // Platform APIs the export phases depend on (all used behind feature detection)
  const m64 = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0, 5, 3, 1, 4, 1]);
  let workerApis = null;
  try {
    const src = "postMessage({ syncAccessHandle: typeof FileSystemSyncAccessHandle === 'function', gpu: !!self.navigator.gpu, videoEncoder: typeof VideoEncoder === 'function', offscreenWebgpu: (() => { try { return !!new OffscreenCanvas(1, 1).getContext('webgpu'); } catch { return false; } })() })";
    const w = new Worker(URL.createObjectURL(new Blob([src], { type: "text/javascript" })));
    workerApis = await new Promise((res) => { w.onmessage = (e) => res(e.data); w.onerror = (e) => res({ error: String(e.message) }); setTimeout(() => res({ error: "timeout" }), 3000); });
    w.terminate();
  } catch (e) { workerApis = { error: e.message }; }
  r.apis = {
    jspi: typeof WebAssembly.Suspending === "function" && typeof WebAssembly.promising === "function",
    memory64: (() => { try { return WebAssembly.validate(m64); } catch { return false; } })(),
    sharedArrayBuffer: typeof SharedArrayBuffer === "function",
    showSaveFilePicker: typeof self.showSaveFilePicker === "function",
    showDirectoryPicker: typeof self.showDirectoryPicker === "function",
    opfs: !!navigator.storage?.getDirectory,
    storageEstimate: await navigator.storage?.estimate?.().catch(() => null) ?? null,
    storagePersisted: await navigator.storage?.persisted?.().catch(() => null) ?? null,
    compressionDeflateRaw: (() => { try { new CompressionStream("deflate-raw"); return true; } catch { return false; } })(),
    schedulerYield: typeof self.scheduler?.yield === "function",
    schedulerPostTask: typeof self.scheduler?.postTask === "function",
    longAnimationFrame: PerformanceObserver.supportedEntryTypes.includes("long-animation-frame"),
    longtask: PerformanceObserver.supportedEntryTypes.includes("longtask"),
    measureUserAgentSpecificMemory: typeof performance.measureUserAgentSpecificMemory === "function",
    wakeLock: !!navigator.wakeLock,
    webLocks: !!navigator.locks,
    setAppBadge: typeof navigator.setAppBadge === "function",
    notification: typeof self.Notification === "function",
    videoFrameFromCanvas: (() => { try { const c = new OffscreenCanvas(2, 2); c.getContext("2d"); const f = new VideoFrame(c, { timestamp: 0 }); f.close(); return true; } catch { return false; } })(),
    mediaStreamTrackProcessor: typeof self.MediaStreamTrackProcessor === "function",
    worker: workerApis,
  };
  return r;
})()`;

// chrome://gpu renders into nested shadow roots: collect all text.
const gpuPageText = `(() => {
  const parts = [];
  const walk = (n) => {
    if (n.shadowRoot) walk(n.shadowRoot);
    for (const c of n.childNodes) {
      if (c.nodeType === 3) { const t = c.textContent.trim(); if (t) parts.push(t); }
      else if (c.nodeType === 1 || c.nodeType === 11) {
        walk(c);
        if (c.nodeType === 1 && /^(TR|DIV|LI|H\\d|P)$/.test(c.tagName)) parts.push("\\n");
      }
    }
  };
  walk(document.body);
  return parts.join(" ").replace(/ *\\n */g, "\\n").replace(/\\n+/g, "\\n");
})()`;

const browser = await attach(http);
const env = {
  recordedAt: new Date().toISOString(),
  git: { sha: sh("git rev-parse HEAD"), dirty: !!sh("git status --porcelain") },
  chrome: browser.version,
  host: {
    platform: platform(), arch: arch(), osRelease: release(),
    macos: sh("sw_vers -productVersion"),
    cpu: cpus()[0]?.model ?? null, cpuCount: cpus().length, memGB: Math.round(totalmem() / 2 ** 30),
    power: sh("pmset -g batt | head -1"),
    lowPowerMode: sh("pmset -g | awk '/lowpowermode/ {print $2}'"),
  },
};

// 0.2: chrome://gpu + SystemInfo
const gpuTab = await browser.newPage("about:blank");
await gpuTab.navigate("chrome://gpu");
await sleep(3000);
const gpuText = await gpuTab.js(gpuPageText);
writeFileSync(join(out, "chrome-gpu.txt"), gpuText);
const status = {};
for (const line of gpuText.split("\n")) {
  const m = line.match(/^(?:\* )?(Canvas|Direct Rendering Display Compositor|Compositing|Multiple Raster Threads|OpenGL|Rasterization|Raw Draw|Skia Graphite|TreesInViz|Video Decode|Video Encode|Vulkan|WebGL|WebGL2|WebGPU|WebNN):\s*(.+)$/);
  if (m) status[m[1]] = m[2];
}
const field = (re) => gpuText.match(re)?.[1]?.trim() ?? null;
env.chromeGpu = {
  featureStatus: status,
  gpu0: field(/^GPU0 : (.+)$/m),
  glRenderer: field(/^GL_RENDERER : (.+)$/m),
  driverVersion: field(/DRIVER_VERSION=([^ ]+)/),
  skiaBackend: field(/^Skia Backend : (.+)$/m),
  os: field(/^Operating system : (.+)$/m),
  raw: "chrome-gpu.txt",
};
await gpuTab.close();
try {
  const si = await browser.send("SystemInfo.getInfo");
  env.systemInfo = {
    modelName: si.modelName, modelVersion: si.modelVersion,
    devices: si.gpu.devices, driverBugWorkarounds: si.gpu.driverBugWorkarounds?.length ?? null,
    auxAttributes: si.gpu.auxAttributes,
    videoDecoding: si.gpu.videoDecoding, videoEncoding: si.gpu.videoEncoding,
  };
} catch (e) {
  env.systemInfo = { error: e.message };
}

// 0.3: the app page (cross-origin isolated, same origin as the export)
const app = await browser.newPage("about:blank");
await app.send("Target.activateTarget", { targetId: app.targetId }).catch(() => {});
const load = await app.loadApp(new URL("?empty&fresh&norecover", url).href);
env.app = { url, load, info: await app.js("filmcraft.info()").catch((e) => ({ error: e.message })) };
env.page = await app.js(pageProbe);
await app.close();
browser.disconnect();

// Summary: the paths worth benchmarking on this machine.
const enc = env.page.videoEncode;
env.paths = {
  hwH264: !!(enc["h264-high-1080p"]?.["prefer-hardware"] === true),
  hwH264_4k: !!(enc["h264-high-4k"]?.["prefer-hardware"] === true),
  hwHevc: !!(enc["hevc-main-1080p"]?.["prefer-hardware"] === true),
  hwHevc10: !!(enc["hevc-main10-4k"]?.["prefer-hardware"] === true),
  hwAv1: !!(enc["av1-main8-1080p"]?.["prefer-hardware"] === true),
  hwVp9: !!(enc["vp9-p0-1080p"]?.["prefer-hardware"] === true),
  swAv1: !!(enc["av1-main8-1080p"]?.["prefer-software"] === true),
  swVp9: !!(enc["vp9-p0-1080p"]?.["prefer-software"] === true),
  aac: env.page.audioEncode["aac-lc"] === true,
  opus: env.page.audioEncode.opus === true,
  webgpu: !!env.page.webgpu?.features,
  timestampQuery: !!env.page.webgpu?.timestampQuery,
};
writeFileSync(join(out, "env.json"), JSON.stringify(env, null, 2));
console.log(JSON.stringify({ chrome: env.chrome.Browser, gpu: status, paths: env.paths, apis: env.page.apis }, null, 2));
