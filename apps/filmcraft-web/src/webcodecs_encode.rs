//! H.264 / HEVC export through the browser's WebCodecs `VideoEncoder` (hardware where the
//! browser has it), behind `filmcraft_export::VideoEncoder`, with the built-in H.264 encoder as the
//! fallback.
//!
//! - **Selection.** Registered ahead of the built-in encoder. It takes an export when
//!   `hardware_encoding` is `Auto` (the web default; `file.exportMedia` `encoder: auto|hardware`),
//!   the codec/size passed the start-up `VideoEncoder.isConfigSupported` probe with
//!   `prefer-hardware`, and the settings are ones it handles; otherwise it declines and the
//!   built-in encoder runs (`encoder: hardware` makes that an error instead). It declines two-pass
//!   VBR, HDR, MXF (Annex B), odd sizes and non-square pixels, like the native hardware encoders.
//! - **Pictures.** The exporter's RGBA is converted to I420 with the same BT.709 limited-range
//!   conversion as the built-in and VideoToolbox paths (`rgba_to_yuv420_8`), and handed over as a
//!   `VideoFrame` tagged BT.709, so colours match the software export.
//! - **Asynchrony.** `encode` submits and returns the packets delivered so far; `ready` holds the
//!   exporter back while `encodeQueueSize` is over [`MAX_QUEUE`] or the codec config
//!   (`decoderConfig.description`, the avcC/hvcC record) has not arrived; `flush` starts the
//!   browser flush and `pending` stays true until it resolved and every chunk was polled. The
//!   stepped exporter yields between steps, so the callbacks run.
//! - **Threads.** The JS objects live in a thread-local table on the thread that created them;
//!   [`WebCodecsEncoder`] only holds its id (so it is `Send`). A call from another thread finds
//!   nothing and returns an error.
//! - **Errors.** An encoder `error` callback, a rejected flush or a chunk out of order becomes an
//!   `ExportError` naming the fix (export with the software encoder); nothing panics.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use filmcraft_export::{BitrateMode, EncodedPacket, EncoderFrame, ExportError, ExportSettings, Format, H264Pass, H264Profile, HardwareEncoding, VideoEncoder};
use filmcraft_isobmff::{AvcConfig, HevcConfig, SampleEntry};
use filmcraft_time::FrameRate;
use serde_json::json;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// Pictures allowed in the browser encoder's queue before the exporter waits.
const MAX_QUEUE: u32 = 8;
/// Pictures submitted but not yet delivered back before the exporter waits (bounds memory when
/// the output callbacks lag).
const MAX_IN_FLIGHT: u64 = 24;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Family {
    Avc,
    Hevc,
}

/// One delivered chunk.
struct Chunk {
    data: Vec<u8>,
    key: bool,
    timestamp_us: i64,
}

/// The JS side of one encoder (thread-local).
struct Js {
    encoder: JsValue,
    out: Rc<RefCell<VecDeque<Chunk>>>,
    description: Rc<RefCell<Option<Vec<u8>>>>,
    error: Rc<RefCell<Option<String>>>,
    flushed: Rc<Cell<bool>>,
    _closures: (Closure<dyn FnMut(JsValue, JsValue)>, Closure<dyn FnMut(JsValue)>),
}

#[derive(Default)]
struct Stats {
    sessions: u64,
    frames: u64,
    queue_max: u32,
    errors: u64,
    declined: u64,
    last_error: Option<String>,
    /// The encoder of the latest H.264 / HEVC export: "webcodecs-hw" or "h264enc".
    backend: Option<&'static str>,
}

thread_local! {
    /// (family, needs 4K) → supported with `prefer-hardware`, from the start-up probe.
    static SUPPORT: RefCell<HashMap<(Family, bool), bool>> = RefCell::new(HashMap::new());
    static ENCODERS: RefCell<HashMap<u32, Js>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u32> = const { Cell::new(1) };
    static STATS: RefCell<Stats> = RefCell::new(Stats::default());
}

fn get(o: &JsValue, k: &str) -> JsValue {
    js_sys::Reflect::get(o, &k.into()).unwrap_or(JsValue::UNDEFINED)
}

fn call(o: &JsValue, method: &str, args: &[&JsValue]) -> Result<JsValue, JsValue> {
    let f: js_sys::Function = get(o, method).dyn_into()?;
    let a = js_sys::Array::new();
    for x in args {
        a.push(x);
    }
    f.apply(o, &a)
}

fn obj(pairs: &[(&str, JsValue)]) -> JsValue {
    let o = js_sys::Object::new();
    for (k, v) in pairs {
        let _ = js_sys::Reflect::set(&o, &(*k).into(), v);
    }
    o.into()
}

fn js_err(e: &JsValue) -> String {
    e.as_string().or_else(|| get(e, "message").as_string()).unwrap_or_else(|| format!("{e:?}"))
}

/// The WebCodecs codec string for an export: H.264 profile + a level that fits the picture
/// rate, HEVC Main.
fn codec_string(family: Family, profile: H264Profile, w: u32, h: u32, fps: f64) -> String {
    // luma samples per second decide the level (H.264 Table A-1 / H.265 Table A.8, rounded up)
    let rate = f64::from(w) * f64::from(h) * fps;
    match family {
        Family::Avc => {
            let p = match profile {
                H264Profile::Baseline => "42e0",
                H264Profile::Main => "4d00",
                H264Profile::High => "6400",
            };
            let level = match rate {
                r if r <= 1920.0 * 1088.0 * 30.0 => 0x28, // 4.0
                r if r <= 1920.0 * 1088.0 * 60.0 => 0x2a, // 4.2
                r if r <= 4096.0 * 2304.0 * 30.0 => 0x33, // 5.1
                _ => 0x34,                                // 5.2
            };
            format!("avc1.{p}{level:02x}")
        }
        Family::Hevc => {
            let level = match rate {
                r if r <= 1920.0 * 1088.0 * 30.0 => 120, // 4.0
                r if r <= 1920.0 * 1088.0 * 60.0 => 123, // 4.1
                r if r <= 4096.0 * 2304.0 * 30.0 => 153, // 5.1
                _ => 156,                                // 5.2
            };
            format!("hvc1.1.6.L{level}.B0")
        }
    }
}

fn base_config(family: Family, w: u32, h: u32, fps: f64, bitrate_bps: f64, mode: &str, profile: H264Profile) -> JsValue {
    let mut pairs = vec![
        ("codec", JsValue::from_str(&codec_string(family, profile, w, h, fps))),
        ("width", w.into()),
        ("height", h.into()),
        ("framerate", fps.into()),
        ("hardwareAcceleration", "prefer-hardware".into()),
        ("latencyMode", "quality".into()),
        ("bitrateMode", mode.into()),
    ];
    if mode != "quantizer" {
        pairs.push(("bitrate", bitrate_bps.into()));
    }
    match family {
        Family::Avc => pairs.push(("avc", obj(&[("format", "avc".into())]))),
        Family::Hevc => pairs.push(("hevc", obj(&[("format", "hevc".into())]))),
    }
    obj(&pairs)
}

/// Probe which families the browser encodes in hardware (at 1080p and 4K). Called once at start-up.
pub async fn probe() {
    let Some(ve) = get(&js_sys::global(), "VideoEncoder").dyn_into::<js_sys::Function>().ok() else { return };
    for family in [Family::Avc, Family::Hevc] {
        for big in [false, true] {
            let (w, h) = if big { (3840, 2160) } else { (1920, 1080) };
            let cfg = base_config(family, w, h, 30.0, if big { 40e6 } else { 12e6 }, "variable", H264Profile::High);
            let ok = match call(&ve, "isConfigSupported", &[&cfg]).and_then(|p| p.dyn_into::<js_sys::Promise>()) {
                Ok(p) => JsFuture::from(p).await.map(|r| get(&r, "supported").as_bool() == Some(true)).unwrap_or(false),
                Err(_) => false,
            };
            SUPPORT.with(|s| s.borrow_mut().insert((family, big), ok));
        }
    }
    log::info!("WebCodecs hardware encoders: {:?}", SUPPORT.with(|s| s.borrow().clone()));
}

fn supported(family: Family, w: u32, h: u32) -> bool {
    let big = u64::from(w) * u64::from(h) > 1920 * 1088;
    if w > 4096 || h > 2304 {
        return false;
    }
    SUPPORT.with(|s| s.borrow().get(&(family, big)).copied().unwrap_or(false))
}

/// Whether the browser can export HEVC (the format list; there is no built-in HEVC encoder).
fn hevc_available() -> bool {
    supported(Family::Hevc, 1920, 1080)
}

/// Register the encoder factory (ahead of the built-in encoders) and the HEVC format probe.
pub fn install() {
    filmcraft_export::register_encoder(factory);
    filmcraft_export::register_format_probe(Format::Hevc, hevc_available);
}

/// Why WebCodecs does not take an export (None = it does).
fn decline_reason(format: Format, w: u32, h: u32, s: &ExportSettings) -> Option<String> {
    let family = match format {
        Format::H264 => Family::Avc,
        Format::Hevc => Family::Hevc,
        _ => return Some("not H.264 / HEVC".into()),
    };
    if s.hardware_encoding != HardwareEncoding::Auto {
        return Some("hardware encoding is off".into());
    }
    if s.format.is_mxf() {
        return Some("MXF carries an Annex B stream".into());
    }
    if s.bitrate_mode == BitrateMode::Vbr2Pass || !matches!(s.h264_pass, H264Pass::Single) {
        return Some("two-pass encoding".into());
    }
    if s.signal.is_hdr() {
        return Some("HDR".into());
    }
    if !w.is_multiple_of(2) || !h.is_multiple_of(2) || w == 0 || h == 0 {
        return Some("odd picture size".into());
    }
    if s.pixel_aspect.is_some_and(|(n, d)| n != d) {
        return Some("non-square pixels".into());
    }
    if !supported(family, w, h) {
        return Some(format!("the browser has no hardware {} encoder for {w}x{h}", format.label()));
    }
    None
}

fn factory(format: Format, w: u32, h: u32, rate: FrameRate, s: &ExportSettings) -> Option<filmcraft_export::Result<Box<dyn VideoEncoder>>> {
    if !matches!(format, Format::H264 | Format::Hevc) {
        return None;
    }
    if let Some(why) = decline_reason(format, w, h, s) {
        STATS.with(|st| st.borrow_mut().backend = Some(if format == Format::H264 { "h264enc" } else { "none" }));
        if s.hardware_encoding == HardwareEncoding::Auto {
            filmcraft_export::note_hw_encode_declined();
            STATS.with(|st| st.borrow_mut().declined += 1);
            log::info!("WebCodecs encoder declined: {why}");
        }
        return None;
    }
    STATS.with(|st| st.borrow_mut().backend = Some("webcodecs-hw"));
    Some(WebCodecsEncoder::new(format, w, h, rate, s).map(|e| Box::new(e) as Box<dyn VideoEncoder>))
}

pub struct WebCodecsEncoder {
    id: u32,
    family: Family,
    w: u32,
    h: u32,
    timescale: u32,
    frame_duration: u32,
    /// Microseconds per frame (WebCodecs timestamps).
    frame_us: f64,
    keyint: u64,
    /// Constant quality (`BitrateMode::Crf`): the per-frame quantizer.
    quantizer: Option<u32>,
    submitted: u64,
    delivered: u64,
    last_timestamp: Option<i64>,
    flush_started: bool,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    /// Y, U and V back to back (the `VideoFrame` buffer layout).
    i420: Vec<u8>,
}

impl WebCodecsEncoder {
    fn new(format: Format, w: u32, h: u32, rate: FrameRate, s: &ExportSettings) -> filmcraft_export::Result<Self> {
        let family = if format == Format::Hevc { Family::Hevc } else { Family::Avc };
        let timescale = u32::try_from(rate.num).ok().filter(|n| *n > 0).ok_or_else(|| ExportError::Encode("invalid frame rate".into()))?;
        let frame_duration = u32::try_from(rate.den).ok().filter(|d| *d > 0).ok_or_else(|| ExportError::Encode("invalid frame rate".into()))?;
        let fps = f64::from(timescale) / f64::from(frame_duration);
        let keyint = u64::from(s.keyframe_distance.filter(|k| *k > 0).unwrap_or_else(|| (fps * 2.0).round().max(1.0) as u32));
        let (mode, quantizer) = match s.bitrate_mode {
            BitrateMode::Cbr => ("constant", None),
            BitrateMode::Crf => ("quantizer", Some(s.crf.round().clamp(0.0, 51.0) as u32)),
            _ => ("variable", None),
        };
        let bitrate = f64::from(s.bitrate_kbps.max(100)) * 1000.0;
        let config = base_config(family, w, h, fps, bitrate, mode, s.h264_profile);

        let ve = get(&js_sys::global(), "VideoEncoder")
            .dyn_into::<js_sys::Function>()
            .map_err(|_| ExportError::Unsupported("this browser has no WebCodecs VideoEncoder".into()))?;
        let out: Rc<RefCell<VecDeque<Chunk>>> = Rc::default();
        let description: Rc<RefCell<Option<Vec<u8>>>> = Rc::default();
        let error: Rc<RefCell<Option<String>>> = Rc::default();
        let flushed = Rc::new(Cell::new(false));
        let (o, d) = (out.clone(), description.clone());
        let on_output = Closure::<dyn FnMut(JsValue, JsValue)>::new(move |chunk: JsValue, meta: JsValue| {
            let len = get(&chunk, "byteLength").as_f64().unwrap_or(0.0).max(0.0) as u32;
            let buf = js_sys::Uint8Array::new_with_length(len);
            if call(&chunk, "copyTo", &[&buf]).is_err() {
                return;
            }
            if d.borrow().is_none() {
                let desc = get(&get(&meta, "decoderConfig"), "description");
                if !desc.is_undefined() && !desc.is_null() {
                    // an ArrayBuffer or a view of one
                    let bytes = if desc.is_instance_of::<js_sys::ArrayBuffer>() {
                        js_sys::Uint8Array::new(&desc).to_vec()
                    } else {
                        js_sys::Uint8Array::new_with_byte_offset_and_length(
                            &get(&desc, "buffer"),
                            get(&desc, "byteOffset").as_f64().unwrap_or(0.0) as u32,
                            get(&desc, "byteLength").as_f64().unwrap_or(0.0) as u32,
                        )
                        .to_vec()
                    };
                    *d.borrow_mut() = Some(bytes);
                }
            }
            o.borrow_mut().push_back(Chunk {
                data: buf.to_vec(),
                key: get(&chunk, "type").as_string().as_deref() == Some("key"),
                timestamp_us: get(&chunk, "timestamp").as_f64().unwrap_or(0.0) as i64,
            });
            crate::repaint();
        });
        let e = error.clone();
        let on_error = Closure::<dyn FnMut(JsValue)>::new(move |err: JsValue| {
            let msg = js_err(&err);
            log::warn!("WebCodecs encoder error: {msg}");
            STATS.with(|st| {
                let mut st = st.borrow_mut();
                st.errors += 1;
                st.last_error = Some(msg.clone());
            });
            *e.borrow_mut() = Some(msg);
            crate::repaint();
        });
        let init = obj(&[("output", on_output.as_ref().clone()), ("error", on_error.as_ref().clone())]);
        let encoder =
            js_sys::Reflect::construct(&ve, &js_sys::Array::of1(&init)).map_err(|e| ExportError::Encode(format!("WebCodecs VideoEncoder: {}", js_err(&e))))?;
        call(&encoder, "configure", &[&config]).map_err(|e| ExportError::Encode(format!("WebCodecs VideoEncoder.configure: {}", js_err(&e))))?;
        let id = NEXT_ID.with(|n| {
            let id = n.get();
            n.set(id.wrapping_add(1).max(1));
            id
        });
        ENCODERS.with(|m| m.borrow_mut().insert(id, Js { encoder, out, description, error, flushed, _closures: (on_output, on_error) }));
        filmcraft_export::note_hw_encode_session();
        STATS.with(|st| st.borrow_mut().sessions += 1);
        Ok(Self {
            id,
            family,
            w,
            h,
            timescale,
            frame_duration,
            frame_us: 1e6 / fps,
            keyint,
            quantizer,
            submitted: 0,
            delivered: 0,
            last_timestamp: None,
            flush_started: false,
            y: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
            i420: Vec::new(),
        })
    }

    fn with_js<R>(&self, f: impl FnOnce(&Js) -> R) -> filmcraft_export::Result<R> {
        ENCODERS
            .with(|m| m.borrow().get(&self.id).map(f))
            .ok_or_else(|| ExportError::Encode("internal: the WebCodecs encoder is gone (used from another thread?)".into()))
    }

    fn check_error(&self) -> filmcraft_export::Result<()> {
        match self.with_js(|js| js.error.borrow().clone())? {
            Some(e) => {
                Err(ExportError::Encode(format!("the browser's hardware encoder failed ({e}); export with the software encoder instead (encoder: software)")))
            }
            None => Ok(()),
        }
    }

    fn drain(&mut self) -> filmcraft_export::Result<Vec<EncodedPacket>> {
        self.check_error()?;
        let chunks: Vec<Chunk> = self.with_js(|js| js.out.borrow_mut().drain(..).collect())?;
        let mut packets = Vec::with_capacity(chunks.len());
        for c in chunks {
            // no frame reordering is configured (and none has been seen): decode order = output
            // order, so pts = dts and the composition offset is 0
            if self.last_timestamp.is_some_and(|t| c.timestamp_us <= t) {
                return Err(ExportError::Encode("the browser's encoder reordered frames, which this exporter does not support; export with the software encoder instead (encoder: software)".into()));
            }
            self.last_timestamp = Some(c.timestamp_us);
            self.delivered = self.delivered.saturating_add(1);
            packets.push(EncodedPacket { data: c.data, key: c.key, duration: self.frame_duration, composition_offset: 0 });
        }
        Ok(packets)
    }
}

impl Drop for WebCodecsEncoder {
    fn drop(&mut self) {
        if let Some(js) = ENCODERS.with(|m| m.borrow_mut().remove(&self.id))
            && get(&js.encoder, "state").as_string().as_deref() != Some("closed")
        {
            let _ = call(&js.encoder, "close", &[]);
        }
    }
}

impl VideoEncoder for WebCodecsEncoder {
    fn sample_entry(&self) -> SampleEntry {
        let (w, h) = (self.w.min(u32::from(u16::MAX)) as u16, self.h.min(u32::from(u16::MAX)) as u16);
        let desc = self.with_js(|js| js.description.borrow().clone()).ok().flatten().unwrap_or_default();
        match self.family {
            // `ready` holds the muxer back until the record arrived, so these parse
            Family::Avc => SampleEntry::avc(AvcConfig::parse(&desc).unwrap_or_else(|_| AvcConfig::new(Vec::new(), Vec::new(), 4)), w, h),
            Family::Hevc => SampleEntry::hevc(HevcConfig::parse(&desc).unwrap_or_default(), w, h),
        }
    }

    fn timescale(&self) -> u32 {
        self.timescale
    }

    fn is_hardware(&self) -> bool {
        true
    }

    fn encode(&mut self, f: &EncoderFrame) -> filmcraft_export::Result<Vec<EncodedPacket>> {
        self.check_error()?;
        if f.hdr.is_some() {
            return Err(ExportError::Unsupported("the browser's hardware encoder does not take HDR pictures here".into()));
        }
        if (f.width, f.height) != (self.w, self.h) {
            return Err(ExportError::Encode(format!("picture is {}x{}, the encoder was created for {}x{}", f.width, f.height, self.w, self.h)));
        }
        let (w, h) = (f.width as usize, f.height as usize);
        if f.rgba.len() < w.saturating_mul(h).saturating_mul(4) {
            return Err(ExportError::Encode("the RGBA picture is smaller than its size".into()));
        }
        filmcraft_export::timed(filmcraft_export::Stage::Convert, || filmcraft_export::rgba_to_yuv420_8(f.rgba, w, h, &mut self.y, &mut self.u, &mut self.v));
        // one I420 buffer (Y, U, V planes back to back), copied into a fresh JS buffer: in the
        // threaded build wasm memory is shared, and a copy also lets the frame outlive this call
        self.i420.clear();
        self.i420.extend_from_slice(&self.y);
        self.i420.extend_from_slice(&self.u);
        self.i420.extend_from_slice(&self.v);
        let buf = js_sys::Uint8Array::from(self.i420.as_slice());
        let timestamp = (f.index as f64 * self.frame_us).round();
        let init = obj(&[
            ("format", "I420".into()),
            ("codedWidth", self.w.into()),
            ("codedHeight", self.h.into()),
            ("timestamp", timestamp.into()),
            ("duration", self.frame_us.round().into()),
            ("colorSpace", obj(&[("primaries", "bt709".into()), ("transfer", "bt709".into()), ("matrix", "bt709".into()), ("fullRange", false.into())])),
        ]);
        let vf_ctor = get(&js_sys::global(), "VideoFrame")
            .dyn_into::<js_sys::Function>()
            .map_err(|_| ExportError::Unsupported("this browser has no WebCodecs VideoFrame".into()))?;
        let frame =
            js_sys::Reflect::construct(&vf_ctor, &js_sys::Array::of2(&buf, &init)).map_err(|e| ExportError::Encode(format!("VideoFrame: {}", js_err(&e))))?;
        let key = f.index.is_multiple_of(self.keyint.max(1));
        let mut opts = vec![("keyFrame", JsValue::from(key))];
        if let Some(q) = self.quantizer {
            let k = if self.family == Family::Hevc { "hevc" } else { "avc" };
            opts.push((k, obj(&[("quantizer", q.into())])));
        }
        let opts = obj(&opts);
        let r = self.with_js(|js| call(&js.encoder, "encode", &[&frame, &opts]))?;
        // the encoder keeps its own reference: release ours right away (no leaked VideoFrames)
        let _ = call(&frame, "close", &[]);
        r.map_err(|e| {
            ExportError::Encode(format!(
                "the browser's hardware encoder rejected a picture ({}); export with the software encoder instead (encoder: software)",
                js_err(&e)
            ))
        })?;
        self.submitted = self.submitted.saturating_add(1);
        filmcraft_export::note_hw_encode_frame();
        let queue = self.with_js(|js| get(&js.encoder, "encodeQueueSize").as_f64().unwrap_or(0.0) as u32)?;
        STATS.with(|st| {
            let mut st = st.borrow_mut();
            st.frames += 1;
            st.queue_max = st.queue_max.max(queue);
        });
        self.drain()
    }

    fn ready(&mut self) -> filmcraft_export::Result<bool> {
        self.check_error()?;
        let (queue, have_config) =
            self.with_js(|js| (get(&js.encoder, "encodeQueueSize").as_f64().unwrap_or(0.0) as u32, js.description.borrow().is_some()))?;
        // the muxer is created after the first group of pictures and needs the codec record
        let config_wait = self.submitted > 0 && !have_config;
        let in_flight = self.submitted.saturating_sub(self.delivered).saturating_sub(self.with_js(|js| js.out.borrow().len() as u64)?);
        Ok(queue <= MAX_QUEUE && !config_wait && in_flight < MAX_IN_FLIGHT)
    }

    fn poll(&mut self) -> filmcraft_export::Result<Vec<EncodedPacket>> {
        self.drain()
    }

    fn flush(&mut self) -> filmcraft_export::Result<Vec<EncodedPacket>> {
        if !self.flush_started {
            self.flush_started = true;
            let (p, flushed, error) = self.with_js(|js| (call(&js.encoder, "flush", &[]), js.flushed.clone(), js.error.clone()))?;
            let p: js_sys::Promise = p.and_then(|p| p.dyn_into()).map_err(|e| ExportError::Encode(format!("VideoEncoder.flush: {}", js_err(&e))))?;
            wasm_bindgen_futures::spawn_local(async move {
                match JsFuture::from(p).await {
                    Ok(_) => flushed.set(true),
                    Err(e) => {
                        *error.borrow_mut() = Some(js_err(&e));
                        flushed.set(true);
                    }
                }
                crate::repaint();
            });
        }
        self.drain()
    }

    fn pending(&self) -> bool {
        self.with_js(|js| !js.flushed.get() || !js.out.borrow().is_empty()).unwrap_or(false)
    }
}

/// `filmcraft.info().export` encoder counters.
pub fn stats() -> serde_json::Value {
    STATS.with(|st| {
        let st = st.borrow();
        json!({
            "webcodecsEncoder": {
                "sessions": st.sessions, "frames": st.frames, "encodeQueueMax": st.queue_max,
                "errors": st.errors, "declined": st.declined, "lastError": st.last_error,
            },
            "backend": st.backend,
            "encodeQueueMax": st.queue_max,
            "encoderErrors": st.errors,
            "openEncoders": ENCODERS.with(|m| m.borrow().len()),
            "support": SUPPORT.with(|s| s.borrow().iter().map(|((f, big), ok)| (format!("{f:?}{}", if *big { "-4k" } else { "-1080p" }), json!(ok))).collect::<serde_json::Map<_, _>>()),
        })
    })
}
