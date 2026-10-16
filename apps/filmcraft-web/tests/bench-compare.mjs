// Compare web export bench results (docs/export-optimize-gpu.md §2.3).
//
//   node apps/filmcraft-web/tests/bench-compare.mjs --baseline baseline [--label <label>] \
//     [--results target/web-bench/results.jsonl] [--doc docs/benchmarks/web-export.md] [--title "…"]
//     [--target wallMs]
//
// For every configuration (project/codec/encoder/compositor/sink/build) measured under both the
// baseline label (or git sha prefix) and the latest label (default: the newest line's label),
// prints wall time, fps, UI p95, long-animation-frame blocking, peak wasm memory and PSNR with
// deltas. Exits non-zero on a regression of a non-target metric beyond noise (§4.2): wall time
// > +5 %, UI p95 > +10 %, peak memory > +10 %, or any failed validation. `--doc` appends the
// table under a dated heading.
import { appendFileSync, existsSync, readFileSync } from "node:fs";
import { arg } from "./cdp.mjs";

const resultsPath = arg("results", "target/web-bench/results.jsonl");
const baselineKey = arg("baseline", "baseline");
const target = arg("target", "");
const doc = arg("doc");
const lines = existsSync(resultsPath) ? readFileSync(resultsPath, "utf8").trim().split("\n").filter(Boolean).map((l) => JSON.parse(l)) : [];
if (!lines.length) {
  console.error(`no results in ${resultsPath}`);
  process.exit(2);
}
const latestLabel = arg("label", lines.at(-1).label);
const matches = (r, key) => r.label === key || (key.length >= 7 && r.git?.sha?.startsWith(key));
// newest result per configuration
const pick = (key) => {
  const m = new Map();
  for (const r of lines) if (matches(r, key)) m.set(r.config, r);
  return m;
};
const base = pick(baselineKey);
const cur = pick(latestLabel);

const M = [
  // [column, getter, unit, regression threshold (fraction; worse = higher), higher is better]
  ["wall s", (r) => r.metrics.wallMs.median / 1000, "s", 0.05, false, "wallMs"],
  ["fps", (r) => r.metrics.exportFps.median, "", null, true, "exportFps"],
  ["UI p95 ms", (r) => r.metrics.uiP95.median, "ms", 0.1, false, "uiP95"],
  ["LoAF block ms", (r) => r.metrics.loafBlockingMs.median, "ms", null, false, "loafBlockingMs"],
  ["wasm peak MiB", (r) => r.metrics.wasmPeak.median / 2 ** 20, "MiB", 0.1, false, "wasmPeak"],
  ["PSNR dB", (r) => r.validations?.find((v) => v.quality)?.quality?.psnrY ?? null, "dB", null, true, "psnr"],
];
const fmt = (x) => (x == null ? "–" : Math.abs(x) >= 100 ? x.toFixed(0) : x.toFixed(2));
const delta = (b, c, higherBetter) => {
  if (b == null || c == null || b === 0) return "";
  const d = (c - b) / b;
  const speed = higherBetter ? c / b : b / c;
  return ` (${d >= 0 ? "+" : ""}${(d * 100).toFixed(0)}%${higherBetter != null && Math.abs(speed - 1) > 0.05 ? `, ${speed.toFixed(2)}×` : ""})`;
};

const rows = [];
const regressions = [];
for (const [config, c] of cur) {
  const b = base.get(config);
  const cells = [config];
  for (const [name, get, , thr, higherBetter, key] of M) {
    const cv = c.metrics ? get(c) : null;
    const bv = b?.metrics ? get(b) : null;
    cells.push(b && c !== b ? `${fmt(bv)} → ${fmt(cv)}${delta(bv, cv, higherBetter)}` : fmt(cv));
    if (b && c !== b && thr != null && key !== target && bv != null && cv != null && cv > bv * (1 + thr)) {
      // the noise band: a change within the combined MAD of both runs is not a regression
      const madB = b.metrics[key]?.mad ?? 0;
      const madC = c.metrics[key]?.mad ?? 0;
      const scale = key === "wallMs" ? 1000 : key === "wasmPeak" ? 2 ** 20 : 1;
      if ((cv - bv) * scale > madB + madC) regressions.push(`${config}: ${name} ${fmt(bv)} → ${fmt(cv)}`);
    }
  }
  cells.push(c.ok ? "ok" : "FAIL");
  if (!c.ok) regressions.push(`${config}: run or validation failed (${JSON.stringify(c.errors).slice(0, 200)} ${JSON.stringify(c.validations?.flatMap((v) => v.failed.map((f) => f.name)))})`);
  if (c.noisy) cells[cells.length - 1] += " (noisy)";
  rows.push(cells);
}
rows.sort((a, b) => a[0].localeCompare(b[0]));
const header = ["config", ...M.map((m) => m[0]), "status"];
const table = [`| ${header.join(" | ")} |`, `|${header.map(() => "---").join("|")}|`, ...rows.map((r) => `| ${r.join(" | ")} |`)].join("\n");
const sample = cur.values().next().value;
const meta = `Baseline \`${baselineKey}\` vs \`${latestLabel}\` (${sample?.git?.sha?.slice(0, 8)}${sample?.git?.dirty ? "+dirty" : ""}), ${sample?.chrome}, ${sample?.gpu ?? "unknown GPU"}, ${sample?.os}.`;
console.log(meta + "\n\n" + table);
if (regressions.length) console.log("\nREGRESSIONS:\n" + regressions.map((r) => "- " + r).join("\n"));
if (doc) {
  const title = arg("title", `${latestLabel} vs ${baselineKey}`);
  appendFileSync(doc, `\n## ${new Date().toISOString().slice(0, 10)}: ${title}\n\n${meta}\n\n${table}\n${regressions.length ? "\nRegressions:\n" + regressions.map((r) => "- " + r).join("\n") + "\n" : ""}`);
}
process.exit(regressions.length ? 1 : 0);
