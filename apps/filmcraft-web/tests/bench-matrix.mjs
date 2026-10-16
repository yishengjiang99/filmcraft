// Runs a matrix of export-bench.mjs configurations one after another (never in parallel: runs
// must not share the machine), with validation against the reference renders.
//
//   node apps/filmcraft-web/tests/bench-matrix.mjs --label baseline --url http://127.0.0.1:8765/ \
//     [--matrix quick|full|"P2:h264:software:auto:auto:mt,…"] [--runs 3] [--warmup 1]
//
// Configuration = project:codec:encoder:compositor:sink:build. Combinations env.json marks as
// unsupported (no encoder for the codec) are skipped and listed.
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { arg } from "./cdp.mjs";

const label = arg("label", "unlabelled");
const url = arg("url", "http://127.0.0.1:8765/");
const out = arg("out", "target/web-bench");
const runs = arg("runs", "3");
const warmup = arg("warmup", "1");
const matrix = arg("matrix", "quick");
const extra = arg("extra", "");
const env = existsSync(`${out}/env.json`) ? JSON.parse(readFileSync(`${out}/env.json`, "utf8")) : null;

const MATRICES = {
  // the per-iteration loop (§3): P2 × H.264 × {software, hardware}
  quick: ["P2:h264:software:auto:auto:mt", "P2:h264:hardware:auto:auto:mt"],
  full: [
    ...["P1", "P2", "P3"].flatMap((p) => ["software", "hardware"].flatMap((e) => ["cpu", "gpu"].map((c) => `${p}:h264:${e}:${c}:auto:mt`))),
    ...["P1", "P2", "P3"].map((p) => `${p}:hevc:hardware:gpu:auto:mt`),
    "P2:h264:software:cpu:auto:st", "P2:h264:hardware:gpu:auto:st",
    "P2:h264:hardware:gpu:blob:mt", "P2:h264:hardware:gpu:opfs:mt",
  ],
};
const configs = (MATRICES[matrix] ?? matrix.split(",")).map((c) => c.split(":"));

const supported = ([, codec, encoder]) => {
  if (!env) return true;
  const p = env.paths ?? {};
  if (codec === "hevc") return encoder === "hardware" && p.hwHevc;
  if (codec === "h264" && encoder === "hardware") return p.hwH264;
  return codec === "h264";
};
const skipped = [];
const results = [];
for (const c of configs) {
  const [project, codec, encoder, compositor, sink, build] = c;
  if (!supported(c)) {
    skipped.push(c.join(":"));
    continue;
  }
  const args = ["apps/filmcraft-web/tests/export-bench.mjs", "--attach", arg("attach", "http://127.0.0.1:9222"), "--url", url, "--project", project, "--codec", codec,
    "--encoder", encoder, "--compositor", compositor, "--sink", sink, "--build", build, "--runs", runs, "--warmup", warmup, "--label", label, "--out", out, "--validate",
    "--ref", `${out}/ref/${project}.mov`, ...extra.split(" ").filter(Boolean)];
  const t0 = Date.now();
  const r = spawnSync(process.execPath, args, { stdio: ["ignore", "ignore", "inherit"] });
  results.push(`${c.join(":")}: ${r.status === 0 ? "ok" : "FAILED"} (${((Date.now() - t0) / 60000).toFixed(1)} min)`);
  console.error(results.at(-1));
}
console.log(results.join("\n"));
if (skipped.length) console.log("skipped (unsupported here):", skipped.join(", "));
