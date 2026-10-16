//! Asynchronous encoders (the web's WebCodecs) through the stepped [`Exporter`]: output that
//! arrives on later steps, an encoder that is not ready, a flush that completes later, errors
//! reported meanwhile, and `require_hardware`.

use std::sync::atomic::AtomicU32;

use super::*;
use crate::tests::project;

fn tmp(name: &str) -> String {
    let d = std::env::temp_dir().join(format!("fc-export-async-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name).to_string_lossy().to_string()
}

/// How the fake asynchronous encoder misbehaves.
static FAIL_AFTER: AtomicU32 = AtomicU32::new(u32::MAX);

/// Wraps the built-in MJPEG encoder: packets are held back and released over later polls, the
/// encoder is "busy" every third time it is asked, the flush completes three polls later.
struct LaggingEncoder {
    inner: Box<dyn VideoEncoder>,
    held: std::collections::VecDeque<EncodedPacket>,
    asked: u32,
    flushing: Option<u32>,
    encoded: u32,
}

impl LaggingEncoder {
    fn release(&mut self, n: usize) -> Vec<EncodedPacket> {
        let n = n.min(self.held.len());
        self.held.drain(..n).collect()
    }
}

impl VideoEncoder for LaggingEncoder {
    fn sample_entry(&self) -> SampleEntry {
        self.inner.sample_entry()
    }
    fn timescale(&self) -> u32 {
        self.inner.timescale()
    }
    fn is_hardware(&self) -> bool {
        true
    }
    fn encode(&mut self, frame: &EncoderFrame) -> Result<Vec<EncodedPacket>> {
        let p = self.inner.encode(frame)?;
        self.held.extend(p);
        self.encoded += 1;
        // nothing comes back with the picture that produced it
        Ok(self.release(self.held.len().saturating_sub(2)))
    }
    fn ready(&mut self) -> Result<bool> {
        if self.encoded >= FAIL_AFTER.load(Ordering::SeqCst) {
            return Err(ExportError::Encode("the browser's hardware encoder failed (test)".into()));
        }
        self.asked += 1;
        Ok(!self.asked.is_multiple_of(3))
    }
    fn poll(&mut self) -> Result<Vec<EncodedPacket>> {
        if let Some(n) = self.flushing.as_mut() {
            *n = n.saturating_sub(1);
        }
        Ok(self.release(1))
    }
    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        let tail = self.inner.flush()?;
        self.held.extend(tail);
        self.flushing = Some(3);
        Ok(Vec::new())
    }
    fn pending(&self) -> bool {
        self.flushing != Some(0) || !self.held.is_empty()
    }
}

/// Takes MJPEG exports to `…lagging…` paths only, so other tests in this process are unaffected.
fn lagging_factory(format: Format, w: u32, h: u32, rate: FrameRate, s: &ExportSettings) -> Option<Result<Box<dyn VideoEncoder>>> {
    if format != Format::Mjpeg || !s.path.contains("lagging") {
        return None;
    }
    let inner = mjpeg_factory(format, w, h, rate, s)?.ok()?;
    Some(Ok(Box::new(LaggingEncoder { inner, held: Default::default(), asked: 0, flushing: None, encoded: 0 })))
}

fn run_stepped(s: &ExportSettings) -> Result<(Vec<u8>, u32)> {
    let (p, seq, m) = project();
    let prog = Progress::default();
    let mut ex = Exporter::new(p, seq, s, &prog)?;
    ex.set_batch(1);
    let mut pendings = 0;
    for _ in 0..10_000 {
        match ex.step(&m, &prog)? {
            Step::Done(_) => return Ok((std::fs::read(&s.path).unwrap(), pendings)),
            Step::Pending => pendings += 1,
            Step::Progress => {}
        }
    }
    panic!("the export did not finish");
}

#[test]
fn an_asynchronous_encoder_gives_the_same_file() {
    register_encoder(lagging_factory);
    let sync = ExportSettings { format: Format::Mjpeg, path: tmp("sync.mov"), ..Default::default() };
    let (reference, _) = run_stepped(&sync).unwrap();
    let lagging = ExportSettings { format: Format::Mjpeg, path: tmp("lagging.mov"), ..Default::default() };
    let (file, pendings) = run_stepped(&lagging).unwrap();
    assert!(pendings > 3, "the exporter never waited for the encoder ({pendings})");
    // packets that arrive late land in a later interleave group, so the chunk layout differs; every
    // track's samples (bytes, timing, sync flags) must be the same
    assert_eq!(samples(&file), samples(&reference), "packets delivered late changed the media");
}

/// Per track: (payload, duration, pts - dts, sync) of every sample.
fn samples(file: &[u8]) -> Vec<Vec<(Vec<u8>, u32, i64, bool)>> {
    let mp4 = filmcraft_isobmff::open(file.to_vec()).unwrap();
    mp4.tracks
        .iter()
        .map(|t| {
            t.samples.iter().map(|s| (file[s.offset as usize..s.offset as usize + s.size as usize].to_vec(), s.duration, s.pts - s.dts, s.is_sync)).collect()
        })
        .collect()
}

#[test]
fn an_encoder_error_reported_later_stops_the_export() {
    register_encoder(lagging_factory);
    FAIL_AFTER.store(5, Ordering::SeqCst);
    let s = ExportSettings { format: Format::Mjpeg, path: tmp("lagging-fail.mov"), ..Default::default() };
    let r = run_stepped(&s);
    FAIL_AFTER.store(u32::MAX, Ordering::SeqCst);
    match r {
        Err(ExportError::Encode(m)) => assert!(m.contains("hardware encoder failed"), "{m}"),
        other => panic!("expected the encoder's error, got {:?}", other.map(|(_, p)| p)),
    }
}

#[test]
fn requiring_hardware_without_a_hardware_encoder_is_an_error() {
    // no hardware encoder is registered in this crate's tests: the built-in H.264 encoder would
    // be used, which `require_hardware` refuses
    let (p, seq, _) = project();
    let s = ExportSettings {
        format: Format::H264,
        path: tmp("require.mp4"),
        hardware_encoding: HardwareEncoding::Auto,
        require_hardware: true,
        ..Default::default()
    };
    match Exporter::new(p, seq, &s, &Progress::default()) {
        Err(ExportError::Unsupported(m)) => assert!(m.contains("encoder: software"), "{m}"),
        Err(e) => panic!("unexpected error {e}"),
        Ok(_) => panic!("a software encoder was accepted although hardware was required"),
    }
}

#[test]
fn a_blocking_export_explains_an_asynchronous_encoder() {
    register_encoder(lagging_factory);
    let (p, seq, m) = project();
    let s = ExportSettings { format: Format::Mjpeg, path: tmp("lagging-blocking.mov"), ..Default::default() };
    // the lagging encoder is busy on its third call: the blocking export cannot wait for it
    match export(&p, seq, &s, &m, &Progress::default()) {
        Err(ExportError::Io(msg)) => assert!(msg.contains("asynchronously"), "{msg}"),
        Err(e) => panic!("unexpected error {e}"),
        Ok(_) => {} // finished before the encoder was ever busy: also fine
    }
}
