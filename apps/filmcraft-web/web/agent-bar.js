// FilmCraft agent panel: a floating, draggable, resizable, collapsible panel. Type an instruction
// (Enter sends, Shift+Enter newline) or talk (mic) -> grepawk's /api/filmcraft/agent plans window.filmcraft
// calls and the finalcut helper (/filmcraft-agent.js) runs them. Voice: ElevenLabs agent whose client tool
// edit_video(instruction) runs the same loop. Off switch: ?noagent. Layout persists in localStorage.
(() => {
  if (new URLSearchParams(location.search).has("noagent")) return;
  const HELPER = "/filmcraft-agent.js", TOKEN = "/api/filmcraft/voice/token", KEY = "filmcraft.agentPanel.v1";
  const MIN_W = 320, MIN_H = 220, DEF_W = 420, DEF_H = 340, M = 8;
  const css = `
#fc-agent{position:fixed;z-index:20;display:flex;flex-direction:column;box-sizing:border-box;
  background:rgba(34,35,39,.97);border:1px solid #45464d;border-radius:10px;box-shadow:0 10px 30px rgba(0,0,0,.45);
  font:13px/1.45 system-ui,sans-serif;color:#e6e6e8;overflow:hidden}
#fc-agent *{box-sizing:border-box}
#fc-agent .hd{display:flex;align-items:center;gap:6px;padding:6px 8px 6px 12px;background:#2b2c31;cursor:move;user-select:none;border-bottom:1px solid #3a3b40}
#fc-agent .hd .t{flex:1;font-weight:600;font-size:12.5px;color:#cfd0d4}
#fc-agent .hd .st{font-size:11.5px;color:#9a9aa0}
#fc-agent button{font:inherit;border:0;border-radius:6px;background:#3a3c42;color:#e6e6e8;cursor:pointer;padding:4px 10px}
#fc-agent button:hover{background:#4a4c53}
#fc-agent button.on{background:#c0392b}
#fc-agent button.send{background:#3b6fe0}
#fc-agent .log{flex:1;min-height:40px;overflow:auto;padding:8px 12px;white-space:pre-wrap;word-break:break-word}
#fc-agent .log:empty::before{content:"Describe an edit, e.g. “split the first clip at 5 seconds and add a cross dissolve”.";color:#7d7e84}
#fc-agent .in{display:flex;flex-direction:column;gap:6px;padding:8px;border-top:1px solid #3a3b40}
#fc-agent textarea{width:100%;height:76px;resize:none;background:#1e1f22;border:1px solid #45464d;border-radius:6px;color:inherit;font:inherit;padding:6px 8px;outline:0}
#fc-agent textarea:focus{border-color:#3b6fe0}
#fc-agent .row{display:flex;gap:6px;align-items:center}
#fc-agent .row .hint{flex:1;color:#7d7e84;font-size:11.5px}
#fc-agent .rz{position:absolute;right:0;bottom:0;width:16px;height:16px;cursor:nwse-resize;
  background:linear-gradient(135deg,transparent 50%,#6a6b72 50%,#6a6b72 60%,transparent 60%,transparent 70%,#6a6b72 70%,#6a6b72 80%,transparent 80%)}
#fc-agent.collapsed{width:auto!important;height:auto!important;border-radius:20px}
#fc-agent.collapsed .log,#fc-agent.collapsed .in,#fc-agent.collapsed .clr,#fc-agent.collapsed .rz,#fc-agent.collapsed .hd .st{display:none}
#fc-agent.collapsed .hd{border:0;padding:6px 8px 6px 14px}
#fc-agent.busy .hd .st::before{content:"● ";color:#e0b13b}
#fc-agent .err{color:#ff8a7a}#fc-agent .ok{color:#8fd19e}#fc-agent .dim{color:#9a9aa0}#fc-agent .you{color:#cfe0ff;margin-top:6px}`;
  let helper;
  const loadHelper = () => helper ||= new Promise((ok, bad) => {
    if (window.filmcraftAgent) return ok();
    const s = document.createElement("script"); s.src = HELPER; s.onload = () => window.filmcraftAgent ? ok() : bad(new Error("agent helper missing"));
    s.onerror = () => { helper = null; bad(new Error("agent endpoint not available here")); }; document.head.append(s);
  });
  const load = () => { try { return JSON.parse(localStorage.getItem(KEY)) || {}; } catch { return {}; } };

  function mount() {
    const st = document.createElement("style"); st.textContent = css; document.head.append(st);
    const box = document.createElement("div"); box.id = "fc-agent"; box.setAttribute("role", "dialog"); box.setAttribute("aria-label", "FilmCraft assistant");
    box.innerHTML = `<div class="hd" title="Drag to move"><span class="t">✦ FilmCraft assistant</span><span class="st"></span>
        <button type="button" class="mic" title="Talk to edit (voice)" aria-label="Voice">🎤</button>
        <button type="button" class="clr" title="Clear log" aria-label="Clear log">⌫</button>
        <button type="button" class="tog" title="Collapse" aria-label="Collapse">–</button></div>
      <div class="log" aria-live="polite"></div>
      <div class="in"><textarea placeholder="Ask FilmCraft to edit…" aria-label="Edit instruction" maxlength="2000"></textarea>
        <div class="row"><span class="hint">Enter to send · Shift+Enter for a new line</span><button type="button" class="send">Send</button></div></div>
      <div class="rz" title="Resize"></div>`;
    document.body.append(box);
    const $ = (s) => box.querySelector(s);
    const input = $("textarea"), log = $(".log"), mic = $(".mic"), tog = $(".tog"), status = $(".hd .st"), hd = $(".hd");
    const s = Object.assign({ w: DEF_W, h: DEF_H, collapsed: false }, load());
    const save = () => { try { localStorage.setItem(KEY, JSON.stringify(s)); } catch {} };

    function apply() {
      box.classList.toggle("collapsed", s.collapsed);
      tog.textContent = s.collapsed ? "▢" : "–"; tog.title = s.collapsed ? "Expand" : "Collapse"; tog.setAttribute("aria-label", tog.title);
      const vw = innerWidth, vh = innerHeight;
      s.w = Math.max(MIN_W, Math.min(s.w, vw - 2 * M)); s.h = Math.max(MIN_H, Math.min(s.h, vh - 2 * M));
      if (!s.collapsed) { box.style.width = s.w + "px"; box.style.height = s.h + "px"; }
      const r = box.getBoundingClientRect();
      // default: bottom-right corner, above the status bar
      if (s.x == null || s.y == null) { s.x = vw - r.width - 16; s.y = vh - r.height - 32; }
      s.x = Math.max(M, Math.min(s.x, vw - r.width - M)); s.y = Math.max(M, Math.min(s.y, vh - r.height - M));
      box.style.left = s.x + "px"; box.style.top = s.y + "px";
    }
    apply();
    addEventListener("resize", apply);

    function drag(el, onMove) {
      el.addEventListener("pointerdown", (e) => {
        if (e.button !== 0 || e.target.closest("button")) return;
        e.preventDefault(); e.stopPropagation(); el.setPointerCapture(e.pointerId);
        const start = { px: e.clientX, py: e.clientY, x: s.x, y: s.y, w: s.w, h: s.h }; let moved = false;
        const mv = (ev) => { moved = true; onMove(ev.clientX - start.px, ev.clientY - start.py, start); apply(); };
        const up = () => { el.removeEventListener("pointermove", mv); el.removeEventListener("pointerup", up); el.removeEventListener("pointercancel", up); if (moved) save(); };
        el.addEventListener("pointermove", mv); el.addEventListener("pointerup", up); el.addEventListener("pointercancel", up);
      });
    }
    drag(hd, (dx, dy, o) => { s.x = o.x + dx; s.y = o.y + dy; });
    drag($(".rz"), (dx, dy, o) => { s.w = o.w + dx; s.h = o.h + dy; });
    hd.addEventListener("dblclick", (e) => { if (!e.target.closest("button")) toggle(); });
    function toggle() {
      // keep the right/bottom edge anchored so the pill stays where the panel was (bottom-right default)
      const r0 = box.getBoundingClientRect();
      s.collapsed = !s.collapsed; apply();
      const r1 = box.getBoundingClientRect();
      s.x += r0.right - r1.right; s.y += r0.bottom - r1.bottom; apply(); save();
      if (!s.collapsed) input.focus();
    }
    tog.onclick = toggle;
    $(".clr").onclick = () => { log.textContent = ""; };

    const line = (text, cls = "") => { const d = document.createElement("div"); d.className = cls; d.textContent = text; log.append(d);
      while (log.childNodes.length > 300) log.firstChild.remove(); log.scrollTop = log.scrollHeight; return d; };
    // keep keystrokes / clipboard / wheel away from the egui canvas while in the panel
    for (const t of ["keydown", "keyup", "keypress", "paste", "copy", "cut", "wheel"]) box.addEventListener(t, (e) => e.stopPropagation());
    for (const t of ["pointerdown", "mousedown", "touchstart"]) box.addEventListener(t, (e) => e.stopPropagation());

    let busy = false;
    async function run(prompt) {
      if (busy) throw new Error("Still working on the previous instruction.");
      if (!window.filmcraft) throw new Error("FilmCraft is still loading.");
      busy = true; box.classList.add("busy"); status.textContent = "working…";
      if (s.collapsed) toggle();
      line("› " + prompt, "you");
      const st = line("planning…", "dim");
      try {
        await loadHelper();
        const r = await window.filmcraftAgent(prompt);
        (r.history || []).forEach((h, i) => (h.calls || []).forEach((c, j) => {
          const res = h.results && h.results[j], bad = res && (res.ok === false || res.error);
          line(`  ${i + 1}. ${c.kind === "request" ? c.method : c.id}${bad ? " ✗ " + (res.error || "failed") : res ? " ✓" : " (not run)"}`, bad ? "err" : "dim");
        }));
        (r.rejected || []).forEach((x) => line("  skipped: " + (x.reason || "rejected"), "dim"));
        const msg = r.explanation || (r.error ? "Error: " + r.error : "Done.");
        st.textContent = msg; st.className = r.error ? "err" : "ok"; log.append(st); log.scrollTop = log.scrollHeight;
        return msg;
      } catch (e) {
        st.textContent = "Error: " + (e && e.message || e); st.className = "err";
        throw e;
      } finally { busy = false; box.classList.remove("busy"); status.textContent = ""; }
    }
    const send = () => { const p = input.value.trim(); if (!p || busy) return; input.value = ""; run(p).catch(() => {}); };
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); send(); }
      else if (e.key === "Escape") input.blur();
    });
    $(".send").onclick = send;

    let convo = null;
    mic.onclick = async () => {
      if (convo) { const c = convo; convo = null; mic.classList.remove("on"); await c.endSession().catch(() => {}); line("voice off", "dim"); return; }
      mic.classList.add("on");
      if (s.collapsed) toggle();
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
