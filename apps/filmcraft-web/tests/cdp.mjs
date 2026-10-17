// Minimal Chrome DevTools Protocol client shared by the web tests and benchmarks.
// No npm dependencies (Node ≥ 22: global WebSocket and fetch).
//
//   const browser = await attach("http://127.0.0.1:9222");        // an already-running Chrome
//   const browser = await launch({ headless: true, port: 9333 });  // spawn one (correctness only)
//   const page = await browser.newPage("http://127.0.0.1:8765/");
//   await page.js("filmcraft.info()");
//   await page.close(); browser.disconnect();
//
// One browser-level WebSocket, one flattened session per page (Target.attachToTarget
// {flatten: true}), so several pages can be driven at once and closed independently.
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/// `--key value` from argv, else `d`.
export const arg = (k, d) => {
  const i = process.argv.indexOf(`--${k}`);
  return i > 0 && i + 1 < process.argv.length ? process.argv[i + 1] : d;
};
export const flag = (k) => process.argv.includes(`--${k}`);

export const defaultChrome = process.platform === "darwin" ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "google-chrome";

/// `/json/version` of a DevTools endpoint, or null when nothing answers.
export async function version(http) {
  try {
    const r = await fetch(`${http.replace(/\/$/, "")}/json/version`, { signal: AbortSignal.timeout(3000) });
    return r.ok ? await r.json() : null;
  } catch {
    return null;
  }
}

/// Connect to an already-running Chrome (`--remote-debugging-port`).
export async function attach(http) {
  const v = await version(http);
  if (!v) throw new Error(`no Chrome DevTools endpoint at ${http} (start Chrome with --remote-debugging-port)`);
  const b = await connect(v.webSocketDebuggerUrl);
  b.version = v;
  b.spawned = null;
  return b;
}

/// Spawn a fresh Chrome with a temporary profile and connect to it. For correctness runs only:
/// headless GPU and media numbers are not representative.
export async function launch({ chrome = defaultChrome, port = 9333, headless = true, args = [] } = {}) {
  const profile = mkdtempSync(join(tmpdir(), "fc-chrome-"));
  const proc = spawn(chrome, [
    ...(headless ? ["--headless=new"] : []),
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    "--no-first-run",
    "--no-default-browser-check",
    "--enable-unsafe-webgpu",
    "--autoplay-policy=no-user-gesture-required",
    "--window-size=1600,1000",
    ...args,
    "about:blank",
  ], { stdio: "ignore" });
  const http = `http://127.0.0.1:${port}`;
  for (let i = 0; i < 100; i++) {
    if (await version(http)) {
      const b = await attach(http);
      b.spawned = proc;
      return b;
    }
    await sleep(100);
  }
  proc.kill();
  throw new Error("Chrome did not start");
}

async function connect(wsUrl) {
  const ws = new WebSocket(wsUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = () => rej(new Error(`cannot connect to ${wsUrl}`)); });
  let nextId = 1;
  const waiting = new Map();
  // sessionId ("" = browser) -> Set of (method, params) listeners
  const listeners = new Map();
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && waiting.has(msg.id)) {
      waiting.get(msg.id)(msg);
      waiting.delete(msg.id);
      return;
    }
    if (msg.method) for (const f of listeners.get(msg.sessionId ?? "") ?? []) f(msg.method, msg.params);
  };
  ws.onclose = () => {
    for (const [, f] of waiting) f({ error: { message: "DevTools connection closed" } });
    waiting.clear();
  };
  const send = (method, params = {}, sessionId) => {
    const id = nextId++;
    ws.send(JSON.stringify(sessionId ? { id, method, params, sessionId } : { id, method, params }));
    return new Promise((res, rej) => waiting.set(id, (m) => (m.error ? rej(new Error(`${method}: ${m.error.message}`)) : res(m.result))));
  };
  const on = (sessionId, f) => {
    if (!listeners.has(sessionId)) listeners.set(sessionId, new Set());
    listeners.get(sessionId).add(f);
    return () => listeners.get(sessionId)?.delete(f);
  };

  const browser = {
    send: (method, params) => send(method, params),
    on: (f) => on("", f),
    /// Open `url` in a new tab and attach to it. `opts.background` leaves it unfocused.
    async newPage(url = "about:blank", { width = 1600, height = 1000, background = false } = {}) {
      const { targetId } = await send("Target.createTarget", { url: "about:blank", background });
      const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
      const page = makePage(send, on, sessionId, targetId);
      await page.send("Page.enable");
      await page.send("Runtime.enable");
      await send("Inspector.enable", {}, sessionId).catch(() => {});
      if (width && height) await page.send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false }).catch(() => {});
      if (url !== "about:blank") await page.navigate(url);
      return page;
    },
    disconnect() {
      try { ws.close(); } catch {}
      if (browser.spawned) browser.spawned.kill();
    },
  };
  return browser;
}

function makePage(send, on, sessionId, targetId) {
  const logs = [];
  // set when the renderer crashed or the wasm instance trapped: the page then never answers
  // promises again (`js` rejects instead of waiting forever)
  let broken = null;
  const breakers = new Set();
  const markBroken = (why) => {
    if (broken) return;
    broken = why;
    for (const f of breakers) f(new Error(`page broken: ${why}`));
  };
  const TRAP = /RuntimeError|unreachable executed|memory access out of bounds|Out of memory/i;
  on(sessionId, (method, params) => {
    if (method === "Inspector.targetCrashed") markBroken("the renderer crashed");
    if (method === "Runtime.exceptionThrown") {
      const d = params.exceptionDetails.exception?.description ?? params.exceptionDetails.text ?? "";
      if (TRAP.test(d)) markBroken(d.split("\n")[0]);
    }
    if (method === "Runtime.consoleAPICalled" && params.type === "error") {
      const t = params.args.map((a) => a.value ?? a.description ?? "").join(" ");
      if (TRAP.test(t)) markBroken(t.split("\n")[0]);
    }
    if (method === "Runtime.consoleAPICalled") {
      logs.push(`${params.type}: ` + params.args.map((a) => a.value ?? a.description ?? "").join(" "));
    } else if (method === "Runtime.exceptionThrown") {
      logs.push("EXCEPTION " + JSON.stringify(params.exceptionDetails.exception?.description ?? params.exceptionDetails.text));
    } else if (method === "Log.entryAdded") {
      logs.push(`log.${params.entry.level}: ${params.entry.text}`);
    }
  });
  const page = {
    sessionId,
    targetId,
    logs,
    /// Why the page stopped answering (renderer crash, wasm trap), or null.
    get broken() { return broken; },
    send: (method, params) => send(method, params, sessionId),
    on: (f) => on(sessionId, f),
    /// Evaluate `expr` (promises awaited) and return its JSON value; throws on a JS exception.
    /// `timeout` also bounds an awaited promise (CDP's own timeout covers synchronous code only),
    /// and a page that crashed or trapped fails at once.
    async js(expr, timeout = 120000) {
      if (broken) throw new Error(`page broken: ${broken}`);
      let timer = null;
      let breaker = null;
      const guard = new Promise((_, rej) => {
        timer = setTimeout(() => rej(new Error(`${expr.slice(0, 200)}: no answer in ${timeout} ms`)), timeout + 5000);
        breaker = rej;
        breakers.add(rej);
      });
      let r;
      try {
        r = await Promise.race([send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true, timeout }, sessionId), guard]);
      } finally {
        clearTimeout(timer);
        breakers.delete(breaker);
      }
      if (r.exceptionDetails) throw new Error(`${expr.slice(0, 200)}: ${r.exceptionDetails.exception?.description ?? r.exceptionDetails.text}`);
      return r.result.value;
    },
    async until(expr, ms = 60000, step = 100) {
      const t0 = Date.now();
      while (Date.now() - t0 < ms) {
        if (await page.js(expr)) return Date.now() - t0;
        await sleep(step);
      }
      throw new Error(`timeout waiting for ${expr}`);
    },
    async navigate(url) {
      await send("Page.navigate", { url }, sessionId);
    },
    /// Navigate and wait for the FilmCraft loader to report ready (or fail).
    async loadApp(url, ms = 120000) {
      await page.navigate(url);
      await page.until("!!(window.filmcraftLoad && (window.filmcraftLoad.readyMs || window.filmcraftLoad.error))", ms);
      const load = await page.js("window.filmcraftLoad");
      if (load.error) throw new Error(`app failed to load: ${load.error}`);
      return load;
    },
    async shot(path) {
      const r = await send("Page.captureScreenshot", { format: "png" }, sessionId);
      writeFileSync(path, Buffer.from(r.data, "base64"));
      return path;
    },
    /// Console lines that mean the run is broken: Rust panics, uncaught exceptions/rejections,
    /// WebGPU validation errors.
    fatalLogs() {
      return logs.filter((l) => /panicked at|RuntimeError|EXCEPTION|Uncaught|unhandledrejection|validation error|GPUValidationError/i.test(l));
    },
    async close() {
      await send("Target.closeTarget", { targetId }).catch(() => {});
    },
  };
  return page;
}
