// Streaming export sinks (docs/export-optimize-gpu.md phase 3). The wasm side writes positioned
// chunks as the export runs (`write(id, position, bytes)`, synchronously: the async work is
// chained here), so a finished file never has to sit in wasm memory.
//
//   kind "opfs":  an OPFS file written through FileSystemWritableFileStream; `finish` resolves with
//                 the OPFS File (disk-backed: downloading it copies nothing through memory).
//   kind "blob":  chunks kept as JS buffers (outside wasm memory); `finish` resolves with a Blob.
//
// Every failure is recorded (`status(id).error`) and reported to the next `write` / `finish`.
const sinks = new Map();
let nextId = 1;

function open(name, kind) {
  const id = nextId++;
  const s = { kind, name, error: null, queued: 0, written: 0, chunks: [], chain: null, handle: null };
  if (kind === "opfs") {
    s.chain = (async () => {
      const root = await navigator.storage.getDirectory();
      const dir = await root.getDirectoryHandle("exports", { create: true });
      // a file left by an earlier session of the same name is replaced
      s.handle = await dir.getFileHandle(name, { create: true });
      return await s.handle.createWritable({ keepExistingData: false });
    })();
    s.chain.catch((e) => { s.error = String(e?.message ?? e); });
  }
  sinks.set(id, s);
  return id;
}

/// `bytes` is copied by the caller (a fresh, non-shared Uint8Array) and owned from here on.
function write(id, position, bytes) {
  const s = sinks.get(id);
  if (!s) return "unknown sink";
  if (s.error) return s.error;
  if (s.kind === "opfs") {
    s.queued += bytes.byteLength;
    s.chain = s.chain.then(async (w) => {
      await w.write({ type: "write", position, data: bytes });
      s.queued -= bytes.byteLength;
      s.written += bytes.byteLength;
      return w;
    });
    s.chain.catch((e) => { s.error = String(e?.message ?? e); });
  } else {
    // a rewrite inside an earlier chunk (the mdat size patched at the end) is applied in place
    const end = position + bytes.byteLength;
    let covered = false;
    for (const c of s.chunks) {
      if (position >= c.position && end <= c.position + c.bytes.byteLength) {
        c.bytes.set(bytes, position - c.position);
        covered = true;
        break;
      }
    }
    if (!covered) {
      const last = s.chunks.at(-1);
      const expected = last ? last.position + last.bytes.byteLength : 0;
      if (position !== expected) {
        s.error = `blob sink: write at ${position}, expected ${expected} (only appends and in-place patches)`;
        return s.error;
      }
      s.chunks.push({ position, bytes });
    }
    s.written = Math.max(s.written, end);
  }
  return null;
}

async function finish(id, length) {
  const s = sinks.get(id);
  if (!s) throw new Error("unknown sink");
  try {
    if (s.kind === "opfs") {
      const w = await s.chain;
      await w.truncate(length);
      await w.close();
      if (s.error) throw new Error(s.error);
      return await s.handle.getFile();
    }
    if (s.error) throw new Error(s.error);
    return new Blob(s.chunks.map((c) => c.bytes), { type: "application/octet-stream" });
  } finally {
    sinks.delete(id);
  }
}

function abort(id) {
  const s = sinks.get(id);
  sinks.delete(id);
  if (s?.kind === "opfs") s.chain.then((w) => w.abort()).catch(() => {});
}

function status(id) {
  const s = sinks.get(id);
  return s ? { kind: s.kind, queued: s.queued, written: s.written, error: s.error } : null;
}

self.__fcExportSink = { open, write, finish, abort, status };
