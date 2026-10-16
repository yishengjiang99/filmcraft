// The web export bench projects (docs/export-optimize-gpu.md §0.6), built only through engine
// commands (`window.filmcraft` in the web app, MCP `command_run` in the CLI) so every run is identical. Media comes from fixtures.mjs (served under
// `bench/`). Every source carries a white flash + beep every 5 s from its own t = 0; edits keep
// clips at their source time (no slips) so the markers stay paired in the export.
//
//   import { buildProject, PROJECTS } from "./bench-projects.mjs";
//   const p = await buildProject(api, "P2");   // → { sequence, durationS, fps, width, height, ... }
export const TICKS = 254016000000;
const t = (s) => Math.round(s * TICKS);

/// The project builders talk to an `api` with `execute(id, params)` and `importMedia(name)`:
/// `webApi(page)` drives the web app over CDP, `mcpApi(...)` the headless CLI (reference renders).
export function webApi(page) {
  return {
    execute: (id, params = {}) => page.js(`filmcraft.execute(${JSON.stringify(id)}, ${JSON.stringify(params)})`),
    async importMedia(name) {
      const r = await page.js(`filmcraft.importUrl(${JSON.stringify(`bench/${name}`)})`);
      if (!r.items?.length) throw new Error(`import ${name}: ${JSON.stringify(r.errors)}`);
      return r.items[0];
    },
    idle: () => page.until(`filmcraft.execute("jobs.list").then((js) => !js.some((j) => !j.finished))`, 300000, 250),
  };
}
const ex = (api, id, params = {}) => api.execute(id, params);
const importMedia = (api, name) => api.importMedia(name);
async function inspect(api) {
  return ex(api, "sequence.inspect", {});
}
/// Clip ids on a track ("V1", "A2", …) in timeline order.
async function clipsOn(api, track) {
  const s = await inspect(api);
  const list = track[0] === "V" ? s.video : s.audio;
  const tr = list[Number(track.slice(1)) - 1];
  if (!tr) throw new Error(`no track ${track}`);
  return tr.items.filter((i) => i.clip != null).sort((a, b) => a.start - b.start);
}
async function effectIndex(api, clip, effect) {
  const s = await inspect(api);
  for (const tr of [...s.video, ...s.audio]) {
    const it = tr.items.find((i) => i.clip === clip);
    if (it) {
      const i = it.effects.findIndex((e) => e.effect === effect);
      if (i >= 0) return i;
    }
  }
  throw new Error(`clip ${clip} has no ${effect}`);
}
async function addEffect(api, clip, effect, params = {}) {
  await ex(api, "effects.apply", { clips: [clip], effect });
  const index = await effectIndex(api, clip, effect);
  for (const [param, value] of Object.entries(params)) await ex(api, "effects.setParam", { clip, effect: index, param, value });
  return index;
}
async function title(api, text, { at, seconds, track, size = 120, position }) {
  return ex(api, "graphics.newText", { text, seconds, track, time: t(at), size, ...(position ? { position } : {}) });
}

export const PROJECTS = {
  // one 1080p30 clip, 20 s, no effects
  async P1(api) {
    const item = await importMedia(api, "f1080p30_h264.mp4");
    const { sequence } = await ex(api, "file.newSequence", { name: "P1 simple", width: 1920, height: 1080, fps: 30, video: 1, audio: 1 });
    await ex(api, "timeline.place", { item, track: "V1", seconds: 0, duration: t(20) });
    return { sequence, durationS: 20, fps: 30, width: 1920, height: 1080 };
  },

  // 1080p30, 60 s: 2 video tracks, 3 cuts, a cross-dissolve, Lumetri basic, a title,
  // dialogue (the clip's own audio, A1) + music (A2) with fades
  async P2(api) {
    const clip = await importMedia(api, "f1080p30_h264.mp4");
    const music = await importMedia(api, "f720p60_vp9.webm");
    const { sequence } = await ex(api, "file.newSequence", { name: "P2 typical", width: 1920, height: 1080, fps: 30, video: 2, audio: 2 });
    await ex(api, "timeline.place", { item: clip, track: "V1", audioTrack: "A1", seconds: 0, duration: t(60) });
    for (const s of [12, 27, 41]) await ex(api, "timeline.razor", { time: t(s), track: "V1" });
    const v1 = await clipsOn(api, "V1");
    if (v1.length !== 4) throw new Error(`P2: expected 4 V1 segments after 3 cuts, got ${v1.length}`);
    await ex(api, "sequence.applyVideoTransition", { clip: v1[2].clip, effect: "cross_dissolve", edge: "in", frames: 30 });
    for (const seg of v1) await addEffect(api, seg.clip, "lumetri", { exposure: 0.3, contrast: 15, saturation: 115 });
    await title(api, "FilmCraft bench P2", { at: 6, seconds: 8, track: 1, size: 96, position: [160, 960] });
    // music: the 30 s webm's sound twice, back to back, faded in and out
    await ex(api, "timeline.place", { item: music, track: "A2", seconds: 0, video: false, duration: t(30) });
    await ex(api, "timeline.place", { item: music, track: "A2", seconds: 30, video: false, duration: t(30) });
    const a2 = await clipsOn(api, "A2");
    if (a2.length !== 2) throw new Error(`P2: expected 2 music clips, got ${a2.length}`);
    await ex(api, "sequence.applyAudioTransition", { clip: a2[0].clip, effect: "constant_power", edge: "in", frames: 60 });
    await ex(api, "sequence.applyAudioTransition", { clip: a2[1].clip, effect: "constant_power", edge: "out", frames: 60 });
    await ex(api, "clip.audioGain", { clips: a2.map((c) => c.clip), mode: "set", db: -12 });
    return { sequence, durationS: 60, fps: 30, width: 1920, height: 1080 };
  },

  // 4K30, 20 s: blur, LUT, vignette; a picture-in-picture with a scale keyframe; 3 titles
  async P3(api) {
    const main = await importMedia(api, "f4k30_hevc.mp4");
    const pip = await importMedia(api, "f1080p30_h264.mp4");
    const { sequence } = await ex(api, "file.newSequence", { name: "P3 heavy", width: 3840, height: 2160, fps: 30, video: 3, audio: 1 });
    const { clips: [bg] } = await ex(api, "timeline.place", { item: main, track: "V1", audioTrack: "A1", seconds: 0, duration: t(20) });
    await addEffect(api, bg, "gaussian_blur", { blurriness: 20 });
    await addEffect(api, bg, "lumetri", {});
    await ex(api, "lumetri.setLook", { clip: bg, lut: "builtin:look-teal-orange" });
    await addEffect(api, bg, "vignette", { amount: -60 });
    const { clips: [pv] } = await ex(api, "timeline.place", { item: pip, track: "V2", seconds: 0, audio: false, duration: t(20) });
    await ex(api, "effects.setParam", { clip: pv, effect: "motion", param: "position", value: [2900, 700] });
    await ex(api, "effects.toggleAnimation", { clip: pv, effect: "motion", param: "scale" });
    await ex(api, "effects.setParam", { clip: pv, effect: "motion", param: "scale", value: 40, time: t(0) });
    await ex(api, "effects.setParam", { clip: pv, effect: "motion", param: "scale", value: 70, time: t(19) });
    for (const [i, at] of [0, 7, 14].entries()) await title(api, `Heavy title ${i + 1}`, { at, seconds: 6, track: 2, size: 160, position: [240, 1900 - i * 200] });
    return { sequence, durationS: 20, fps: 30, width: 3840, height: 2160 };
  },

  // Short variants for the iteration loop: the same structure in a fraction of the time (each
  // keeps ≥ 2 of the 5 s sync markers). The full P1–P4 are for end-of-phase runs.
  async P1s(api) {
    const item = await importMedia(api, "f1080p30_h264.mp4");
    const { sequence } = await ex(api, "file.newSequence", { name: "P1s simple", width: 1920, height: 1080, fps: 30, video: 1, audio: 1 });
    await ex(api, "timeline.place", { item, track: "V1", seconds: 0, duration: t(6) });
    return { sequence, durationS: 6, fps: 30, width: 1920, height: 1080 };
  },

  async P2s(api) {
    const clip = await importMedia(api, "f1080p30_h264.mp4");
    const music = await importMedia(api, "f720p60_vp9.webm");
    const { sequence } = await ex(api, "file.newSequence", { name: "P2s typical", width: 1920, height: 1080, fps: 30, video: 2, audio: 2 });
    await ex(api, "timeline.place", { item: clip, track: "V1", audioTrack: "A1", seconds: 0, duration: t(12) });
    for (const s of [3, 7.5, 9.5]) await ex(api, "timeline.razor", { time: t(s), track: "V1" });
    const v1 = await clipsOn(api, "V1");
    if (v1.length !== 4) throw new Error(`P2s: expected 4 V1 segments after 3 cuts, got ${v1.length}`);
    await ex(api, "sequence.applyVideoTransition", { clip: v1[2].clip, effect: "cross_dissolve", edge: "in", frames: 30 });
    for (const seg of v1) await addEffect(api, seg.clip, "lumetri", { exposure: 0.3, contrast: 15, saturation: 115 });
    await title(api, "FilmCraft bench P2s", { at: 1, seconds: 6, track: 1, size: 96, position: [160, 960] });
    await ex(api, "timeline.place", { item: music, track: "A2", seconds: 0, video: false, duration: t(12) });
    const a2 = await clipsOn(api, "A2");
    await ex(api, "sequence.applyAudioTransition", { clip: a2[0].clip, effect: "constant_power", edge: "in", frames: 30 });
    await ex(api, "sequence.applyAudioTransition", { clip: a2[0].clip, effect: "constant_power", edge: "out", frames: 30 });
    await ex(api, "clip.audioGain", { clips: [a2[0].clip], mode: "set", db: -12 });
    return { sequence, durationS: 12, fps: 30, width: 1920, height: 1080 };
  },

  async P3s(api) {
    const main = await importMedia(api, "f4k30_hevc.mp4");
    const pip = await importMedia(api, "f1080p30_h264.mp4");
    const { sequence } = await ex(api, "file.newSequence", { name: "P3s heavy", width: 3840, height: 2160, fps: 30, video: 3, audio: 1 });
    const { clips: [bg] } = await ex(api, "timeline.place", { item: main, track: "V1", audioTrack: "A1", seconds: 0, duration: t(6) });
    await addEffect(api, bg, "gaussian_blur", { blurriness: 20 });
    await addEffect(api, bg, "lumetri", {});
    await ex(api, "lumetri.setLook", { clip: bg, lut: "builtin:look-teal-orange" });
    await addEffect(api, bg, "vignette", { amount: -60 });
    const { clips: [pv] } = await ex(api, "timeline.place", { item: pip, track: "V2", seconds: 0, audio: false, duration: t(6) });
    await ex(api, "effects.setParam", { clip: pv, effect: "motion", param: "position", value: [2900, 700] });
    await ex(api, "effects.toggleAnimation", { clip: pv, effect: "motion", param: "scale" });
    await ex(api, "effects.setParam", { clip: pv, effect: "motion", param: "scale", value: 40, time: t(0) });
    await ex(api, "effects.setParam", { clip: pv, effect: "motion", param: "scale", value: 70, time: t(5.5) });
    for (const [i, at] of [0, 2, 4].entries()) await title(api, `Heavy title ${i + 1}`, { at, seconds: 2, track: 2, size: 160, position: [240, 1900 - i * 200] });
    return { sequence, durationS: 6, fps: 30, width: 3840, height: 2160 };
  },

  // P1's 20 s clip looped to 60 min (memory / streaming test)
  async P4(api) {
    const item = await importMedia(api, "f1080p30_h264.mp4");
    const { sequence } = await ex(api, "file.newSequence", { name: "P4 long", width: 1920, height: 1080, fps: 30, video: 1, audio: 1 });
    for (let i = 0; i < 180; i++) await ex(api, "timeline.place", { item, track: "V1", seconds: i * 20, duration: t(20) });
    return { sequence, durationS: 3600, fps: 30, width: 1920, height: 1080 };
  },
};

/// Build project `name` (on a fresh app/session) and wait until no import/index job runs. `target`
/// is a CDP page (the web app) or an api object (`mcpApi`).
export async function buildProject(target, name) {
  const api = target.execute ? target : webApi(target);
  const build = PROJECTS[name];
  if (!build) throw new Error(`unknown project ${name} (P1..P4, P1s..P3s)`);
  const meta = await build(api);
  await api.idle();
  const s = await inspect(api);
  const durationS = s.duration / TICKS;
  if (Math.abs(durationS - meta.durationS) > 0.05) throw new Error(`${name}: sequence is ${durationS.toFixed(3)} s, expected ${meta.durationS}`);
  return { name, ...meta, frames: Math.round(meta.durationS * meta.fps), tracks: { video: s.video.length, audio: s.audio.length } };
}
