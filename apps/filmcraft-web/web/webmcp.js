// WebMCP tools for FilmCraft (docs/webmcp.md). Registers in-page tools for browser agents through the
// WebMCP proposal (github.com/webmachinelearning/webmcp). Does nothing unless the browser exposes
// modelContext (document.modelContext per the current spec, navigator.modelContext in older drafts).
// Off switch: remove the <script> tag, or open the page with ?nowebmcp.
(() => {
  "use strict";
  if (new URLSearchParams(location.search).has("nowebmcp")) return;
  const ctx = (typeof document !== "undefined" && document.modelContext) || (typeof navigator !== "undefined" && navigator.modelContext);
  if (!ctx || !("modelContext" in navigator || "modelContext" in document)) return;

  const TPS = 254016000000; // FilmCraft ticks per second
  const HELPER = "/filmcraft-agent.js", CATALOG = "webmcp-commands.json";
  const DESTRUCTIVE = /delete|remove|clear|close|reset|export|newProject|lift|extract|replace|overwrite/i;
  const text = (v) => ({ content: [{ type: "text", text: typeof v === "string" ? v : JSON.stringify(v === undefined ? null : v) }] });
  const fail = (e) => ({ isError: true, content: [{ type: "text", text: "Error: " + String((e && e.message) || e) }] });
  const ticks = (s) => Math.round(Number(s) * TPS);

  const fc = () => new Promise((ok, bad) => {
    const t0 = Date.now();
    (function poll() {
      if (window.filmcraft && window.filmcraft.execute) return ok(window.filmcraft);
      if (Date.now() - t0 > 30000) return bad(new Error("FilmCraft engine is not loaded yet"));
      setTimeout(poll, 200);
    })();
  });
  const exec = async (id, params) => (await fc()).execute(id, params || {});

  // Destructive actions ask the user. Prefer the spec's client.requestUserInteraction when the agent passes a
  // client; fall back to window.confirm.
  async function confirmUser(client, msg) {
    try {
      if (client && typeof client.requestUserInteraction === "function")
        return !!(await client.requestUserInteraction(async () => window.confirm(msg)));
    } catch (e) { /* fall through */ }
    return window.confirm(msg);
  }
  const guard = async (client, msg, fn) => (await confirmUser(client, msg)) ? fn() : text("Cancelled by the user.");

  const scriptSrc = document.currentScript && document.currentScript.src;
  let catalog;
  const loadCatalog = () => catalog ||= fetch(new URL(CATALOG, scriptSrc || location.href))
    .then((r) => r.ok ? r.json() : []).catch(() => []);
  let helper;
  const loadHelper = () => helper ||= new Promise((ok, bad) => {
    if (window.filmcraftAgent) return ok();
    const s = document.createElement("script"); s.src = HELPER;
    s.onload = () => window.filmcraftAgent ? ok() : bad(new Error("agent helper missing"));
    s.onerror = () => { helper = null; bad(new Error("agent endpoint not available here")); };
    document.head.append(s);
  });
  async function snapshot() {
    await fc(); await loadHelper().catch(() => {});
    if (window.filmcraftAgentSnapshot) return window.filmcraftAgentSnapshot();
    return { sequence: await exec("sequence.inspect", {}), files: await window.filmcraft.files() };
  }

  const obj = (props, required) => ({ type: "object", properties: props || {}, required: required || [], additionalProperties: false });
  const tools = [];
  const tool = (name, description, inputSchema, run, opts) => tools.push(Object.assign({
    name, description, inputSchema,
    async execute(input, client) { try { return await run(input || {}, client); } catch (e) { return fail(e); } },
  }, opts && opts.readOnly ? { annotations: { readOnlyHint: true } } : {}));

  tool("filmcraft.snapshot", "Compact JSON of the open FilmCraft project: tracks with clips (ids, names, start/end/in/out in seconds), media, markers, selection, playhead and duration. Call this before editing to get clip ids.",
    obj(), async () => text(await snapshot()), { readOnly: true });

  tool("filmcraft.ask", "Describe an edit in plain language (e.g. 'split the second clip at 5 seconds and add a fade'). Grepawk's FilmCraft agent plans the command calls and runs them in this page. Destructive steps ask the user first.",
    obj({ prompt: { type: "string", description: "The edit to make" } }, ["prompt"]), async ({ prompt }, client) => {
      await fc(); await loadHelper();
      const f = window.filmcraft, orig = f.execute;
      f.execute = async function (id, params) {
        if (DESTRUCTIVE.test(String(id)) && !(await confirmUser(client, `The FilmCraft agent wants to run "${id}". Allow?`)))
          throw new Error("user declined " + id);
        return orig.call(this, id, params);
      };
      try {
        const r = await window.filmcraftAgent(String(prompt));
        return text({ done: r.done, reply: r.reply || r.message, steps: (r.history || []).map((h) => ({ calls: h.calls, results: h.results.map((x) => x.ok ? "ok" : x.error) })) });
      } finally { f.execute = orig; }
    });

  tool("filmcraft.list_commands", "Search FilmCraft's core engine commands (about 650: edit, timeline, clip, sequence, markers, effects, captions, graphics, export...). Returns ids, descriptions and params ({name: type}, '!' = required, 'ticks' = seconds*254016000000). Use with filmcraft.execute.",
    obj({ query: { type: "string", description: "Substring or regex to match id/description, e.g. 'marker' or '^timeline\\.'" }, limit: { type: "number", description: "Max results (default 40)" } }),
    async ({ query, limit }) => {
      const all = await loadCatalog(); let re;
      try { re = new RegExp(query || "", "i"); } catch (e) { re = { test: (s) => s.toLowerCase().includes(String(query).toLowerCase()) }; }
      const hits = all.filter((c) => re.test(c.id) || re.test(c.d));
      return text({ total: hits.length, commands: hits.slice(0, limit || 40) });
    }, { readOnly: true });

  tool("filmcraft.execute", "Run any FilmCraft engine command by id with params (see filmcraft.list_commands for ids and params). Destructive commands (delete, remove, clear, close, reset, export) ask the user to confirm first.",
    obj({ command: { type: "string", description: "Command id, e.g. timeline.razor" }, params: { type: "object", description: "Command params", additionalProperties: true } }, ["command"]),
    async ({ command, params }, client) => {
      const run = async () => text(await exec(String(command), params || {}));
      return DESTRUCTIVE.test(String(command)) ? guard(client, `Allow the AI agent to run FilmCraft command "${command}"?`, run) : run();
    });

  // Curated top tools (seconds instead of ticks).
  const ids = { type: "array", items: { type: "string" }, description: "Clip ids from filmcraft.snapshot (default: current selection)" };
  tool("filmcraft.undo", "Undo the last edit.", obj(), async () => text(await exec("edit.undo")));
  tool("filmcraft.redo", "Redo the last undone edit.", obj(), async () => text(await exec("edit.redo")));
  tool("filmcraft.set_playhead", "Move the playhead to a time in seconds.", obj({ seconds: { type: "number" } }, ["seconds"]),
    async ({ seconds }) => text(await exec("playhead.set", { time: ticks(seconds) })));
  tool("filmcraft.select_clips", "Select clips by id (replaces the selection unless add is true).", obj({ clips: Object.assign({}, ids, { description: "Clip ids" }), add: { type: "boolean" } }, ["clips"]),
    async ({ clips, add }) => text(await exec("timeline.select", { clips, add: !!add, toggle: false })));
  tool("filmcraft.split_at", "Razor (split) clips at a time in seconds, optionally on one track (e.g. 'V1') or one clip.",
    obj({ seconds: { type: "number" }, track: { type: "string" }, clip: { type: "string" } }, ["seconds"]),
    async ({ seconds, track, clip }) => text(await exec("timeline.razor", Object.assign({ time: ticks(seconds) }, track ? { track } : {}, clip ? { clip } : {}))));
  tool("filmcraft.add_marker", "Add a sequence marker at a time in seconds (default: playhead).",
    obj({ seconds: { type: "number" }, name: { type: "string" }, comment: { type: "string" } }),
    async ({ seconds, name, comment }) => text(await exec("markers.add", Object.assign({ target: "program" }, seconds !== undefined ? { time: ticks(seconds) } : {}, name ? { name } : {}, comment ? { comment } : {}))));
  tool("filmcraft.rename_clip", "Rename a clip.", obj({ clip: { type: "string" }, name: { type: "string" } }, ["clip", "name"]),
    async ({ clip, name }) => text(await exec("clip.rename", { clip, name })));
  tool("filmcraft.add_transition", "Apply a video transition (default Cross Dissolve) to a clip edge.",
    obj({ clip: { type: "string" }, effect: { type: "string", description: "e.g. cross_dissolve, dip_to_black" }, edge: { type: "string", enum: ["in", "out"] }, frames: { type: "number" } }),
    async (a) => text(await exec("sequence.applyVideoTransition", a)));
  tool("filmcraft.apply_effect", "Apply a video/audio effect (e.g. gaussian_blur) to clips.", obj({ effect: { type: "string" }, clips: ids }, ["effect"]),
    async (a) => text(await exec("effects.apply", a)));
  tool("filmcraft.add_caption", "Add a caption at a time in seconds.", obj({ text: { type: "string" }, seconds: { type: "number" }, durationSeconds: { type: "number" } }, ["text"]),
    async (a) => text(await exec("captions.add", a)));
  tool("filmcraft.delete_clips", "Delete clips (leaves gaps; set ripple to close them). Asks the user to confirm.",
    obj({ clips: ids, ripple: { type: "boolean" } }),
    async ({ clips, ripple }, client) => guard(client, `Delete ${clips && clips.length ? clips.length + " clip(s)" : "the selected clips"}${ripple ? " (ripple)" : ""}?`,
      async () => text(await exec(ripple ? "edit.rippleDelete" : "edit.clear", clips ? { clips } : {}))));
  tool("filmcraft.export", "Export the sequence to a video file in the in-browser file system (e.g. /files/out.mp4). Asks the user to confirm.",
    obj({ path: { type: "string", description: "Output path, default /files/export.mp4" }, format: { type: "string", description: "h264 (default), hevc, prores, gif, wav..." }, quality: { type: "number", description: "0..100, default 80" } }),
    async ({ path, format, quality }, client) => guard(client, `Export the sequence to ${path || "/files/export.mp4"}?`,
      async () => text(await exec("file.exportMedia", { path: path || "/files/export.mp4", format: format || "h264", quality: quality === undefined ? 80 : quality }))));
  tool("filmcraft.list_files", "List files in FilmCraft's in-browser file system.", obj(), async () => text(await (await fc()).files()), { readOnly: true });

  // Register. Current spec: registerTool(tool, {signal}) (may return a promise). Older drafts: provideContext({tools}).
  const controller = new AbortController();
  const registered = [];
  (async () => {
    if (typeof ctx.registerTool === "function") {
      for (const t of tools) {
        try { await ctx.registerTool(t, { signal: controller.signal }); registered.push(t.name); }
        catch (e) { try { await ctx.registerTool(t); registered.push(t.name); } catch (e2) { console.warn("[webmcp] registerTool failed", t.name, e2); } }
      }
    } else if (typeof ctx.provideContext === "function") {
      try { await ctx.provideContext({ tools }); tools.forEach((t) => registered.push(t.name)); } catch (e) { console.warn("[webmcp] provideContext failed", e); }
    }
  })();
  window.filmcraftWebMCP = { tools, registered, unregister: () => { controller.abort(); try { ctx.unregisterTool && tools.forEach((t) => ctx.unregisterTool(t.name)); } catch (e) {} } };
})();
