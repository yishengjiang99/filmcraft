//! The export's I420 picture computed on the GPU from the compositor's accumulator (`i420.wgsl`):
//! the same bytes, within one code value, as `Image::over_black_rgba8` followed by
//! `filmcraft_export::rgba_to_yuv420_8` on the CPU. Reading back 1.5 bytes a pixel instead of the
//! accumulator's 8 (half-float RGBA) also removes the CPU's half → f32, sRGB and YUV passes.

/// Bytes of a `w`×`h` I420 picture: Y, then U and V at half size (rounded up).
pub fn i420_len(w: u32, h: u32) -> Option<u64> {
    let (w, h) = (u64::from(w), u64::from(h));
    let c = w.div_ceil(2).checked_mul(h.div_ceil(2))?;
    w.checked_mul(h)?.checked_add(c.checked_mul(2)?)
}

/// The pass's buffer size: [`i420_len`] rounded up to whole words.
pub(crate) fn i420_padded(w: u32, h: u32) -> Option<u64> {
    i420_len(w, h)?.div_ceil(4).checked_mul(4)
}

pub(crate) struct I420Stage {
    pipeline: wgpu::ComputePipeline,
    bgl: wgpu::BindGroupLayout,
    srgb: wgpu::Buffer,
    /// The pass's output, reused while the size stays: (buffer, bytes).
    out: Option<(wgpu::Buffer, u64)>,
}

/// Workgroups along x before the grid wraps into y (the per-dimension limit is 65535).
const ROW_GROUPS: u32 = 32768;

impl I420Stage {
    pub(crate) fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("filmcraft-i420"),
            source: wgpu::ShaderSource::Wgsl(include_str!("i420.wgsl").into()),
        });
        let buf = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("i420"),
            entries: &[
                buf(0, wgpu::BufferBindingType::Uniform),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                buf(2, wgpu::BufferBindingType::Storage { read_only: true }),
                buf(3, wgpu::BufferBindingType::Storage { read_only: false }),
            ],
        });
        let layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("i420"), bind_group_layouts: &[Some(&bgl)], immediate_size: 0 });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("i420"),
            layout: Some(&layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let table: Vec<u8> = filmcraft_color::linear_to_srgb_u8_table().iter().flat_map(|v| u32::from(*v).to_le_bytes()).collect();
        let srgb = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("i420-srgb"),
            size: table.len() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&srgb, 0, &table);
        Self { pipeline, bgl, srgb, out: None }
    }

    /// Record the pass over `accum` (`w`×`h`) and a copy of its result into `dst` (at least
    /// [`i420_padded`] bytes). False when the size is unusable or `dst` too small.
    pub(crate) fn record(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        enc: &mut wgpu::CommandEncoder,
        accum: &wgpu::TextureView,
        (w, h): (u32, u32),
        dst: &wgpu::Buffer,
    ) -> bool {
        let Some(bytes) = i420_padded(w, h) else { return false };
        if w == 0 || h == 0 || dst.size() < bytes {
            return false;
        }
        let Ok(words) = u32::try_from(bytes / 4) else { return false };
        if self.out.as_ref().is_none_or(|(_, n)| *n != bytes) {
            let b = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("i420-out"),
                size: bytes,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            self.out = Some((b, bytes));
        }
        let Some((out, _)) = self.out.as_ref() else { return false };
        let mut u = Vec::with_capacity(32);
        for v in [w, h, w.div_ceil(2), h.div_ceil(2), words, 0, 0, 0] {
            u.extend_from_slice(&v.to_le_bytes());
        }
        let ub = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("i420-u"),
            size: u.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&ub, 0, &u);
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("i420"),
            layout: &self.bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: ub.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(accum) },
                wgpu::BindGroupEntry { binding: 2, resource: self.srgb.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: out.as_entire_binding() },
            ],
        });
        let groups = words.div_ceil(64);
        let (gx, gy) = if groups <= ROW_GROUPS { (groups.max(1), 1) } else { (ROW_GROUPS, groups.div_ceil(ROW_GROUPS)) };
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("i420"), timestamp_writes: None });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(gx, gy, 1);
        }
        enc.copy_buffer_to_buffer(out, 0, dst, 0, bytes);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_round_chroma_up_and_pad_to_words() {
        assert_eq!(i420_len(4, 2), Some(8 + 2 * 2));
        assert_eq!(i420_len(3, 3), Some(9 + 2 * 4));
        assert_eq!(i420_padded(3, 3), Some(20));
        assert_eq!(i420_len(3840, 2160), Some(3840 * 2160 * 3 / 2));
        assert_eq!(i420_len(0, 5), Some(0));
        // hostile sizes: None, never an overflow
        assert_eq!(i420_len(u32::MAX, u32::MAX), None);
        assert_eq!(i420_padded(u32::MAX, u32::MAX), None);
        assert!(i420_len(u32::MAX, 1).is_some());
    }
}
