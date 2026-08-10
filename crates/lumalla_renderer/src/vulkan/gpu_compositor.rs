//! GPU compositing: surface texture cache and scanout render pass.

use std::collections::HashMap;
use std::collections::HashSet;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd};
use std::ptr;

use anyhow::Context;
use ash::vk;

use crate::default_cursor::default_cursor_frame;
use crate::scene_backing::{CompositeMode, DamageRect, UploadRect};
use crate::{CursorFrame, DmabufAttachment, SurfaceFrame};

const WL_SHM_FORMAT_XRGB8888: u32 = 1;

use super::{
    CommandBufferRecorder, CommandPool, DescriptorPool, DescriptorSetLayout, Device,
    DmaBufImage, Fence, Framebuffer, GraphicsPipeline, GraphicsPipelineBuilder, Image,
    PhysicalDevice, RenderPass, Sampler, ShaderModule, VulkanContext, drm_fourcc_to_vulkan,
};

const MAX_SURFACE_TEXTURES: u32 = 256;
const CURSOR_TEXTURE_KEY: (u32, u32) = (u32::MAX, u32::MAX);

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct LayerPushConstants {
    dest: [f32; 4],
    output_size: [f32; 2],
    force_opaque: f32,
    _padding: f32,
}

pub struct GpuCompositor {
    pipeline: GraphicsPipeline,
    _vert_shader: ShaderModule,
    _frag_shader: ShaderModule,
    descriptor_layout: DescriptorSetLayout,
    descriptor_pool: DescriptorPool,
    sampler: Sampler,
}

impl GpuCompositor {
    pub fn new(
        device: &Device,
        render_pass: &RenderPass,
    ) -> anyhow::Result<Self> {
        let vert_spv = spv_from_bytes(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/composite.vert.spv"
        )));
        let frag_spv = spv_from_bytes(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/composite.frag.spv"
        )));

        let vert_shader = ShaderModule::from_spirv(device, &vert_spv)?;
        let frag_shader = ShaderModule::from_spirv(device, &frag_spv)?;
        let descriptor_layout = DescriptorSetLayout::new_texture_sampler(device)?;
        let descriptor_pool =
            DescriptorPool::new_combined_image_sampler(device, MAX_SURFACE_TEXTURES)?;
        let sampler = Sampler::new_nearest(device)?;

        let push_constants = vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
            offset: 0,
            size: mem::size_of::<LayerPushConstants>() as u32,
        };

        let pipeline = GraphicsPipelineBuilder::new(device, render_pass)
            .vertex_shader(&vert_shader)
            .fragment_shader(&frag_shader)
            .descriptor_set_layout(descriptor_layout.handle())
            .push_constant_range(push_constants)
            .build()?;

        Ok(Self {
            pipeline,
            _vert_shader: vert_shader,
            _frag_shader: frag_shader,
            descriptor_layout,
            descriptor_pool,
            sampler,
        })
    }

    fn draw_layer(
        &self,
        device: &Device,
        recorder: &mut CommandBufferRecorder,
        texture: &SurfaceTexture,
        dest: [f32; 4],
        output_width: u32,
        output_height: u32,
        force_opaque: bool,
        clip: Option<&vk::Rect2D>,
    ) {
        recorder.bind_pipeline(&self.pipeline);
        recorder.bind_descriptor_sets(
            self.pipeline.layout(),
            0,
            &[texture.descriptor_set],
            &[],
        );
        let push = LayerPushConstants {
            dest,
            output_size: [output_width as f32, output_height as f32],
            force_opaque: if force_opaque { 1.0 } else { 0.0 },
            _padding: 0.0,
        };
        unsafe {
            device.handle().cmd_push_constants(
                recorder.command_buffer(),
                self.pipeline.layout(),
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                bytemuck::bytes_of(&push),
            );
        }
        recorder.set_viewport_fullscreen(output_width, output_height);
        if let Some(clip) = clip {
            recorder.set_scissor(clip);
        } else {
            recorder.set_scissor_fullscreen(output_width, output_height);
        }
        recorder.draw_fullscreen_quad();
    }
}

struct SurfaceTexture {
    backing: TextureBacking,
    descriptor_set: vk::DescriptorSet,
    wl_format: u32,
    uploaded: bool,
    buffer_id: u32,
    dmabuf_modifier: u64,
    dmabuf_stride: u32,
    dmabuf_offset: u32,
    dmabuf_width: u32,
    dmabuf_height: u32,
}

enum TextureBacking {
    Shm(Image),
    Dmabuf(DmaBufImage),
}

impl TextureBacking {
    fn view(&self) -> vk::ImageView {
        match self {
            Self::Shm(image) => image.view(),
            Self::Dmabuf(image) => image.view(),
        }
    }
}

pub struct SurfaceTextureCache {
    textures: HashMap<(u32, u32), SurfaceTexture>,
}

impl SurfaceTextureCache {
    pub fn new() -> Self {
        Self {
            textures: HashMap::new(),
        }
    }

    pub fn clear(&mut self) {
        self.textures.clear();
    }

    pub fn remove(&mut self, key: (u32, u32)) {
        self.textures.remove(&key);
    }

    pub fn remove_client(&mut self, owner_id: u32) {
        self.textures
            .retain(|(owner, _), _| *owner != owner_id);
    }

    fn texture(&self, key: (u32, u32)) -> Option<&SurfaceTexture> {
        self.textures.get(&key)
    }

    fn sync_cursor(
        &mut self,
        vulkan: &mut VulkanContext,
        compositor: &GpuCompositor,
        cursor: Option<&CursorFrame>,
    ) -> anyhow::Result<()> {
        match cursor {
            Some(frame) => {
                let key = (frame.owner_id, frame.surface_id);
                if let Some(dmabuf) = frame.dmabuf.as_ref() {
                    let surface = cursor_surface_view(frame);
                    self.sync_dmabuf(vulkan, compositor, key, &surface, dmabuf)
                } else {
                    self.sync_shm_pixels(
                        vulkan,
                        compositor,
                        key,
                        frame.buffer_id,
                        &frame.pixels,
                        frame.width as u32,
                        frame.height as u32,
                        frame.stride as u32,
                        frame.format,
                    )
                }
            }
            None => {
                let default = default_cursor_frame();
                self.sync_shm_pixels(
                    vulkan,
                    compositor,
                    CURSOR_TEXTURE_KEY,
                    default.buffer_id,
                    &default.pixels,
                    default.width as u32,
                    default.height as u32,
                    default.stride as u32,
                    default.format,
                )
            }
        }
    }

    fn sync_frame(
        &mut self,
        vulkan: &mut VulkanContext,
        compositor: &GpuCompositor,
        frame: &SurfaceFrame,
    ) -> anyhow::Result<()> {
        let key = (frame.owner_id, frame.surface_id);
        if let Some(dmabuf) = frame.dmabuf.as_ref() {
            self.sync_dmabuf(vulkan, compositor, key, frame, dmabuf)
        } else {
            self.sync_shm_frame(vulkan, compositor, frame)
        }
    }

    fn sync_shm_frame(
        &mut self,
        vulkan: &mut VulkanContext,
        compositor: &GpuCompositor,
        frame: &SurfaceFrame,
    ) -> anyhow::Result<()> {
        let key = (frame.owner_id, frame.surface_id);
        let width = frame.width as u32;
        let height = frame.height as u32;
        let stride = frame.stride as u32;
        let needs_create = self.textures.get(&key).is_none_or(|tex| {
            !matches!(tex.backing, TextureBacking::Shm(_))
                || tex.extent().is_none_or(|extent| extent.width != width || extent.height != height)
        });

        if needs_create {
            let image = vulkan.create_sampled_image(width, height)?;
            let descriptor_set = compositor
                .descriptor_pool
                .allocate_sampler_set(vulkan.device(), &compositor.descriptor_layout)?;
            self.textures.insert(
                key,
                SurfaceTexture {
                    backing: TextureBacking::Shm(image),
                    descriptor_set,
                    wl_format: frame.format,
                    uploaded: false,
                    buffer_id: frame.buffer_id,
                    dmabuf_modifier: 0,
                    dmabuf_stride: 0,
                    dmabuf_offset: 0,
                    dmabuf_width: 0,
                    dmabuf_height: 0,
                },
            );
        }

        let texture = self
            .textures
            .get_mut(&key)
            .context("Surface texture missing after create")?;
        texture.wl_format = frame.format;
        texture.buffer_id = frame.buffer_id;

        let image = match &texture.backing {
            TextureBacking::Shm(image) => image,
            TextureBacking::Dmabuf(_) => anyhow::bail!("SHM upload targeted imported DMA-BUF"),
        };

        if needs_create || frame.full_surface || frame.damage.is_empty() {
            upload_bgra_texture(
                vulkan.device(),
                vulkan.physical_device(),
                vulkan.graphics_command_pool(),
                image,
                &frame.pixels,
                width,
                height,
                stride,
                texture.uploaded,
                None,
            )?;
        } else {
            for region in frame
                .damage
                .iter()
                .filter_map(|rect| output_damage_to_buffer_rect(frame, *rect))
            {
                upload_bgra_texture(
                    vulkan.device(),
                    vulkan.physical_device(),
                    vulkan.graphics_command_pool(),
                    image,
                    &frame.pixels,
                    width,
                    height,
                    stride,
                    texture.uploaded,
                    Some(region),
                )?;
            }
        }
        texture.uploaded = true;

        write_texture_descriptor(
            vulkan.device(),
            texture.descriptor_set,
            image.view(),
            compositor.sampler.handle(),
        );

        Ok(())
    }

    fn sync_dmabuf(
        &mut self,
        vulkan: &mut VulkanContext,
        compositor: &GpuCompositor,
        key: (u32, u32),
        frame: &SurfaceFrame,
        dmabuf: &DmabufAttachment,
    ) -> anyhow::Result<()> {
        let width = frame.width as u32;
        let height = frame.height as u32;
        let stride = frame.stride as u32;
        let can_reuse = self.textures.get(&key).is_some_and(|tex| {
            matches!(tex.backing, TextureBacking::Dmabuf(_))
                && tex.buffer_id == dmabuf.buffer_id
                && tex.wl_format == frame.format
                && tex.dmabuf_modifier == dmabuf.modifier
                && tex.dmabuf_stride == stride
                && tex.dmabuf_offset == dmabuf.offset
                && tex.dmabuf_width == width
                && tex.dmabuf_height == height
        });

        if can_reuse {
            // Same import identity, but client content may have changed — re-acquire.
            let image = {
                let tex = self.textures.get(&key).context("Missing reused DMA-BUF texture")?;
                match &tex.backing {
                    TextureBacking::Dmabuf(image) => image.image(),
                    TextureBacking::Shm(_) => anyhow::bail!("Expected DMA-BUF backing"),
                }
            };
            acquire_dmabuf_for_sample(
                vulkan.device(),
                vulkan.graphics_command_pool(),
                image,
                false,
            )?;
            return Ok(());
        }

        let format = drm_fourcc_to_vulkan(dmabuf.drm_fourcc)
            .with_context(|| format!("Unsupported DRM fourcc {:#x}", dmabuf.drm_fourcc))?;
        let import_fd = dup_fd(dmabuf.fd.as_raw_fd())?;
        let imported = DmaBufImage::import_from_dma_buf(
            vulkan.device(),
            vulkan.physical_device(),
            import_fd,
            width,
            height,
            format,
            dmabuf.modifier,
            dmabuf.offset as u64,
            stride,
        )?;
        acquire_dmabuf_for_sample(
            vulkan.device(),
            vulkan.graphics_command_pool(),
            imported.image(),
            true,
        )?;

        let descriptor_set = compositor
            .descriptor_pool
            .allocate_sampler_set(vulkan.device(), &compositor.descriptor_layout)?;
        write_texture_descriptor(
            vulkan.device(),
            descriptor_set,
            imported.view(),
            compositor.sampler.handle(),
        );

        self.textures.insert(
            key,
            SurfaceTexture {
                backing: TextureBacking::Dmabuf(imported),
                descriptor_set,
                wl_format: frame.format,
                uploaded: true,
                buffer_id: dmabuf.buffer_id,
                dmabuf_modifier: dmabuf.modifier,
                dmabuf_stride: stride,
                dmabuf_offset: dmabuf.offset,
                dmabuf_width: width,
                dmabuf_height: height,
            },
        );
        Ok(())
    }

    fn sync_shm_pixels(
        &mut self,
        vulkan: &mut VulkanContext,
        compositor: &GpuCompositor,
        key: (u32, u32),
        buffer_id: u32,
        pixels: &[u8],
        width: u32,
        height: u32,
        stride: u32,
        wl_format: u32,
    ) -> anyhow::Result<()> {
        let needs_create = self.textures.get(&key).is_none_or(|tex| {
            !matches!(tex.backing, TextureBacking::Shm(_))
                || tex.extent().is_none_or(|extent| extent.width != width || extent.height != height)
        });

        if needs_create {
            let image = vulkan.create_sampled_image(width, height)?;
            let descriptor_set = compositor
                .descriptor_pool
                .allocate_sampler_set(vulkan.device(), &compositor.descriptor_layout)?;
            self.textures.insert(
                key,
                SurfaceTexture {
                    backing: TextureBacking::Shm(image),
                    descriptor_set,
                    wl_format,
                    uploaded: false,
                    buffer_id,
                    dmabuf_modifier: 0,
                    dmabuf_stride: 0,
                    dmabuf_offset: 0,
                    dmabuf_width: 0,
                    dmabuf_height: 0,
                },
            );
        }

        let texture = self
            .textures
            .get_mut(&key)
            .context("Surface texture missing after create")?;
        texture.wl_format = wl_format;
        texture.buffer_id = buffer_id;

        let image = match &texture.backing {
            TextureBacking::Shm(image) => image,
            TextureBacking::Dmabuf(_) => anyhow::bail!("SHM upload targeted imported DMA-BUF"),
        };
        upload_bgra_texture(
            vulkan.device(),
            vulkan.physical_device(),
            vulkan.graphics_command_pool(),
            image,
            pixels,
            width,
            height,
            stride,
            texture.uploaded,
            None,
        )?;
        texture.uploaded = true;

        write_texture_descriptor(
            vulkan.device(),
            texture.descriptor_set,
            image.view(),
            compositor.sampler.handle(),
        );

        Ok(())
    }

    pub fn sync_scene(
        &mut self,
        vulkan: &mut VulkanContext,
        compositor: &GpuCompositor,
        layers: &[&SurfaceFrame],
        cursor: Option<&CursorFrame>,
        composite_mode: &CompositeMode,
        dirty_surfaces: &HashSet<(u32, u32)>,
        sync_cursor: bool,
    ) -> anyhow::Result<()> {
        let sync_all = matches!(composite_mode, CompositeMode::Full);
        for frame in layers {
            let key = (frame.owner_id, frame.surface_id);
            if sync_all || dirty_surfaces.contains(&key) {
                self.sync_frame(vulkan, compositor, frame)?;
            }
        }
        if sync_all || sync_cursor {
            self.sync_cursor(vulkan, compositor, cursor)?;
        }
        Ok(())
    }
}

impl SurfaceTexture {
    fn extent(&self) -> Option<vk::Extent2D> {
        match &self.backing {
            TextureBacking::Shm(image) => Some(image.extent()),
            TextureBacking::Dmabuf(image) => Some(image.extent()),
        }
    }
}

fn cursor_surface_view(cursor: &CursorFrame) -> SurfaceFrame {
    SurfaceFrame {
        owner_id: cursor.owner_id,
        surface_id: cursor.surface_id,
        buffer_id: cursor.buffer_id,
        pixels: Vec::new(),
        width: cursor.width,
        height: cursor.height,
        stride: cursor.stride,
        format: cursor.format,
        x: 0,
        y: 0,
        buffer_scale: cursor.buffer_scale,
        dmabuf: None,
        damage: Vec::new(),
        full_surface: true,
    }
}

fn dup_fd(fd: std::os::fd::RawFd) -> anyhow::Result<std::os::fd::OwnedFd> {
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    anyhow::ensure!(dup >= 0, "Failed to duplicate DMA-BUF fd");
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(dup) })
}

fn acquire_dmabuf_for_sample(
    device: &Device,
    command_pool: &CommandPool,
    image: vk::Image,
    first_import: bool,
) -> anyhow::Result<()> {
    let command_buffer = command_pool.allocate_command_buffer(device)?;
    let graphics_family = device.graphics_queue_family();
    let record_result = (|| -> anyhow::Result<()> {
        let recorder = CommandBufferRecorder::begin_one_time(device, command_buffer)?;
        let (old_layout, src_access, src_stage) = if first_import {
            (
                vk::ImageLayout::UNDEFINED,
                vk::AccessFlags::empty(),
                vk::PipelineStageFlags::TOP_OF_PIPE,
            )
        } else {
            (
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::AccessFlags::SHADER_READ,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
            )
        };
        let barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(src_access)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
            .dst_queue_family_index(graphics_family)
            .image(image)
            .subresource_range(color_subresource_range());
        unsafe {
            device.handle().cmd_pipeline_barrier(
                recorder.command_buffer(),
                src_stage,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        }
        recorder.end()?;
        Ok(())
    })();
    if let Err(error) = record_result {
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }
    let fence = match Fence::new(device, false) {
        Ok(fence) => fence,
        Err(error) => {
            command_pool.free_command_buffers(device, &[command_buffer]);
            return Err(error);
        }
    };
    if let Err(error) = device.submit_graphics(&[command_buffer], &[], &[], &[], fence.handle()) {
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }
    if let Err(error) = fence
        .wait_default()
        .context("Timed out acquiring imported DMA-BUF")
    {
        let _ = device.wait_idle();
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }
    command_pool.free_command_buffers(device, &[command_buffer]);
    Ok(())
}

/// Copies the displayed scanout image into a back buffer before incremental compositing.
pub fn copy_scanout_frame(
    vulkan: &VulkanContext,
    src: &DmaBufImage,
    dst: &DmaBufImage,
    dst_was_fresh: bool,
) -> anyhow::Result<()> {
    let device = vulkan.device();
    let command_pool = vulkan.graphics_command_pool();
    let command_buffer = command_pool.allocate_command_buffer(device)?;
    let extent = src.extent();

    let dst_old_layout = if dst_was_fresh {
        vk::ImageLayout::UNDEFINED
    } else {
        vk::ImageLayout::GENERAL
    };

    let record_result = (|| -> anyhow::Result<()> {
        let recorder = CommandBufferRecorder::begin_one_time(device, command_buffer)?;
        let cb = recorder.command_buffer();

        let src_barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(src.image())
            .subresource_range(color_subresource_range());
        let (dst_src_access, _dst_src_stage) = if dst_was_fresh {
            (
                vk::AccessFlags::empty(),
                vk::PipelineStageFlags::TOP_OF_PIPE,
            )
        } else {
            (
                vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE,
                vk::PipelineStageFlags::ALL_COMMANDS,
            )
        };
        let dst_barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(dst_src_access)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .old_layout(dst_old_layout)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(dst.image())
            .subresource_range(color_subresource_range());
        unsafe {
            device.handle().cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[src_barrier, dst_barrier],
            );
        }

        let copy_region = vk::ImageCopy::default()
            .src_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .dst_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            });
        unsafe {
            device.handle().cmd_copy_image(
                cb,
                src.image(),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst.image(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[copy_region],
            );
        }

        let src_back = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(src.image())
            .subresource_range(color_subresource_range());
        let dst_back = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(dst.image())
            .subresource_range(color_subresource_range());
        unsafe {
            device.handle().cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[src_back, dst_back],
            );
        }

        recorder.end()?;
        Ok(())
    })();

    if let Err(error) = record_result {
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }

    let fence = match Fence::new(device, false) {
        Ok(fence) => fence,
        Err(error) => {
            command_pool.free_command_buffers(device, &[command_buffer]);
            return Err(error);
        }
    };
    if let Err(error) = device.submit_graphics(&[command_buffer], &[], &[], &[], fence.handle()) {
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }
    if let Err(error) = fence
        .wait_default()
        .context("Timed out copying scanout frame for partial composite")
    {
        let _ = device.wait_idle();
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }
    command_pool.free_command_buffers(device, &[command_buffer]);
    Ok(())
}

pub fn composite_to_scanout(
    vulkan: &VulkanContext,
    compositor: &GpuCompositor,
    cache: &SurfaceTextureCache,
    render_pass: &RenderPass,
    scanout_image: &DmaBufImage,
    framebuffer: &Framebuffer,
    scanout_old_layout: vk::ImageLayout,
    output_width: u32,
    output_height: u32,
    clear_color: [f32; 4],
    composite_mode: CompositeMode,
    layers: &[&SurfaceFrame],
    cursor: Option<&CursorFrame>,
    pointer_x: i32,
    pointer_y: i32,
) -> anyhow::Result<()> {
    let clear_value = vk::ClearValue {
        color: vk::ClearColorValue {
            float32: clear_color,
        },
    };

    let device = vulkan.device();
    let command_pool = vulkan.graphics_command_pool();
    let command_buffer = command_pool.allocate_command_buffer(device)?;

    let record_result = (|| -> anyhow::Result<()> {
        transition_scanout_for_render(
            device,
            command_buffer,
            scanout_image,
            scanout_old_layout,
        )?;

        let mut recorder = CommandBufferRecorder::begin_one_time(device, command_buffer)?;
        recorder.begin_render_pass(render_pass, framebuffer, &[clear_value])?;

        match composite_mode {
            CompositeMode::Full => {
                draw_scene_layers(
                    compositor,
                    device,
                    &mut recorder,
                    cache,
                    layers,
                    output_width,
                    output_height,
                    None,
                );
                draw_cursor_layer(
                    compositor,
                    device,
                    &mut recorder,
                    cache,
                    cursor,
                    pointer_x,
                    pointer_y,
                    output_width,
                    output_height,
                    None,
                );
            }
            CompositeMode::Partial(regions) => {
                for region in regions {
                    let clip = upload_rect_to_vk(region);
                    // ClearAttachments is clipped by the dynamic scissor.
                    recorder.set_scissor(&clip);
                    recorder.clear_color_rects(clear_color, &[clip]);
                    draw_scene_layers(
                        compositor,
                        device,
                        &mut recorder,
                        cache,
                        layers,
                        output_width,
                        output_height,
                        Some(&clip),
                    );
                    draw_cursor_layer(
                        compositor,
                        device,
                        &mut recorder,
                        cache,
                        cursor,
                        pointer_x,
                        pointer_y,
                        output_width,
                        output_height,
                        Some(&clip),
                    );
                }
            }
        }

        recorder.end_render_pass();
        recorder.end()?;
        Ok(())
    })();

    if let Err(error) = record_result {
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }

    let fence = match Fence::new(device, false) {
        Ok(fence) => fence,
        Err(error) => {
            command_pool.free_command_buffers(device, &[command_buffer]);
            return Err(error);
        }
    };
    if let Err(error) = device.submit_graphics(&[command_buffer], &[], &[], &[], fence.handle()) {
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }
    if let Err(error) = fence
        .wait_default()
        .context("Timed out waiting for GPU composite to complete")
    {
        let _ = device.wait_idle();
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }
    command_pool.free_command_buffers(device, &[command_buffer]);
    Ok(())
}

fn draw_scene_layers(
    compositor: &GpuCompositor,
    device: &Device,
    recorder: &mut CommandBufferRecorder<'_>,
    cache: &SurfaceTextureCache,
    layers: &[&SurfaceFrame],
    output_width: u32,
    output_height: u32,
    clip: Option<&vk::Rect2D>,
) {
    for frame in layers {
        let key = (frame.owner_id, frame.surface_id);
        let Some(texture) = cache.texture(key) else {
            continue;
        };
        let dest = surface_dest_rect(frame);
        if dest[2] <= 0.0 || dest[3] <= 0.0 {
            continue;
        }
        if let Some(clip) = clip {
            if !dest_intersects_clip(dest, clip) {
                continue;
            }
        }
        compositor.draw_layer(
            device,
            recorder,
            texture,
            dest,
            output_width,
            output_height,
            frame.format == WL_SHM_FORMAT_XRGB8888,
            clip,
        );
    }
}

fn draw_cursor_layer(
    compositor: &GpuCompositor,
    device: &Device,
    recorder: &mut CommandBufferRecorder<'_>,
    cache: &SurfaceTextureCache,
    cursor: Option<&CursorFrame>,
    pointer_x: i32,
    pointer_y: i32,
    output_width: u32,
    output_height: u32,
    clip: Option<&vk::Rect2D>,
) {
    let cursor_key = cursor
        .map(|c| (c.owner_id, c.surface_id))
        .unwrap_or(CURSOR_TEXTURE_KEY);
    let Some(texture) = cache.texture(cursor_key) else {
        return;
    };
    match cursor {
        Some(cursor_frame) => {
            let dest = cursor_dest_rect(cursor_frame, pointer_x, pointer_y);
            if dest[2] > 0.0
                && dest[3] > 0.0
                && clip.is_none_or(|clip| dest_intersects_clip(dest, clip))
            {
                compositor.draw_layer(
                    device,
                    recorder,
                    texture,
                    dest,
                    output_width,
                    output_height,
                    cursor_frame.format == WL_SHM_FORMAT_XRGB8888,
                    clip,
                );
            }
        }
        None => {
            let default = default_cursor_frame();
            let dest = cursor_dest_rect(default, pointer_x, pointer_y);
            if dest[2] > 0.0
                && dest[3] > 0.0
                && clip.is_none_or(|clip| dest_intersects_clip(dest, clip))
            {
                compositor.draw_layer(
                    device,
                    recorder,
                    texture,
                    dest,
                    output_width,
                    output_height,
                    default.format == WL_SHM_FORMAT_XRGB8888,
                    clip,
                );
            }
        }
    }
}

fn surface_dest_rect(frame: &SurfaceFrame) -> [f32; 4] {
    let scale = frame.buffer_scale.max(1) as f32;
    [
        frame.x as f32,
        frame.y as f32,
        frame.width as f32 / scale,
        frame.height as f32 / scale,
    ]
}

fn cursor_dest_rect(cursor: &CursorFrame, pointer_x: i32, pointer_y: i32) -> [f32; 4] {
    let dest_w = div_ceil_i32(cursor.width as i32, cursor.buffer_scale.max(1)) as f32;
    let dest_h = div_ceil_i32(cursor.height as i32, cursor.buffer_scale.max(1)) as f32;
    [
        (pointer_x - cursor.hotspot_x) as f32,
        (pointer_y - cursor.hotspot_y) as f32,
        dest_w,
        dest_h,
    ]
}

fn div_ceil_i32(value: i32, divisor: i32) -> i32 {
    if divisor <= 0 {
        return value;
    }
    (value + divisor - 1) / divisor
}

fn transition_scanout_for_render(
    device: &Device,
    command_buffer: vk::CommandBuffer,
    image: &DmaBufImage,
    old_layout: vk::ImageLayout,
) -> anyhow::Result<()> {
    if old_layout == vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL {
        return Ok(());
    }

    let (src_access, src_stage) = match old_layout {
        vk::ImageLayout::UNDEFINED => (
            vk::AccessFlags::empty(),
            vk::PipelineStageFlags::TOP_OF_PIPE,
        ),
        vk::ImageLayout::GENERAL => (
            vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE,
            vk::PipelineStageFlags::ALL_COMMANDS,
        ),
        _ => (
            vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE,
            vk::PipelineStageFlags::ALL_COMMANDS,
        ),
    };

    let barrier = vk::ImageMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image.image())
        .subresource_range(color_subresource_range());
    unsafe {
        device.handle().cmd_pipeline_barrier(
            command_buffer,
            src_stage,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );
    }
    Ok(())
}

fn upload_bgra_texture(
    device: &Device,
    physical_device: &PhysicalDevice,
    command_pool: &CommandPool,
    image: &Image,
    pixels: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    previously_uploaded: bool,
    region: Option<UploadRect>,
) -> anyhow::Result<()> {
    let row_bytes = usize::try_from(stride).context("Stride overflows")?;
    let full_size = row_bytes
        .checked_mul(height as usize)
        .context("Texture size overflows")?;
    anyhow::ensure!(pixels.len() >= full_size, "Texture pixel data is truncated");

    let (staging_bytes, copy_stride, copy_height, image_offset, image_extent) =
        match region {
            Some(region) => {
                let region_row_bytes = usize::try_from(region.width)
                    .context("Region width overflows")?
                    .checked_mul(4)
                    .context("Region row bytes overflow")?;
                let region_size = region_row_bytes
                    .checked_mul(region.height as usize)
                    .context("Region size overflows")?;
                let mut staging_bytes = vec![0u8; region_size];
                for row in 0..region.height {
                    let src_row = (region.y + row) as usize;
                    let src_start = src_row
                        .checked_mul(row_bytes)
                        .and_then(|offset| offset.checked_add(region.x as usize * 4))
                        .context("Region source offset overflows")?;
                    let dst_start = row as usize * region_row_bytes;
                    let dst_end = dst_start + region_row_bytes;
                    staging_bytes[dst_start..dst_end]
                        .copy_from_slice(&pixels[src_start..src_start + region_row_bytes]);
                }
                (
                    staging_bytes,
                    region.width,
                    region.height,
                    vk::Offset3D {
                        x: region.x as i32,
                        y: region.y as i32,
                        z: 0,
                    },
                    vk::Extent3D {
                        width: region.width,
                        height: region.height,
                        depth: 1,
                    },
                )
            }
            None => (
                pixels[..full_size].to_vec(),
                width,
                height,
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                },
            ),
        };

    let staging = StagingBuffer::new(device, physical_device, &staging_bytes)?;
    let command_buffer = command_pool.allocate_command_buffer(device)?;

    let record_result = (|| -> anyhow::Result<()> {
        let recorder = CommandBufferRecorder::begin_one_time(device, command_buffer)?;
        let old_layout = if previously_uploaded {
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        } else {
            vk::ImageLayout::UNDEFINED
        };
        let to_transfer = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::MEMORY_READ)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image.image())
            .subresource_range(color_subresource_range());
        unsafe {
            device.handle().cmd_pipeline_barrier(
                recorder.command_buffer(),
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_transfer],
            );
        }

        let region = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(copy_stride)
            .buffer_image_height(copy_height)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_offset(image_offset)
            .image_extent(image_extent);
        unsafe {
            device.handle().cmd_copy_buffer_to_image(
                recorder.command_buffer(),
                staging.buffer,
                image.image(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
        }

        let to_sample = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image.image())
            .subresource_range(color_subresource_range());
        unsafe {
            device.handle().cmd_pipeline_barrier(
                recorder.command_buffer(),
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_sample],
            );
        }
        recorder.end()?;
        Ok(())
    })();

    if let Err(error) = record_result {
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }

    let fence = match Fence::new(device, false) {
        Ok(fence) => fence,
        Err(error) => {
            command_pool.free_command_buffers(device, &[command_buffer]);
            return Err(error);
        }
    };
    if let Err(error) = device.submit_graphics(&[command_buffer], &[], &[], &[], fence.handle()) {
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }
    if let Err(error) = fence.wait_default().context("Timed out uploading surface texture") {
        let _ = device.wait_idle();
        command_pool.free_command_buffers(device, &[command_buffer]);
        return Err(error);
    }
    command_pool.free_command_buffers(device, &[command_buffer]);
    Ok(())
}

fn write_texture_descriptor(
    device: &Device,
    descriptor_set: vk::DescriptorSet,
    image_view: vk::ImageView,
    sampler: vk::Sampler,
) {
    let image_info = vk::DescriptorImageInfo::default()
        .image_view(image_view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    let sampler_info = vk::DescriptorImageInfo::default().sampler(sampler);
    let image_write = vk::WriteDescriptorSet::default()
        .dst_set(descriptor_set)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
        .image_info(std::slice::from_ref(&image_info));
    let sampler_write = vk::WriteDescriptorSet::default()
        .dst_set(descriptor_set)
        .dst_binding(1)
        .descriptor_type(vk::DescriptorType::SAMPLER)
        .image_info(std::slice::from_ref(&sampler_info));
    unsafe {
        device
            .handle()
            .update_descriptor_sets(&[image_write, sampler_write], &[]);
    }
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

fn upload_rect_to_vk(rect: UploadRect) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D {
            x: rect.x as i32,
            y: rect.y as i32,
        },
        extent: vk::Extent2D {
            width: rect.width,
            height: rect.height,
        },
    }
}

fn dest_intersects_clip(dest: [f32; 4], clip: &vk::Rect2D) -> bool {
    let dest_rect = DamageRect {
        x: dest[0] as i32,
        y: dest[1] as i32,
        width: dest[2].ceil() as i32,
        height: dest[3].ceil() as i32,
    };
    let clip_rect = DamageRect {
        x: clip.offset.x,
        y: clip.offset.y,
        width: clip.extent.width as i32,
        height: clip.extent.height as i32,
    };
    rects_intersect(dest_rect, clip_rect)
}

fn rects_intersect(a: DamageRect, b: DamageRect) -> bool {
    let ax1 = a.x.saturating_add(a.width);
    let ay1 = a.y.saturating_add(a.height);
    let bx1 = b.x.saturating_add(b.width);
    let by1 = b.y.saturating_add(b.height);
    a.x < bx1 && ax1 > b.x && a.y < by1 && ay1 > b.y
}

fn output_damage_to_buffer_rect(frame: &SurfaceFrame, damage: DamageRect) -> Option<UploadRect> {
    let scale = frame.buffer_scale.max(1);
    let dest_w = div_ceil_i32(frame.width as i32, scale);
    let dest_h = div_ceil_i32(frame.height as i32, scale);
    let dest = DamageRect {
        x: frame.x,
        y: frame.y,
        width: dest_w,
        height: dest_h,
    };
    let intersect = intersect_damage(damage, dest)?;
    let local_x = intersect.x.saturating_sub(frame.x);
    let local_y = intersect.y.saturating_sub(frame.y);
    Some(UploadRect {
        x: (local_x * scale) as u32,
        y: (local_y * scale) as u32,
        width: (intersect.width * scale) as u32,
        height: (intersect.height * scale) as u32,
    })
}

fn intersect_damage(a: DamageRect, b: DamageRect) -> Option<DamageRect> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = a.x.saturating_add(a.width).min(b.x.saturating_add(b.width));
    let y1 = a.y.saturating_add(a.height).min(b.y.saturating_add(b.height));
    let width = x1 - x0;
    let height = y1 - y0;
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(DamageRect {
        x: x0,
        y: y0,
        width,
        height,
    })
}

fn spv_from_bytes(bytes: &[u8]) -> Vec<u32> {
    assert!(
        bytes.len().is_multiple_of(4),
        "SPIR-V bytecode length must be a multiple of 4"
    );
    bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

struct StagingBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    device: ash::Device,
}

impl StagingBuffer {
    fn new(
        device: &Device,
        physical_device: &PhysicalDevice,
        bytes: &[u8],
    ) -> anyhow::Result<Self> {
        let size = bytes.len() as vk::DeviceSize;
        let buffer_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { device.handle().create_buffer(&buffer_info, None) }
            .context("Failed to create texture staging buffer")?;
        let requirements = unsafe { device.handle().get_buffer_memory_requirements(buffer) };
        let Some((memory_type_index, coherent)) = find_host_memory_type(
            physical_device.memory_properties(),
            requirements.memory_type_bits,
        ) else {
            unsafe {
                device.handle().destroy_buffer(buffer, None);
            }
            anyhow::bail!("No host-visible Vulkan memory for texture staging");
        };
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        let memory = match unsafe { device.handle().allocate_memory(&allocate_info, None) } {
            Ok(memory) => memory,
            Err(error) => {
                unsafe {
                    device.handle().destroy_buffer(buffer, None);
                }
                return Err(error).context("Failed to allocate texture staging memory");
            }
        };
        if let Err(error) = unsafe { device.handle().bind_buffer_memory(buffer, memory, 0) } {
            unsafe {
                device.handle().free_memory(memory, None);
                device.handle().destroy_buffer(buffer, None);
            }
            return Err(error).context("Failed to bind texture staging memory");
        };

        let mapped = match unsafe {
            device
                .handle()
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
        } {
            Ok(mapped) => mapped,
            Err(error) => {
                unsafe {
                    device.handle().free_memory(memory, None);
                    device.handle().destroy_buffer(buffer, None);
                }
                return Err(error).context("Failed to map texture staging memory");
            }
        };
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast(), bytes.len());
        }
        if !coherent {
            let range = vk::MappedMemoryRange::default()
                .memory(memory)
                .offset(0)
                .size(vk::WHOLE_SIZE);
            unsafe {
                device
                    .handle()
                    .flush_mapped_memory_ranges(&[range])
                    .context("Failed to flush texture staging memory")?;
            }
        }
        unsafe {
            device.handle().unmap_memory(memory);
        }

        Ok(Self {
            buffer,
            memory,
            device: device.handle().clone(),
        })
    }
}

impl Drop for StagingBuffer {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

fn find_host_memory_type(
    properties: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
) -> Option<(u32, bool)> {
    let mut host_visible = None;
    for index in 0..properties.memory_type_count {
        if type_bits & (1 << index) == 0 {
            continue;
        }
        let flags = properties.memory_types[index as usize].property_flags;
        if !flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
            continue;
        }
        let coherent = flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT);
        if coherent {
            return Some((index, true));
        }
        host_visible = Some((index, false));
    }
    host_visible
}
