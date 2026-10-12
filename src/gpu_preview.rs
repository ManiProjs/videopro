use mpv_engine::ExportedFrame;
use slint::{Image, wgpu_30::wgpu};

pub struct GpuPreview {
    device: wgpu::Device,
    queue: wgpu::Queue,
    blitter: wgpu::util::TextureBlitter,
}

impl GpuPreview {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let blitter = wgpu::util::TextureBlitter::new(&device, wgpu::TextureFormat::Rgba8Unorm);
        Self {
            device,
            queue,
            blitter,
        }
    }

    pub fn import_frame(&self, frame: ExportedFrame) -> Result<Image, Box<dyn std::error::Error>> {
        let width = frame.width();
        let height = frame.height();
        let source = frame.into_wgpu_texture(&self.device)?;
        let source_view = source.create_view(&wgpu::TextureViewDescriptor::default());

        // Slint accepts RGBA textures. The blitter samples mpv's BGRA GPU
        // texture and writes the correctly interpreted pixels directly to
        // a Slint-compatible texture, without reading pixels back to the CPU.
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("VideoPro preview frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("VideoPro preview conversion"),
            });
        self.blitter
            .copy(&self.device, &mut encoder, &source_view, &target_view);
        self.queue.submit([encoder.finish()]);

        Ok(Image::try_from(target)?)
    }
}
