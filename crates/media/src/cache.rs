//! A byte-budgeted LRU cache of decoded/rendered frames, shared by monitors, thumbnails and export.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use filmcraft_frame::VideoFrame;

struct Entry {
    frame: Arc<VideoFrame>,
    bytes: usize,
    stamp: u64,
}

struct Inner<K> {
    map: HashMap<K, Entry>,
    bytes: usize,
    clock: u64,
    hits: u64,
    misses: u64,
}

pub struct FrameCache<K: Eq + Hash + Clone = (u64, i64, u16)> {
    inner: Mutex<Inner<K>>,
    budget: usize,
}

impl<K: Eq + Hash + Clone> FrameCache<K> {
    pub fn new(budget_bytes: usize) -> Self {
        Self { inner: Mutex::new(Inner { map: HashMap::new(), bytes: 0, clock: 0, hits: 0, misses: 0 }), budget: budget_bytes }
    }

    pub fn get(&self, k: &K) -> Option<Arc<VideoFrame>> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.clock += 1;
        let c = g.clock;
        match g.map.get_mut(k) {
            Some(e) => {
                e.stamp = c;
                let f = e.frame.clone();
                g.hits += 1;
                Some(f)
            }
            None => {
                g.misses += 1;
                None
            }
        }
    }

    pub fn insert(&self, k: K, frame: Arc<VideoFrame>) {
        let bytes = frame.byte_size();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.clock += 1;
        let stamp = g.clock;
        if let Some(old) = g.map.insert(k, Entry { frame, bytes, stamp }) {
            g.bytes -= old.bytes;
        }
        g.bytes += bytes;
        if g.bytes > self.budget {
            // Evict oldest entries until under 90% of budget.
            let mut v: Vec<(u64, K, usize)> = g.map.iter().map(|(k, e)| (e.stamp, k.clone(), e.bytes)).collect();
            v.sort_unstable_by_key(|x| x.0);
            let target = self.budget * 9 / 10;
            for (_, k, b) in v {
                if g.bytes <= target {
                    break;
                }
                g.map.remove(&k);
                g.bytes -= b;
            }
        }
    }

    pub fn get_or_insert_with<E>(&self, k: K, f: impl FnOnce() -> Result<Arc<VideoFrame>, E>) -> Result<Arc<VideoFrame>, E> {
        if let Some(v) = self.get(&k) {
            return Ok(v);
        }
        let v = f()?;
        self.insert(k, v.clone());
        Ok(v)
    }

    pub fn clear(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.map.clear();
        g.bytes = 0;
    }

    pub fn retain(&self, mut keep: impl FnMut(&K) -> bool) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut freed = 0;
        g.map.retain(|k, e| {
            let k2 = keep(k);
            if !k2 {
                freed += e.bytes;
            }
            k2
        });
        g.bytes -= freed;
    }

    /// (entries, bytes, hits, misses)
    pub fn stats(&self) -> (usize, usize, u64, u64) {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        (g.map.len(), g.bytes, g.hits, g.misses)
    }
}

/// Trim a per-source cache of decoded frames (keyed by presentation time) to `keep`, dropping
/// the frames farthest from `want` (the latest request) first, but never a frame `pinned` says was
/// asked for recently: two requesters far apart on one source (an export and the scrubbed monitor)
/// would otherwise evict each other's frame between its decode and its next ask, and re-decode
/// it from the key frame forever. Pinned frames go too (farthest first) only beyond `keep` +
/// `max_pinned`, so memory stays bounded whatever asks.
pub fn evict_far<T>(frames: &mut std::collections::BTreeMap<i64, T>, keep: usize, want: i64, max_pinned: usize, pinned: impl Fn(i64) -> bool) {
    let far = |k: &i64| k.abs_diff(want);
    while frames.len() > keep {
        let free = frames.keys().copied().filter(|k| !pinned(*k)).max_by_key(far);
        let victim = match free {
            Some(k) => k,
            None if frames.len() > keep.saturating_add(max_pinned) => match frames.keys().copied().max_by_key(far) {
                Some(k) => k,
                None => break,
            },
            None => break,
        };
        frames.remove(&victim);
    }
}

/// The cached frames to pin for [`evict_far`]: each recently asked frame and the `run_ahead`
/// frames cached after it. A decoder runs ahead of its request and a one-pass reader (an export)
/// asks for those next; pinning only what was asked let another requester on the same source
/// (the scrubbed monitor) evict them, and the export then restarted the decoder at the key frame
/// for nearly every frame.
pub fn run_ahead_pins<T>(
    frames: &std::collections::BTreeMap<i64, T>,
    asked: impl IntoIterator<Item = i64>,
    run_ahead: usize,
) -> std::collections::HashSet<i64> {
    let mut pins = std::collections::HashSet::new();
    for a in asked {
        pins.extend(frames.range(a..).take(run_ahead.saturating_add(1)).map(|(k, _)| *k));
    }
    pins
}

#[cfg(test)]
mod tests {
    #[test]
    fn run_ahead_of_an_asked_frame_survives_another_requester() {
        use std::collections::BTreeMap;
        // the export asked for 10 (the decoder output up to 20); the monitor now wants 100
        let mut f: BTreeMap<i64, ()> = (10..=20).chain(95..=105).map(|k| (k, ())).collect();
        let pins = super::run_ahead_pins(&f, [10, 100], 8);
        assert!((10..=18).all(|k| pins.contains(&k)) && !pins.contains(&19));
        assert!((100..=105).all(|k| pins.contains(&k)) && !pins.contains(&95));
        super::evict_far(&mut f, 16, 100, 32, |k| pins.contains(&k));
        assert_eq!(f.len(), 16);
        assert!((10..=18).all(|k| f.contains_key(&k)), "the export's next frames stayed: {:?}", f.keys().collect::<Vec<_>>());
        // nothing asked: nothing pinned; hostile run-ahead does not overflow
        assert!(super::run_ahead_pins(&f, [], 8).is_empty());
        assert_eq!(super::run_ahead_pins(&f, [i64::MAX], usize::MAX).len(), 0);
    }

    #[test]
    fn evict_far_keeps_recently_asked_frames() {
        use std::collections::BTreeMap;
        // the export waits for 116; the monitor now scrubs at 30 and its decode filled the cache
        let mut f: BTreeMap<i64, ()> = (0..40).chain([116]).map(|k| (k, ())).collect();
        super::evict_far(&mut f, 16, 30, 32, |k| k == 116);
        assert_eq!(f.len(), 16);
        assert!(f.contains_key(&116), "the export's frame survived");
        assert!(f.contains_key(&30));
        // without pins: the farthest go first, as before
        let mut f: BTreeMap<i64, ()> = (0..40).chain([116]).map(|k| (k, ())).collect();
        super::evict_far(&mut f, 16, 30, 32, |_| false);
        assert!(!f.contains_key(&116));
        assert_eq!(f.keys().copied().collect::<Vec<_>>(), (22..38).collect::<Vec<_>>());
        // pins beyond the cap go too, farthest first
        let mut f: BTreeMap<i64, ()> = (0..100).map(|k| (k, ())).collect();
        super::evict_far(&mut f, 4, 0, 6, |_| true);
        assert_eq!(f.len(), 10);
        assert!(f.contains_key(&0) && !f.contains_key(&99));
        // hostile keys: no overflow
        let mut f: BTreeMap<i64, ()> = [i64::MIN, i64::MAX, 0].into_iter().map(|k| (k, ())).collect();
        super::evict_far(&mut f, 1, i64::MIN, 0, |_| false);
        assert_eq!(f.keys().copied().collect::<Vec<_>>(), vec![i64::MIN]);
    }

    use super::*;

    #[test]
    fn evicts_lru() {
        let c: FrameCache<u32> = FrameCache::new(4 * 4 * 3);
        for i in 0..3 {
            c.insert(i, Arc::new(VideoFrame::rgba8(2, 2, vec![0; 16])));
        }
        assert!(c.get(&0).is_some()); // touch 0
        c.insert(3, Arc::new(VideoFrame::rgba8(2, 2, vec![0; 16])));
        assert!(c.get(&0).is_some());
        assert!(c.get(&1).is_none());
        assert!(c.stats().1 <= 48);
    }
}
