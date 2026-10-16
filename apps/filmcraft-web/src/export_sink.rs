//! Streamed export output (`filmcraft_export::StreamTarget`) over the page's `__fcExportSink`
//! (`web/export-sink.js`): the muxer's bytes go out in chunks as the export runs, to an OPFS file
//! or to JS-side chunks, so a finished export never sits in wasm memory (and long ones are not
//! bounded by it). When the file is complete it is registered at its virtual path (as a Blob:
//! `filmcraft.readFile` works) and offered as a download, all without copying it back into wasm.
//!
//! Sink kinds (`file.exportMedia` `sink`): `opfs` (disk-backed), `blob` (JS memory), `auto` (OPFS
//! when the browser has it, else blob), `memory` (the old path: buffered in wasm). Like the encoder,
//! the JS state lives in a thread-local table and the target holds only its id.

use std::cell::RefCell;
use std::collections::HashMap;

use filmcraft_export::StreamTarget;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

thread_local! {
    /// sink id → (virtual path, kind)
    static OPEN: RefCell<HashMap<u32, (String, String)>> = RefCell::new(HashMap::new());
    static LAST_KIND: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn api() -> Option<JsValue> {
    js_sys::Reflect::get(&js_sys::global(), &"__fcExportSink".into()).ok().filter(|v| v.is_object())
}

fn call(o: &JsValue, method: &str, args: &[&JsValue]) -> Result<JsValue, JsValue> {
    let f: js_sys::Function = js_sys::Reflect::get(o, &method.into())?.dyn_into()?;
    let a = js_sys::Array::new();
    for x in args {
        a.push(x);
    }
    f.apply(o, &a)
}

fn io(e: impl std::fmt::Debug) -> std::io::Error {
    std::io::Error::other(format!("{e:?}"))
}

/// Register the stream target with the exporter.
pub fn install() {
    if api().is_some() {
        filmcraft_export::register_stream_target(open);
    }
}

/// The sink kind of the latest export (`filmcraft.info().export.sink`).
pub fn last_kind() -> Option<String> {
    LAST_KIND.with(|k| k.borrow().clone())
}

fn open(path: &str, kind: &str) -> Option<std::io::Result<Box<dyn StreamTarget>>> {
    let api = api()?;
    let kind = match kind {
        "opfs" | "blob" => kind,
        _ if crate::opfs::available() => "opfs",
        _ => "blob",
    };
    let name: String = path.rsplit('/').next().unwrap_or("export").chars().map(|c| if c.is_alphanumeric() || ".-_ ".contains(c) { c } else { '_' }).collect();
    let id = match call(&api, "open", &[&name.into(), &kind.into()]).map(|v| v.as_f64()) {
        Ok(Some(id)) if id >= 1.0 && id <= f64::from(u32::MAX) => id as u32,
        Ok(_) => return Some(Err(std::io::Error::other("export sink: no id"))),
        Err(e) => return Some(Err(io(e))),
    };
    OPEN.with(|o| o.borrow_mut().insert(id, (path.to_string(), kind.to_string())));
    LAST_KIND.with(|k| *k.borrow_mut() = Some(kind.to_string()));
    Some(Ok(Box::new(WebStream { id })))
}

/// One open sink (its JS state lives in `__fcExportSink`).
struct WebStream {
    id: u32,
}

impl StreamTarget for WebStream {
    fn write_at(&mut self, position: u64, data: &[u8]) -> std::io::Result<()> {
        let api = api().ok_or_else(|| std::io::Error::other("export sink is gone"))?;
        // a fresh JS buffer (wasm memory may be shared, and the write completes later)
        let bytes = js_sys::Uint8Array::from(data);
        let r = call(&api, "write", &[&JsValue::from(self.id), &JsValue::from(position as f64), &bytes]).map_err(io)?;
        match r.as_string() {
            Some(err) => Err(std::io::Error::other(format!("writing the export failed: {err}"))),
            None => Ok(()),
        }
    }

    fn finish(self: Box<Self>, length: u64) -> std::io::Result<()> {
        let api = api().ok_or_else(|| std::io::Error::other("export sink is gone"))?;
        let (path, _kind) = OPEN.with(|o| o.borrow_mut().remove(&self.id)).ok_or_else(|| std::io::Error::other("unknown export sink"))?;
        // an error recorded by an earlier write fails the export now, before reporting success
        let status = call(&api, "status", &[&JsValue::from(self.id)]).map_err(io)?;
        if let Some(err) = js_sys::Reflect::get(&status, &"error".into()).ok().and_then(|e| e.as_string()) {
            return Err(std::io::Error::other(format!("writing the export failed: {err}")));
        }
        let p: js_sys::Promise = call(&api, "finish", &[&JsValue::from(self.id), &JsValue::from(length as f64)]).map_err(io)?.dyn_into().map_err(io)?;
        wasm_bindgen_futures::spawn_local(async move {
            match JsFuture::from(p).await.and_then(|b| b.dyn_into::<web_sys::Blob>()) {
                Ok(blob) => {
                    crate::fs::put_blob(&path, blob.clone());
                    if let Err(e) = crate::fs::download_blob(&path, &blob) {
                        log::warn!("download {path}: {e:?}");
                    }
                }
                Err(e) => log::error!("finishing the export {path} failed: {e:?}"),
            }
            crate::repaint();
        });
        Ok(())
    }
}

impl Drop for WebStream {
    fn drop(&mut self) {
        // dropped without finishing (cancelled / failed export): discard the partial file
        if OPEN.with(|o| o.borrow_mut().remove(&self.id)).is_some()
            && let Some(api) = api()
        {
            let _ = call(&api, "abort", &[&JsValue::from(self.id)]);
        }
    }
}
