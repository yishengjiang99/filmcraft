//! Streamed output ([`StreamTarget`], the web's OPFS / Blob sinks): the file is the same as a
//! buffered export, and nothing is held back beyond one chunk.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::*;
use crate::tests::project;

fn tmp(name: &str) -> String {
    let d = std::env::temp_dir().join(format!("fc-export-stream-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name).to_string_lossy().to_string()
}

/// Streamed files by path, and the largest write seen.
fn store() -> &'static Mutex<(HashMap<String, Vec<u8>>, usize)> {
    static S: OnceLock<Mutex<(HashMap<String, Vec<u8>>, usize)>> = OnceLock::new();
    S.get_or_init(|| Mutex::new((HashMap::new(), 0)))
}

struct MemTarget {
    path: String,
    data: Vec<u8>,
}

impl StreamTarget for MemTarget {
    fn write_at(&mut self, position: u64, data: &[u8]) -> std::io::Result<()> {
        let p = position as usize;
        if self.data.len() < p + data.len() {
            self.data.resize(p + data.len(), 0);
        }
        self.data[p..p + data.len()].copy_from_slice(data);
        let mut s = store().lock().unwrap();
        s.1 = s.1.max(data.len());
        Ok(())
    }
    fn finish(self: Box<Self>, length: u64) -> std::io::Result<()> {
        let mut d = self.data;
        d.truncate(length as usize);
        store().lock().unwrap().0.insert(self.path, d);
        Ok(())
    }
}

/// Streams only `…streamed…` paths, so other tests in this process are unaffected.
fn mem_factory(path: &str, _kind: &str) -> Option<std::io::Result<Box<dyn StreamTarget>>> {
    path.contains("streamed").then(|| Ok(Box::new(MemTarget { path: path.to_string(), data: Vec::new() }) as Box<dyn StreamTarget>))
}

fn stepped(s: &ExportSettings) {
    let (p, seq, m) = project();
    let prog = Progress::default();
    let mut ex = Exporter::new(p, seq, s, &prog).unwrap();
    ex.set_batch(1);
    while !matches!(ex.step(&m, &prog).unwrap(), Step::Done(_)) {}
}

#[test]
fn a_streamed_export_is_the_same_file() {
    register_stream_target(mem_factory);
    // buffered: the sink receives the whole file at the end
    let buffered: Arc<Mutex<Option<Vec<u8>>>> = Arc::default();
    let b = buffered.clone();
    let sink = OutputSink(Arc::new(move |_: &str, d: Vec<u8>| {
        *b.lock().unwrap() = Some(d);
        Ok(())
    }));
    for format in [Format::H264, Format::Mjpeg] {
        let ext = if format == Format::H264 { "mp4" } else { "mov" };
        let mem = ExportSettings {
            format,
            path: tmp(&format!("buffered.{ext}")),
            sink: Some(sink.clone()),
            stream_sink: Some("memory".into()),
            ..Default::default()
        };
        stepped(&mem);
        let reference = buffered.lock().unwrap().take().expect("the buffered export reached the sink");
        let streamed_path = tmp(&format!("streamed.{ext}"));
        let st = ExportSettings { format, path: streamed_path.clone(), sink: Some(sink.clone()), ..Default::default() };
        stepped(&st);
        assert!(buffered.lock().unwrap().is_none(), "a streamed export does not also buffer the file");
        let streamed = store().lock().unwrap().0.remove(&streamed_path).expect("the stream target was finished");
        assert!(streamed == reference, "{format:?}: the streamed file differs from the buffered one ({} vs {} bytes)", streamed.len(), reference.len());
    }
    assert!(store().lock().unwrap().1 <= STREAM_CHUNK + (1 << 20), "chunks stay near STREAM_CHUNK");
}

#[test]
fn stream_out_seeks_and_patches() {
    struct Rec(Arc<Mutex<Vec<(u64, Vec<u8>)>>>, Arc<Mutex<Option<u64>>>);
    impl StreamTarget for Rec {
        fn write_at(&mut self, position: u64, data: &[u8]) -> std::io::Result<()> {
            self.0.lock().unwrap().push((position, data.to_vec()));
            Ok(())
        }
        fn finish(self: Box<Self>, length: u64) -> std::io::Result<()> {
            *self.1.lock().unwrap() = Some(length);
            Ok(())
        }
    }
    use std::io::{Seek, SeekFrom};
    let writes: Arc<Mutex<Vec<(u64, Vec<u8>)>>> = Arc::default();
    let len: Arc<Mutex<Option<u64>>> = Arc::default();
    let mut o = StreamOut::new(Box::new(Rec(writes.clone(), len.clone())));
    o.write_all(b"headerXXXXbody").unwrap();
    assert_eq!(o.seek(SeekFrom::Start(6)).unwrap(), 6);
    o.write_all(b"1234").unwrap();
    assert_eq!(o.seek(SeekFrom::End(0)).unwrap(), 14);
    o.write_all(b"tail").unwrap();
    // hostile seeks are errors, not panics
    assert!(o.seek(SeekFrom::Current(-1000)).is_err());
    assert!(o.seek(SeekFrom::End(i64::MIN)).is_err());
    assert_eq!(o.finish().unwrap(), 18);
    assert_eq!(*len.lock().unwrap(), Some(18));
    let mut file = vec![0u8; 18];
    for (p, d) in writes.lock().unwrap().iter() {
        file[*p as usize..*p as usize + d.len()].copy_from_slice(d);
    }
    assert_eq!(&file, b"header1234bodytail");
}
