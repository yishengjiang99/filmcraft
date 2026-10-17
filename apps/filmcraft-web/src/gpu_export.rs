//! GPU compositing for exports, on a WebGPU device (today eframe's).
//!
//! Desktop and CLI exports composite on an off-screen device of their own and read every frame
//! back synchronously ([`filmcraft_gpu::ExportRenderer`]). Neither works on a browser's main
//! thread: creating a device is asynchronous and a read-back completes only once the event loop
//! runs. So the web registers a frame renderer built on the device the UI already has, reading
//! back asynchronously ([`filmcraft_gpu::AsyncExportRenderer`]): a frame is submitted and
//! answered `Pending`, the stepped exporter yields, and a later step collects the picture.
//!
//! wgpu's WebGPU objects belong to the thread that made them (they are not `Send`), while a
//! [`FrameRenderer`] must be: the renderer lives in a thread-local of the UI thread, and the
//! registered [`FrameRenderer`] is a handle to it. A frame asked for on another thread (none
//! today: exports step on the UI thread) is declined and renders on the CPU.
//!
//! [`install`] takes plain `wgpu` handles, not eframe's: the export worker (Phase 4b) creates a
//! device of its own and installs it the same way.
//!
//! No WebGPU (`?webgl`, `?cpu`, a WebGL2 fallback): the factory returns `None` and exports
//! composite on the CPU, as do frames the GPU path declines (transitions, adjustment layers, HDR)
//! and every frame after a GPU error or a lost device.

use std::cell::{Cell, RefCell};

use filmcraft_export::{FrameOutcome, FrameRenderer};
use filmcraft_gpu::{AsyncExportRenderer, AsyncFrame, wgpu};
use filmcraft_project::{ItemId, Project};
use filmcraft_render::{Image, RenderOptions, SourceProvider};
use filmcraft_time::Tick;
use serde_json::json;

/// Staging buffers of one export together (1080p: 16 frames in flight; 4K: 7).
const READBACK_BUDGET: u64 = 512 << 20;

thread_local! {
    /// The device and queue exports composite on (eframe's, when the UI runs on WebGPU and the GPU
    /// compositor is allowed).
    static DEVICE: RefCell<Option<(wgpu::Device, wgpu::Queue)>> = const { RefCell::new(None) };
    /// The renderer of the export in progress (one at a time; keyed by its handle's id).
    static ACTIVE: RefCell<Option<(u64, AsyncExportRenderer)>> = const { RefCell::new(None) };
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
    /// Why the last GPU export renderer stopped, if it did.
    static LAST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Read-back latency of the last export: frames, total and maximum milliseconds.
    static LATENCY: Cell<(u64, f64, f64)> = const { Cell::new((0, 0.0, 0.0)) };
    /// Whether the last export's renderer ran layer effects on the GPU.
    static LAST_FX: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Offer GPU export rendering on `device` (called once at start-up, WebGPU only). Any device works;
/// nothing here touches the UI's resources.
pub fn install(device: &wgpu::Device, queue: &wgpu::Queue) {
    DEVICE.with(|d| *d.borrow_mut() = Some((device.clone(), queue.clone())));
    filmcraft_export::register_frame_renderer(factory);
}

fn factory() -> Option<Box<dyn FrameRenderer>> {
    let (device, queue) = DEVICE.with(|d| d.borrow().clone())?;
    // one renderer per export (the pipeline asks for several; the others would sit idle)
    if ACTIVE.with(|a| a.borrow().is_some()) {
        return None;
    }
    let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| AsyncExportRenderer::new(&device, &queue, READBACK_BUDGET)));
    let Ok(renderer) = made else {
        log::error!("GPU export renderer failed to initialise; exports render on the CPU");
        return None;
    };
    let id = NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id.wrapping_add(1));
        id
    });
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
    LATENCY.with(|l| l.set((0, 0.0, 0.0)));
    LAST_FX.with(|f| f.set(Some(renderer.has_effect_stage())));
    ACTIVE.with(|a| *a.borrow_mut() = Some((id, renderer)));
    Some(Box::new(Handle { id }))
}

/// The registered renderer: a reference to [`ACTIVE`] on the UI thread.
struct Handle {
    id: u64,
}

impl Handle {
    /// Run `f` on this handle's renderer; `None` on another thread or once it is gone.
    fn with<R>(&self, f: impl FnOnce(&mut AsyncExportRenderer) -> R) -> Option<R> {
        ACTIVE.with(|a| {
            let mut a = a.try_borrow_mut().ok()?;
            match a.as_mut() {
                Some((id, r)) if *id == self.id => {
                    let out = f(r);
                    LATENCY.with(|l| l.set((r.collected, r.latency_total.as_secs_f64() * 1e3, r.latency_max.as_secs_f64() * 1e3)));
                    if let Some(why) = r.failed() {
                        LAST_ERROR.with(|e| *e.borrow_mut() = Some(why.to_string()));
                    }
                    Some(out)
                }
                _ => None,
            }
        })
    }
}

fn incomplete() -> bool {
    filmcraft_media::pending::is_set()
}

impl FrameRenderer for Handle {
    fn render(&mut self, _: &Project, _: ItemId, _: Tick, _: RenderOptions, _: &dyn SourceProvider) -> Option<Image> {
        // only the asynchronous path below: a blocking read-back is impossible on this thread
        None
    }

    fn render_async(&mut self, project: &Project, seq: ItemId, t: Tick, opts: RenderOptions, sources: &dyn SourceProvider) -> FrameOutcome {
        match self.with(|r| r.render(project, seq, t, opts, sources, &incomplete)) {
            Some(AsyncFrame::Ready(img)) => FrameOutcome::Ready(img),
            Some(AsyncFrame::ReadyI420 { w, h, data }) => FrameOutcome::ReadyI420 { w, h, data },
            Some(AsyncFrame::Pending) => {
                // the read-back completes on the event loop: ask for the next UI frame
                crate::repaint();
                FrameOutcome::Pending
            }
            Some(AsyncFrame::Declined) | None => FrameOutcome::Declined,
        }
    }

    fn prefetch(&mut self, project: &Project, seq: ItemId, t: Tick, opts: RenderOptions, sources: &dyn SourceProvider) {
        self.with(|r| r.prefetch(project, seq, t, opts, sources, &incomplete));
    }

    fn frames_in_flight(&self, width: u32, height: u32) -> usize {
        self.with(|r| r.frames_in_flight(width, height)).unwrap_or(1)
    }

    fn set_i420(&mut self, on: bool) -> bool {
        self.with(|r| r.set_i420(on)).unwrap_or(false)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // (on the UI thread, where the export's pipeline is dropped; elsewhere this is a no-op)
        let _ = ACTIVE.try_with(|a| {
            if let Ok(mut a) = a.try_borrow_mut()
                && a.as_ref().is_some_and(|(id, _)| *id == self.id)
            {
                *a = None;
            }
        });
    }
}

/// `filmcraft.info().export` fields: which compositor drew the last export's frames.
pub fn stats() -> serde_json::Value {
    let c = filmcraft_export::last_export_render_stats();
    let compositor = match (c.gpu > 0, c.cpu > 0) {
        (true, false) => "gpu",
        (true, true) => "gpu+cpu",
        _ => "cpu",
    };
    let available = DEVICE.with(|d| d.borrow().is_some());
    json!({
        "compositor": compositor,
        "gpuFrames": c.gpu,
        "cpuFrames": c.cpu,
        "gpuAvailable": available,
        "gpuEffects": LAST_FX.with(Cell::get),
        "gpuError": LAST_ERROR.with(|e| e.borrow().clone()),
        // submission → collection of a GPU frame (GPU time plus waiting for the event loop)
        "gpuReadbackMs": LATENCY.with(|l| {
            let (n, total, max) = l.get();
            (n > 0).then(|| json!({"avg": total / n as f64, "max": max}))
        }),
        // timestamp-query pass timings: not collected (eframe's device is created without the
        // feature); null rather than a guess
        "gpu": null,
    })
}
