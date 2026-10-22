// FilmCraft prompt bar: type an instruction (Enter) or talk (mic) -> grepawk's /api/filmcraft/agent plans
// window.filmcraft calls, the helper (/filmcraft-agent.js, from finalcut) runs them. Voice uses an
// ElevenLabs Conversational AI agent whose client tool edit_video(instruction) runs the same loop.
// Off switch: ?noagent hides it. Silently absent when the helper isn't served (e.g. local `xtask web --serve`).
(() => {
  if (new URLSearchParams(location.search).has("noagent")) return;
  const HELPER = "/filmcraft-agent.js", TOKEN = "/api/filmcraft/voice/token";
  const css = `
#fc-agent{position:fixed;left:50%;bottom:10px;transform:translateX(-50%);z-index:5;width:min(560px,calc(100vw - 24px));
  font:13px/1.4 system-ui,sans-serif;color:#e6e6e8;opacity:.55;transition:opacity .15s}
#fc-agent:hover,#fc-agent:focus-within,#fc-agent.busy{opacity:1}
#fc-agent .row{display:flex;gap:6px;background:rgba(38,39,43,.94);border:1px solid #3a3b40;border-radius:8px;padding:4px}
#fc-agent input{flex:1;min-width:0;background:transparent;border:0;outline:0;color:inherit;font:inherit;padding:4px 6px}
#fc-agent button{font:inherit;border:0;border-radius:6px;background:#33353a;color:#e6e6e8;cursor:pointer;padding:3px 9px}
#fc-agent button.on{background:#c0392b}
#fc-agent .log{display:none;max-height:28vh;overflow:auto;margin-bottom:4px;background:rgba(30,31,34,.94);border-radius:8px;padding:6px 8px;white-space:pre-wrap}
#fc-agent.open .log{display:block}
#fc-agent .err{color:#ff8a7a}#fc-agent .ok{color:#8fd19e}#fc-agent .dim{color:#9a9aa0}`;
  let helper;
  const loadHelper = () => helper ||= new Promise((ok, bad) => {
    if (window.filmcraftAgent) return ok();
    const s = document.createElement("script"); s.src = HELPER; s.onload = () => window.filmcraftAgent ? ok() : bad(new Error("agent helper missing"));
    s.onerror = () => { helper = null; bad(new Error("agent endpoint not available here")); }; document.head.append(s);
  });

  function mount() {
    const st = document.createElement("style"); st.textContent = css; document.head.append(st);
    const box = document.createElement("div"); box.id = "fc-agent";
    box.innerHTML = `<div class="log" aria-live="polite"></div><div class="row">
      <input type="text" placeholder="Ask FilmCraft to edit… (Enter)" aria-label="Edit instruction" maxlength="2000">
      <button type="button" class="mic" title="Talk to edit (voice)" aria-label="Voice">🎤</button>
      <button type="button" class="tog" title="Show/hide log" aria-label="Log">≡</button></div>`;
    document.body.append(box);
    const input = box.querySelector("input"), log = box.querySelector(".log"), mic = box.querySelector(".mic");
    const line = (text, cls = "") => { const d = document.createElement("div"); d.className = cls; d.textContent = text; log.append(d);
      while (log.childNodes.length > 200) log.firstChild.remove(); log.scrollTop = log.scrollHeight; box.classList.add("open"); return d; };
    box.querySelector(".tog").onclick = () => box.classList.toggle("open");
    // keep keystrokes away from the egui canvas while typing
    for (const t of ["keydown", "keyup", "keypress", "paste", "copy", "cut"]) input.addEventListener(t, (e) => e.stopPropagation());

    let busy = false;
    async function run(prompt) {
      if (busy) throw new Error("Still working on the previous instruction.");
      if (!window.filmcraft) throw new Error("FilmCraft is still loading.");
      busy = true; box.classList.add("busy");
      line("› " + prompt);
      const status = line("planning…", "dim");
      try {
        await loadHelper();
        const r = await window.filmcraftAgent(prompt);
        (r.history || []).forEach((h, i) => (h.calls || []).forEach((c, j) => {
          const res = h.results && h.results[j], bad = res && (res.ok === false || res.error);
          line(`  ${i + 1}. ${c.kind === "request" ? c.method : c.id}${bad ? " ✗ " + (res.error || "failed") : " ✓"}`, bad ? "err" : "dim");
        }));
        (r.rejected || []).forEach((x) => line("  skipped: " + (x.reason || "rejected"), "dim"));
        const msg = r.explanation || (r.error ? "Error: " + r.error : "Done.");
        status.textContent = msg; status.className = r.error ? "err" : "ok";
        return msg;
      } catch (e) {
        status.textContent = "Error: " + (e && e.message || e); status.className = "err";
        throw e;
      } finally { busy = false; box.classList.remove("busy"); }
    }
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && input.value.trim() && !e.isComposing) { const p = input.value.trim(); input.value = ""; run(p).catch(() => {}); }
      else if (e.key === "Escape") input.blur();
    });

    let convo = null;
    mic.onclick = async () => {
      if (convo) { const c = convo; convo = null; mic.classList.remove("on"); await c.endSession().catch(() => {}); line("voice off", "dim"); return; }
      mic.classList.add("on");
      try {
        const tr = await fetch(TOKEN, { cache: "no-store" });
        if (!tr.ok) throw new Error(tr.status === 503 ? "voice is not configured on the server yet" : "voice token failed (" + tr.status + ")");
        const { signedUrl } = await tr.json();
        const { Conversation } = await import("./elevenlabs-client.js");
        convo = await Conversation.startSession({
          signedUrl, connectionType: "websocket",
          clientTools: { edit_video: async ({ instruction }) => {
            try { return await run(String(instruction || "")); } catch (e) { return "Failed: " + (e && e.message || e); }
          } },
          onConnect: () => line("voice on — speak your edit", "dim"),
          onDisconnect: () => { convo = null; mic.classList.remove("on"); },
          onError: (m) => line("voice error: " + m, "err"),
          onMessage: ({ source, message }) => message && line((source === "user" ? "🗣 " : "🔊 ") + message, "dim"),
        });
      } catch (e) { convo = null; mic.classList.remove("on"); line("Voice: " + (e && e.message || e), "err"); }
    };
    window.filmcraftPrompt = run; // console/automation: await filmcraftPrompt("…")
  }
  if (document.body) mount(); else addEventListener("DOMContentLoaded", mount);
})();
