// Open Folder… for the web build. Chromium: FileSystemDirectoryHandle/FileSystemFileHandle are the source of truth
// (registerFolderHandles; the engine calls getFile() per chunk read, so disk edits are seen and no File is held).
// (File ▸ Import From ▸ Folder…, Media Browser ▸ Local Drives ▸ Open Folder…,
// Import ▸ Folder…). The engine calls window.filmcraftOpenFolder(); files are registered lazily with
// filmcraft.registerFolder(name, [[relPath, File], …]) under /files/<name>/… (nothing is read or uploaded:
// the engine fetches 1 MiB chunks on demand). Chromium: showDirectoryPicker, handle kept in IndexedDB
// (Local Drives / Recent; permission asked again on reopen). Elsewhere: <input type=file webkitdirectory>.
(() => {
  const MEDIA = /\.(mp4|m4v|mov|qt|mkv|webm|avi|mxf|mts|m2ts|ts|mpg|mpeg|3gp|wmv|flv|ogv|mp3|wav|aif|aiff|aac|m4a|flac|ogg|oga|opus|wma|caf|png|jpe?g|gif|bmp|tiff?|webp|heic|heif|exr|dpx|tga|psd|svg|srt|vtt|scc|stl|xml|fcpxml|aaf|edl|fcproj|cube|3dl)$/i;
  const MAX_FILES = 20000, DB = "filmcraft-folders", STORE = "handles", KEEP = 8;

  const idb = () => new Promise((ok, bad) => { const r = indexedDB.open(DB, 1); r.onupgradeneeded = () => r.result.createObjectStore(STORE); r.onsuccess = () => ok(r.result); r.onerror = () => bad(r.error); });
  async function tx(mode, fn) { const db = await idb(); return new Promise((ok, bad) => { const t = db.transaction(STORE, mode); const r = fn(t.objectStore(STORE)); t.oncomplete = () => ok(r && r.result); t.onerror = () => bad(t.error); }); }
  async function remember(handle) {
    try {
      const all = (await tx("readonly", s => s.getAll())) || [];
      const keys = (await tx("readonly", s => s.getAllKeys())) || [];
      const rec = { name: handle.name, handle, at: Date.now() };
      await tx("readwrite", s => s.put(rec, handle.name));
      const old = all.filter(r => r.name !== handle.name).sort((a, b) => b.at - a.at).slice(KEEP - 1);
      for (const r of old) if (keys.includes(r.name)) await tx("readwrite", s => s.delete(r.name));
    } catch (e) { console.warn("folder-open: could not remember folder", e); }
  }
  const stored = async () => { try { return ((await tx("readonly", s => s.getAll())) || []).sort((a, b) => b.at - a.at); } catch { return []; } };

  async function walk(dir, prefix, out) {
    for await (const [name, h] of dir.entries()) {
      if (out.length >= MAX_FILES) return;
      if (name.startsWith(".")) continue;
      if (h.kind === "directory") await walk(h, prefix + name + "/", out);
      else if (MEDIA.test(name)) { const f = await h.getFile(); out.push([prefix + name, h, f.size, f.lastModified]); } // size/stamp only; the handle is kept, not the File
    }
  }
  async function show(name, entries, handles) {
    const fc = window.filmcraft;
    const r = handles ? fc.registerFolderHandles(name, entries) : fc.registerFolder(name, entries);
    try { await fc.request("ui.panel.show", { panel: "MediaBrowser" }); } catch {}
    try { await fc.execute("mediaBrowser.navigate", { path: r.dir }); } catch {}
    return r;
  }
  async function fromHandle(handle) {
    const entries = []; await walk(handle, "", entries);
    return show(handle.name, entries, true);
  }
  function viaInput() {
    return new Promise((ok) => {
      const i = document.createElement("input");
      i.type = "file"; i.webkitdirectory = true; i.multiple = true; i.hidden = true; i.id = "fc-folder-input";
      i.onchange = async () => {
        const files = [...(i.files || [])]; i.remove();
        if (!files.length) return ok(null);
        const root = (files[0].webkitRelativePath || files[0].name).split("/")[0];
        const entries = files.filter(f => MEDIA.test(f.name) && !/(^|\/)\./.test(f.webkitRelativePath || ""))
          .slice(0, MAX_FILES).map(f => [(f.webkitRelativePath || f.name).split("/").slice(1).join("/") || f.name, f]);
        ok(await show(root, entries));
      };
      i.oncancel = () => { i.remove(); ok(null); };
      document.body.append(i); i.click();
    });
  }
  window.filmcraftOpenFolder = async () => {
    if (typeof window.showDirectoryPicker === "function") {
      let handle;
      try { handle = await window.showDirectoryPicker({ id: "filmcraft-media", mode: "read" }); } catch { return null; } // cancelled
      await remember(handle);
      return fromHandle(handle);
    }
    return viaInput();
  };
  // A remembered folder clicked under Local Drives: ask for read permission again, then load it.
  window.filmcraftReopenFolder = async (name) => {
    const rec = (await stored()).find(r => r.name === name);
    if (!rec) return null;
    try {
      let p = await rec.handle.queryPermission({ mode: "read" });
      if (p !== "granted") p = await rec.handle.requestPermission({ mode: "read" });
      if (p !== "granted") return null;
    } catch { return null; }
    await remember(rec.handle);
    return fromHandle(rec.handle);
  };
  // List remembered folders under Local Drives once the app is up.
  (async function listRemembered() {
    for (let i = 0; i < 300 && !(window.filmcraft && window.filmcraft.addFolderRoot); i++) await new Promise(r => setTimeout(r, 200));
    if (!window.filmcraft || !window.filmcraft.addFolderRoot || !window.indexedDB) return;
    for (const r of (await stored()).reverse()) window.filmcraft.addFolderRoot(r.name);
  })();
})();
