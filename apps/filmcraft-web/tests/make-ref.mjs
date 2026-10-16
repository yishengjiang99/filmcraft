// Reference renders for the web export bench (docs/export-optimize-gpu.md §2.2): each bench
// project built headlessly in filmcraft-cli (MCP over stdio, the same engine commands as the web
// harness) and exported with the CPU compositor to ProRes 422 HQ, cached in target/web-bench/ref/.
// validate-export.mjs compares exports against these (PSNR / SSIM).
//
//   node apps/filmcraft-web/tests/make-ref.mjs [--projects P1,P2,P3] [--cli target/release/filmcraft-cli]
//                                              [--fixtures target/web-bench/fixtures] [--out target/web-bench/ref] [--force]
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, statSync } from "node:fs";
import { join, resolve } from "node:path";
import { arg, flag, sleep } from "./cdp.mjs";
import { buildProject } from "./bench-projects.mjs";

const cli = resolve(arg("cli", "target/release/filmcraft-cli"));
const fixtures = resolve(arg("fixtures", "target/web-bench/fixtures"));
const out = resolve(arg("out", "target/web-bench/ref"));
const projects = arg("projects", "P1,P2,P3").split(",");
mkdirSync(out, { recursive: true });

/// A minimal MCP (JSON-RPC over stdio) client for `filmcraft-cli mcp`.
async function mcpSession() {
  const proc = spawn(cli, ["mcp"], { stdio: ["pipe", "pipe", "inherit"] });
  let buf = "";
  let next = 1;
  const waiting = new Map();
  proc.stdout.on("data", (d) => {
    buf += d;
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      if (!line) continue;
      let msg;
      try { msg = JSON.parse(line); } catch { continue; }
      if (msg.id != null && waiting.has(msg.id)) {
        waiting.get(msg.id)(msg);
        waiting.delete(msg.id);
      }
    }
  });
  const rpc = (method, params) => new Promise((res, rej) => {
    const id = next++;
    waiting.set(id, (m) => (m.error ? rej(new Error(`${method}: ${m.error.message}`)) : res(m.result)));
    proc.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  });
  await rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "make-ref", version: "0" } });
  proc.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
  const execute = async (id, params = {}) => {
    const r = await rpc("tools/call", { name: "command_run", arguments: { id, params } });
    const text = r.content?.map((c) => c.text ?? "").join("") ?? "";
    if (r.isError) throw new Error(`${id}: ${text}`);
    try { return JSON.parse(text); } catch { return text; }
  };
  return {
    execute,
    async importMedia(name) {
      const r = await execute("file.import", { paths: [join(fixtures, name)] });
      const item = r.items?.[0] ?? r.imported?.[0] ?? r.ids?.[0];
      if (item == null) throw new Error(`import ${name}: ${JSON.stringify(r).slice(0, 300)}`);
      return item;
    },
    async idle() {
      for (let i = 0; i < 1200; i++) {
        const jobs = await execute("jobs.list", {});
        if (!jobs.some((j) => !j.finished)) return;
        await sleep(250);
      }
      throw new Error("jobs did not settle");
    },
    close: () => proc.kill(),
  };
}

for (const name of projects) {
  const file = join(out, `${name}.mov`);
  if (existsSync(file) && !flag("force")) {
    console.log(`${name}: cached ${file}`);
    continue;
  }
  const api = await mcpSession();
  try {
    const t0 = Date.now();
    const p = await buildProject(api, name);
    const r = await api.execute("file.exportMedia", { path: file, format: "prores", proresProfile: "hq", gpuRendering: "off", wait: true });
    if (!existsSync(file)) throw new Error(`export wrote nothing: ${JSON.stringify(r).slice(0, 300)}`);
    console.log(`${name}: ${file} (${(statSync(file).size / 2 ** 20).toFixed(0)} MiB, ${p.frames} frames, ${((Date.now() - t0) / 1000).toFixed(1)} s)`);
  } finally {
    api.close();
  }
}
