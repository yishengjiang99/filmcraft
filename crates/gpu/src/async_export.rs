//! Asynchronous export rendering: the same composite as [`crate::ExportRenderer`], read back
//! without blocking. A browser's main thread may not wait for the GPU (WebGPU's `mapAsync` only
//! completes once the page's event loop runs), so a frame is **submitted** (plan composited
//! into the linear float accumulator, copied into a staging buffer of a small ring, `map_async`
//! requested) and **collected** on a later call once its buffer is mapped (half floats → f32,
//! exactly as the synchronous read-back converts them). Several frames can be in flight, one per
//! staging buffer.
//!
//! The renderer owns no device: it borrows one (on the web, eframe's WebGPU device), so creating
//! it never blocks. Every submission runs inside validation and out-of-memory error scopes; an
//! error, a failed mapping (lost device) or a read-back that does not finish within
//! [`MAP_TIMEOUT`] (and [`MAP_TIMEOUT_POLLS`] looks: a busy main thread is not a slow GPU) retires the renderer ([`AsyncExportRenderer::failed`]), and the caller renders
//! the rest of the export on the CPU instead of hanging.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use filmcraft_project::{ItemId, Project};
use filmcraft_render::plan::{FramePlan, plan_frame};
use filmcraft_render::{Image, RenderOptions, SourceProvider};
use filmcraft_time::Tick;

use crate::{GpuCompositor, prepare};

/// A read-back that takes longer than this is treated as a lost device.
pub const MAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// ... and only once it was looked at this often: a browser runs the read-back's completion only
/// between the exporter's steps, and one step can take seconds (a 4K frame on the CPU), so wall
/// time alone would retire a healthy GPU.
pub const MAP_TIMEOUT_POLLS: u32 = 30;

/// Rows converted per mapped range: on WebGPU every mapped range is copied into wasm memory first,
/// and a whole 4K frame would be a 66 MB copy (on a 2 GiB heap).
const BAND_ROWS: u32 = 64;

/// Staging buffers kept at most, whatever the memory budget allows.
const MAX_SLOTS: usize = 16;

const IDLE: u8 = 0;
const MAPPING: u8 = 1;
const MAPPED: u8 = 2;
const MAP_FAILED: u8 = 3;

#[cfg(target_arch = "wasm32")]
type ScopeAnswer = Pin<Box<dyn Future<Output = Option<wgpu::Error>>>>;
#[cfg(not(target_arch = "wasm32"))]
type ScopeAnswer = Pin<Box<dyn Future<Output = Option<wgpu::Error>> + Send>>;

/// One staging buffer and the frame it holds.
struct Slot {
    buf: wgpu::Buffer,
    /// Frame key (the frame's tick) while in use.
    key: Option<i64>,
    /// (width, height, row pitch) of the copy.
    layout: (u32, u32, u32),
    state: Arc<AtomicU8>,
    since: web_time::Instant,
    /// Times it was looked at while still mapping.
    polls: u32,
    /// The submission's error scopes, until answered.
    scopes: Vec<ScopeAnswer>,
}

/// What [`AsyncExportRenderer::render`] came to for one frame.
#[derive(Debug)]
pub enum AsyncFrame {
    /// The frame as `render_sequence` would return it (premultiplied linear-light RGBA).
    Ready(Image),
    /// Submitted, not back yet: ask again later.
    Pending,
    /// Not for the GPU (transitions, a failed renderer): render it on the CPU.
    Declined,
}

/// Export frames composited on a borrowed device and read back asynchronously.
pub struct AsyncExportRenderer {
    device: wgpu::Device,
    compositor: GpuCompositor,
    slots: Vec<Slot>,
    /// Bytes the staging buffers may take together.
    budget: u64,
    failed: Option<String>,
    /// Frames collected (stats).
    pub collected: u64,
    /// Sum and maximum of the time from submission to collection (stats).
    pub latency_total: std::time::Duration,
    pub latency_max: std::time::Duration,
}

impl AsyncExportRenderer {
    /// A renderer on `device` / `queue` whose staging buffers take at most `budget` bytes (at
    /// least two buffers are allowed, so one frame can be read while the next renders).
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, budget: u64) -> Self {
        let mut compositor = GpuCompositor::new(device, queue);
        compositor.set_upload_cache(8);
        Self {
            device: device.clone(),
            compositor,
            slots: Vec::new(),
            budget,
            failed: None,
            collected: 0,
            latency_total: std::time::Duration::ZERO,
            latency_max: std::time::Duration::ZERO,
        }
    }

    /// Why the renderer retired (a GPU error, a lost device, a read-back timeout), if it did.
    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// Whether layer effects run on the GPU on this device.
    pub fn has_effect_stage(&self) -> bool {
        self.compositor.has_effect_stage()
    }

    /// Frames that can be in flight at once for a `w`×`h` render (staging buffers the budget allows).
    pub fn frames_in_flight(&self, w: u32, h: u32) -> usize {
        let row = u64::from(w).saturating_mul(8).div_ceil(256).saturating_mul(256);
        let bytes = row.saturating_mul(u64::from(h)).max(1);
        usize::try_from(self.budget / bytes).unwrap_or(MAX_SLOTS).clamp(2, MAX_SLOTS)
    }

    fn fail(&mut self, why: String) {
        if self.failed.is_none() {
            log::error!("GPU export rendering stopped ({why}); the rest of the export renders on the CPU");
            self.failed = Some(why);
        }
        // in-flight frames are dropped: nothing will collect them
        for s in &mut self.slots {
            s.key = None;
            s.scopes.clear();
        }
    }

    /// The frame at `t`: collected when its read-back finished, `Pending` while in flight
    /// (submitted now when it was not), `Declined` for the CPU. `incomplete` tells whether the
    /// frame's media was still loading when it was planned (the web's pending flag): such a plan
    /// is not submitted (it would draw missing layers) and the answer is `Pending`.
    pub fn render(
        &mut self,
        project: &Project,
        seq: ItemId,
        t: Tick,
        opts: RenderOptions,
        sources: &dyn SourceProvider,
        incomplete: &dyn Fn() -> bool,
    ) -> AsyncFrame {
        if self.failed.is_some() {
            return AsyncFrame::Declined;
        }
        if self.slot_of(t.0).is_some() {
            return self.collect(t.0);
        }
        if self.free_slot().is_none() && !self.reclaim(t.0) {
            // every buffer is still being read back: they finish (or time out) on their own
            self.poll_device();
            self.check_timeouts();
            return if self.failed.is_some() { AsyncFrame::Declined } else { AsyncFrame::Pending };
        }
        let plan = plan_frame(project, seq, t, opts, sources);
        if incomplete() {
            return AsyncFrame::Pending;
        }
        match plan {
            FramePlan::Image(img) => AsyncFrame::Ready(img),
            FramePlan::Composite { .. } => AsyncFrame::Declined,
            FramePlan::Layers { .. } => {
                if !self.submit(t.0, &plan) {
                    return if self.failed.is_some() { AsyncFrame::Declined } else { AsyncFrame::Pending };
                }
                // a native device may already be done; the web's is not before the event loop runs
                self.collect(t.0)
            }
        }
    }

    /// Submit the frame at `t` ahead of [`render`](Self::render) asking for it, when a staging
    /// buffer is free (a prefetch never evicts a finished frame). A plan whose media is still
    /// loading (`incomplete`) or that is not for the GPU is skipped.
    pub fn prefetch(&mut self, project: &Project, seq: ItemId, t: Tick, opts: RenderOptions, sources: &dyn SourceProvider, incomplete: &dyn Fn() -> bool) {
        if self.failed.is_some() || self.slot_of(t.0).is_some() || self.free_slot().is_none() {
            return;
        }
        let plan = plan_frame(project, seq, t, opts, sources);
        if incomplete() || !matches!(plan, FramePlan::Layers { .. }) {
            return;
        }
        self.submit(t.0, &plan);
    }

    fn max_slots(&self) -> usize {
        let (w, h) = self.slots.first().map_or((1920, 1080), |s| (s.layout.0, s.layout.1));
        self.frames_in_flight(w, h)
    }

    fn slot_of(&self, key: i64) -> Option<usize> {
        self.slots.iter().position(|s| s.key == Some(key))
    }

    /// A slot with no frame (an existing one, or room to make one).
    fn free_slot(&self) -> Option<Option<usize>> {
        match self.slots.iter().position(|s| s.key.is_none() && s.state.load(Ordering::Acquire) == IDLE) {
            Some(i) => Some(Some(i)),
            None if self.slots.len() < self.max_slots() => Some(None),
            None => None,
        }
    }

    /// Free a finished frame nobody is asking for now, to make room for `key`: one whose frame
    /// is behind `key` first (left over), else the one furthest ahead. Whether one was freed.
    fn reclaim(&mut self, key: i64) -> bool {
        self.poll_device();
        let done: Vec<(usize, i64)> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.key.filter(|&k| k != key && s.state.load(Ordering::Acquire) == MAPPED).map(|k| (i, k)))
            .collect();
        let pick = done.iter().filter(|(_, k)| *k < key).min_by_key(|(_, k)| *k).or_else(|| done.iter().max_by_key(|(_, k)| *k));
        let Some(&(i, _)) = pick else { return false };
        if let Some(s) = self.slots.get_mut(i) {
            s.buf.unmap();
            s.state.store(IDLE, Ordering::Release);
            s.key = None;
            s.scopes.clear();
        }
        true
    }

    /// Composite `plan` and start reading it back into a free slot. False when no slot is free
    /// or the submission failed (then [`failed`](Self::failed) says why).
    fn submit(&mut self, key: i64, plan: &FramePlan) -> bool {
        let Some(free) = self.free_slot() else { return false };
        let t = web_time::Instant::now();
        let validation = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let memory = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let prep = prepare(plan);
        self.compositor.render_plan(plan, Some(&prep), true, false);
        let layout = self.compositor.accumulator_layout();
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("filmcraft-composite-export-async") });
        let slot = match (layout, free) {
            (Some(_), Some(i)) if self.slots.get(i).is_some_and(|s| s.buf.size() >= Self::bytes(layout)) => Some(i),
            (Some((w, h, row)), _) => {
                // a new (or larger) staging buffer
                let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("accum-readback-async"),
                    size: u64::from(row).saturating_mul(u64::from(h)).max(4),
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                let s = Slot { buf, key: None, layout: (w, h, row), state: Arc::new(AtomicU8::new(IDLE)), since: t, polls: 0, scopes: Vec::new() };
                match free {
                    Some(i) if i < self.slots.len() => {
                        if let Some(old) = self.slots.get_mut(i) {
                            old.buf.destroy();
                            *old = s;
                        }
                        Some(i)
                    }
                    _ => {
                        self.slots.push(s);
                        Some(self.slots.len() - 1)
                    }
                }
            }
            (None, _) => None,
        };
        let copied = slot.and_then(|i| self.slots.get(i)).and_then(|s| self.compositor.copy_accumulator_into(&mut enc, &s.buf));
        self.compositor.submit(enc);
        let scopes: Vec<ScopeAnswer> = vec![Box::pin(memory.pop()), Box::pin(validation.pop())];
        crate::export_renderer::timing::add_submit(t.elapsed());
        let (Some(i), Some(layout)) = (slot, copied) else {
            if layout.is_none() {
                self.fail("the composite has no accumulator".into());
            }
            return false;
        };
        let Some(s) = self.slots.get_mut(i) else { return false };
        s.key = Some(key);
        s.layout = layout;
        s.since = web_time::Instant::now();
        s.polls = 0;
        s.scopes = scopes;
        s.state.store(MAPPING, Ordering::Release);
        let state = s.state.clone();
        s.buf.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            state.store(if r.is_ok() { MAPPED } else { MAP_FAILED }, Ordering::Release);
        });
        true
    }

    fn overdue(s: &Slot) -> bool {
        s.since.elapsed() > MAP_TIMEOUT && s.polls >= MAP_TIMEOUT_POLLS
    }

    /// The mapped half floats of `buf` as f32 RGBA, a band of rows at a time.
    fn convert(buf: &wgpu::Buffer, w: u32, h: u32, row: u32) -> Option<Vec<f32>> {
        let width = w as usize;
        let len = width.checked_mul(h as usize)?.checked_mul(4)?;
        if len == 0 {
            return None;
        }
        let mut out = filmcraft_frame::pool::take_f32_overwritten(len);
        let mut y = 0u32;
        let ok = loop {
            if y >= h {
                break true;
            }
            let rows = BAND_ROWS.min(h - y);
            let start = u64::from(y) * u64::from(row);
            let end = start + u64::from(rows) * u64::from(row);
            let Ok(data) = buf.slice(start..end).get_mapped_range() else { break false };
            let Some(dst) = out.get_mut(y as usize * width * 4..(y + rows) as usize * width * 4) else { break false };
            if !crate::convert_half_rows(&data, dst, width, row as usize) {
                break false;
            }
            y += rows;
        };
        if ok {
            Some(out)
        } else {
            filmcraft_frame::pool::recycle_f32(out);
            None
        }
    }

    fn bytes(layout: Option<(u32, u32, u32)>) -> u64 {
        layout.map_or(u64::MAX, |(_, h, row)| u64::from(row).saturating_mul(u64::from(h)))
    }

    /// Let a native device run its callbacks (the web's run on the event loop by themselves).
    fn poll_device(&self) {
        let _ = self.device.poll(wgpu::PollType::Poll);
    }

    fn check_timeouts(&mut self) {
        for s in &mut self.slots {
            s.polls = s.polls.saturating_add(1);
        }
        if self.slots.iter().any(|s| s.key.is_some() && s.state.load(Ordering::Acquire) == MAPPING && Self::overdue(s)) {
            self.fail(format!("a read-back did not finish within {} s", MAP_TIMEOUT.as_secs()));
        }
    }

    /// The frame in slot `key`: converted and freed when mapped and its error scopes are clear.
    fn collect(&mut self, key: i64) -> AsyncFrame {
        self.poll_device();
        let Some(i) = self.slot_of(key) else { return AsyncFrame::Pending };
        let Some(s) = self.slots.get_mut(i) else { return AsyncFrame::Pending };
        // error scopes first: an error means this frame's pixels (and the device) can't be trusted
        let mut error = None;
        s.scopes.retain_mut(|f| match crate::poll_now(f.as_mut()) {
            Some(Some(e)) => {
                error = Some(format!("GPU error: {e}"));
                false
            }
            Some(None) => false,
            None => true,
        });
        let state = s.state.load(Ordering::Acquire);
        if error.is_none() && state == MAP_FAILED {
            error = Some("the read-back failed (GPU device lost?)".into());
        }
        let ready = state == MAPPED && s.scopes.is_empty();
        if !ready {
            s.polls = s.polls.saturating_add(1);
        }
        if error.is_none() && !ready && Self::overdue(s) {
            error = Some(format!("a read-back did not finish within {} s", MAP_TIMEOUT.as_secs()));
        }
        if let Some(e) = error {
            if state == MAPPED {
                s.buf.unmap();
                s.state.store(IDLE, Ordering::Release);
            } else if state == MAP_FAILED {
                s.state.store(IDLE, Ordering::Release);
            }
            self.fail(e);
            return AsyncFrame::Declined;
        }
        if !(state == MAPPED && s.scopes.is_empty()) {
            return AsyncFrame::Pending;
        }
        let t = web_time::Instant::now();
        let (w, h, row) = s.layout;
        let latency = s.since.elapsed();
        let px = Self::convert(&s.buf, w, h, row);
        s.buf.unmap();
        s.state.store(IDLE, Ordering::Release);
        s.key = None;
        crate::export_renderer::timing::add_convert(t.elapsed());
        match px {
            Some(px) => {
                crate::export_renderer::timing::add_frame();
                self.collected += 1;
                self.latency_total += latency;
                self.latency_max = self.latency_max.max(latency);
                AsyncFrame::Ready(Image { w: w as usize, h: h as usize, px })
            }
            None => {
                self.fail("the read-back was short".into());
                AsyncFrame::Declined
            }
        }
    }
}

impl Drop for AsyncExportRenderer {
    /// Staging buffers are destroyed now, not whenever the device next collects garbage (a
    /// cancelled 4K export leaves up to half a gigabyte of them).
    fn drop(&mut self) {
        for s in &self.slots {
            s.buf.destroy();
        }
    }
}
