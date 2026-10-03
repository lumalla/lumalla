//! GPU compositing: surface texture cache and scanout render pass.

use std::collections::HashMap;
use std::collections::HashSet;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd};
use std::rc::Rc;

use anyhow::Context;
use ash::vk;
use lumalla_shared::{BufferTransform, Guide, GuideKind, GuideLayer, View};

use crate::bitmap_font::{self, GuideLabelKey};
use crate::default_cursor::default_cursor_frame;
use crate::scene_backing::{CompositeMode, DamageRect, UploadRect, buffer_damage_to_upload_rect};
use crate::{CursorDraw, CursorFrame, DmabufAttachment, SurfaceFrame};

const WL_SHM_FORMAT_XRGB8888: u32 = 1;
const WL_SHM_FORMAT_ARGB8888: u32 = 0;

use super::{
    CommandBufferRecorder, DescriptorPool, DescriptorSetLayout, Device, DmaBufImage, Fence,
    Framebuffer, GraphicsPipeline, GraphicsPipelineBuilder, Image, RenderPass, Sampler,
    ShaderModule, StagingBuffer, VulkanContext, drm_fourcc_to_vulkan,
};

const MAX_SURFACE_TEXTURES: u32 = 320;
const CURSOR_TEXTURE_KEY: (u32, u32) = (u32::MAX, u32::MAX);
/// Synthetic owner id for guide label textures (`surface_id` = guide index).
const GUIDE_LABEL_OWNER: u32 = u32::MAX - 1;

/// Batched GPU work for one present/screencast submit: one reusable command buffer.
pub struct GpuWorkBatch {
    /// Begun primary command buffer; ended on [`Self::submit`].
    command_buffer: Option<vk::CommandBuffer>,
    staging: Vec<StagingBuffer>,
}

impl GpuWorkBatch {
    pub fn new() -> Self {
        Self {
            command_buffer: None,
            staging: Vec::new(),
        }
    }

    /// Ensure a one-time primary command buffer is begun and return its handle.
    pub fn ensure_recording(
        &mut self,
        vulkan: &mut VulkanContext,
    ) -> anyhow::Result<vk::CommandBuffer> {
        if let Some(command_buffer) = self.command_buffer {
            return Ok(command_buffer);
        }
        let command_buffer = vulkan.acquire_command_buffer()?;
        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        if let Err(error) = unsafe {
            vulkan
                .device()
                .handle()
                .begin_command_buffer(command_buffer, &begin_info)
        } {
            vulkan.release_command_buffers([command_buffer]);
            return Err(error).context("Failed to begin frame command buffer");
        }
        self.command_buffer = Some(command_buffer);
        Ok(command_buffer)
    }

    pub fn push_staging(&mut self, staging: StagingBuffer) {
        self.staging.push(staging);
    }

    /// Command buffer handle while recording (after [`Self::ensure_recording`]).
    pub fn recording_buffer(&self) -> anyhow::Result<vk::CommandBuffer> {
        self.command_buffer
            .context("GPU work batch is not recording")
    }

    pub fn is_empty(&self) -> bool {
        self.command_buffer.is_none()
    }

    /// End recording without submitting and return resources to their pools.
    pub fn abandon(&mut self, vulkan: &mut VulkanContext) {
        if let Some(command_buffer) = self.command_buffer.take() {
            let _ = unsafe { vulkan.device().handle().end_command_buffer(command_buffer) };
            vulkan.release_command_buffers([command_buffer]);
        }
        vulkan.release_staging_many(self.staging.drain(..));
    }

    /// Ends recording and submits all work with a single fence (does not wait).
    pub fn submit(mut self, vulkan: &mut VulkanContext) -> anyhow::Result<PendingGpuSubmit> {
        let Some(command_buffer) = self.command_buffer.take() else {
            vulkan.release_staging_many(self.staging.drain(..));
            return Ok(PendingGpuSubmit::empty());
        };
        if let Err(error) = unsafe { vulkan.device().handle().end_command_buffer(command_buffer) } {
            vulkan.release_command_buffers([command_buffer]);
            vulkan.release_staging_many(self.staging.drain(..));
            return Err(error).context("Failed to end frame command buffer");
        }
        let fence = match Fence::new(vulkan.device(), false) {
            Ok(fence) => fence,
            Err(error) => {
                vulkan.release_command_buffers([command_buffer]);
                vulkan.release_staging_many(self.staging.drain(..));
                return Err(error).context("Failed to create frame GPU fence");
            }
        };
        if let Err(error) =
            vulkan
                .device()
                .submit_graphics(&[command_buffer], &[], &[], &[], fence.handle())
        {
            vulkan.release_command_buffers([command_buffer]);
            vulkan.release_staging_many(self.staging.drain(..));
            return Err(error).context("Failed to submit frame GPU work");
        }
        Ok(PendingGpuSubmit {
            fence: Some(fence),
            command_buffers: vec![command_buffer],
            staging: std::mem::take(&mut self.staging),
            texture_epoch: None,
        })
    }
}

/// In-flight GPU work that must complete before a scanout buffer is flipped or reused.
pub struct PendingGpuSubmit {
    fence: Option<Fence>,
    command_buffers: Vec<vk::CommandBuffer>,
    staging: Vec<StagingBuffer>,
    /// Texture-cache epoch from [`SurfaceTextureCache::begin_submit`], if tracked.
    texture_epoch: Option<u64>,
}

impl PendingGpuSubmit {
    fn empty() -> Self {
        Self {
            fence: None,
            command_buffers: Vec::new(),
            staging: Vec::new(),
            texture_epoch: None,
        }
    }

    pub fn is_pending(&self) -> bool {
        self.fence.is_some()
    }

    /// Bind this submit to a [`SurfaceTextureCache::begin_submit`] epoch.
    pub fn with_texture_epoch(mut self, epoch: u64) -> Self {
        self.texture_epoch = Some(epoch);
        self
    }

    /// Epoch used for deferred texture retirement, if any.
    pub fn texture_epoch(&self) -> Option<u64> {
        self.texture_epoch
    }

    /// Recycle command buffers and return staging to the pool after the fence has signaled.
    fn recycle(&mut self, vulkan: &mut VulkanContext) {
        vulkan.release_command_buffers(self.command_buffers.drain(..));
        vulkan.release_staging_many(self.staging.drain(..));
        let _ = self.fence.take();
    }

    /// Blocks until GPU work finishes and recycles command buffers / staging.
    pub fn wait(mut self, vulkan: &mut VulkanContext) -> anyhow::Result<()> {
        if let Some(fence) = self.fence.take() {
            if let Err(error) = fence
                .wait_default()
                .context("Timed out waiting for GPU frame work")
            {
                let _ = vulkan.device().wait_idle();
                vulkan.release_command_buffers(self.command_buffers.drain(..));
                vulkan.release_staging_many(self.staging.drain(..));
                return Err(error);
            }
        }
        self.recycle(vulkan);
        Ok(())
    }

    /// Non-blocking completion check.
    ///
    /// Returns `Ok(None)` when work is finished (resources recycled), or
    /// `Ok(Some(self))` when the fence is still pending.
    pub fn try_complete(
        mut self,
        vulkan: &mut VulkanContext,
    ) -> anyhow::Result<Option<Self>> {
        if let Some(fence) = &self.fence {
            if !fence
                .is_signaled()
                .context("Failed to poll GPU frame fence")?
            {
                return Ok(Some(self));
            }
        }
        self.recycle(vulkan);
        Ok(None)
    }
}

impl Drop for PendingGpuSubmit {
    fn drop(&mut self) {
        // Staging buffers and the fence must outlive GPU use. Callers should
        // recycle via wait()/try_complete(); this is a safety net against UAF
        // when a ScanoutBuffer/slot is dropped with work still in flight.
        if let Some(fence) = self.fence.take() {
            if fence.wait_default().is_err() {
                let _ = unsafe { fence.device_handle().device_wait_idle() };
            }
        }
        self.staging.clear();
        // Command buffers cannot be returned without VulkanContext; leak rather
        // than free into a live pool from the wrong thread/context.
        if !self.command_buffers.is_empty() {
            std::mem::forget(std::mem::take(&mut self.command_buffers));
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct LayerPushConstants {
    dest: [f32; 4],
    src_uv: [f32; 4],
    output_size: [f32; 2],
    force_opaque: f32,
    buffer_transform: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SolidPushConstants {
    dest: [f32; 4],
    color: [f32; 4],
    output_size: [f32; 2],
    half_width: f32,
    _pad: f32,
    p0: [f32; 2],
    p1: [f32; 2],
}

pub struct GpuCompositor {
    pipeline: GraphicsPipeline,
    solid_pipeline: GraphicsPipeline,
    _vert_shader: ShaderModule,
    _frag_shader: ShaderModule,
    _solid_vert_shader: ShaderModule,
    _solid_frag_shader: ShaderModule,
    descriptor_layout: DescriptorSetLayout,
    pub(crate) descriptor_pool: DescriptorPool,
    sampler: Sampler,
    /// Used when compositing a downscaled screencast so text stays readable.
    sampler_linear: Sampler,
}

/// Restores nearest-neighbor sampling when dropped after a linear screencast composite.
pub struct LinearSampleGuard<'a> {
    device: &'a Device,
    nearest: vk::Sampler,
    rebound: Vec<(vk::DescriptorSet, vk::ImageView)>,
}

impl Drop for LinearSampleGuard<'_> {
    fn drop(&mut self) {
        for &(descriptor_set, view) in &self.rebound {
            write_texture_descriptor(self.device, descriptor_set, view, self.nearest);
        }
    }
}

impl GpuCompositor {
    pub fn new(device: &Device, render_pass: &RenderPass) -> anyhow::Result<Self> {
        let vert_spv = spv_from_bytes(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/composite.vert.spv"
        )));
        let frag_spv = spv_from_bytes(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/composite.frag.spv"
        )));
        let solid_vert_spv = spv_from_bytes(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/solid.vert.spv"
        )));
        let solid_frag_spv = spv_from_bytes(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/solid.frag.spv"
        )));

        let vert_shader = ShaderModule::from_spirv(device, &vert_spv)?;
        let frag_shader = ShaderModule::from_spirv(device, &frag_spv)?;
        let solid_vert_shader = ShaderModule::from_spirv(device, &solid_vert_spv)?;
        let solid_frag_shader = ShaderModule::from_spirv(device, &solid_frag_spv)?;
        let descriptor_layout = DescriptorSetLayout::new_texture_sampler(device)?;
        let descriptor_pool =
            DescriptorPool::new_combined_image_sampler(device, MAX_SURFACE_TEXTURES)?;
        let sampler = Sampler::new_nearest(device)?;
        let sampler_linear = Sampler::new_linear(device)?;

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

        let solid_push = vk::PushConstantRange {
            stage_flags: vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
            offset: 0,
            size: mem::size_of::<SolidPushConstants>() as u32,
        };
        let solid_pipeline = GraphicsPipelineBuilder::new(device, render_pass)
            .vertex_shader(&solid_vert_shader)
            .fragment_shader(&solid_frag_shader)
            .push_constant_range(solid_push)
            .build()?;

        Ok(Self {
            pipeline,
            solid_pipeline,
            _vert_shader: vert_shader,
            _frag_shader: frag_shader,
            _solid_vert_shader: solid_vert_shader,
            _solid_frag_shader: solid_frag_shader,
            descriptor_layout,
            descriptor_pool,
            sampler,
            sampler_linear,
        })
    }

    /// Rebind layer textures to bilinear sampling for a downscaled composite.
    /// Drop the returned guard to restore nearest sampling for scanout.
    pub fn bind_linear_sampling<'a>(
        &'a self,
        device: &'a Device,
        cache: &SurfaceTextureCache,
        layers: &[&SurfaceFrame],
        cursor: CursorDraw<'_>,
    ) -> LinearSampleGuard<'a> {
        let mut rebound: Vec<(vk::DescriptorSet, vk::ImageView)> = Vec::new();
        let mut rebind = |key: (u32, u32)| {
            if let Some(texture) = cache.texture(key) {
                let view = texture.backing.view();
                write_texture_descriptor(
                    device,
                    texture.descriptor_set,
                    view,
                    self.sampler_linear.handle(),
                );
                rebound.push((texture.descriptor_set, view));
            }
        };
        for frame in layers {
            rebind((frame.owner_id, frame.surface_id));
        }
        match cursor {
            CursorDraw::Client(frame) => rebind((frame.owner_id, frame.surface_id)),
            CursorDraw::Default => rebind(CURSOR_TEXTURE_KEY),
            CursorDraw::Hidden => {}
        }
        LinearSampleGuard {
            device,
            nearest: self.sampler.handle(),
            rebound,
        }
    }

    fn draw_layer(
        &self,
        device: &Device,
        recorder: &mut CommandBufferRecorder,
        texture: &SurfaceTexture,
        dest: [f32; 4],
        src_uv: [f32; 4],
        output_width: u32,
        output_height: u32,
        force_opaque: bool,
        buffer_transform: BufferTransform,
        clip: Option<&vk::Rect2D>,
    ) {
        recorder.bind_pipeline(&self.pipeline);
        self.set_layer_viewport(recorder, output_width, output_height, clip);
        self.draw_layer_prepared(
            device,
            recorder,
            texture.descriptor_set,
            dest,
            src_uv,
            output_width,
            output_height,
            force_opaque,
            buffer_transform,
        );
    }

    fn set_layer_viewport(
        &self,
        recorder: &mut CommandBufferRecorder,
        output_width: u32,
        output_height: u32,
        clip: Option<&vk::Rect2D>,
    ) {
        recorder.set_viewport_fullscreen(output_width, output_height);
        if let Some(clip) = clip {
            recorder.set_scissor(clip);
        } else {
            recorder.set_scissor_fullscreen(output_width, output_height);
        }
    }

    /// Draw a textured quad assuming the layer pipeline and viewport/scissor are already set.
    fn draw_layer_prepared(
        &self,
        device: &Device,
        recorder: &mut CommandBufferRecorder,
        descriptor_set: vk::DescriptorSet,
        dest: [f32; 4],
        src_uv: [f32; 4],
        output_width: u32,
        output_height: u32,
        force_opaque: bool,
        buffer_transform: BufferTransform,
    ) {
        recorder.bind_descriptor_sets(self.pipeline.layout(), 0, &[descriptor_set], &[]);
        let push = LayerPushConstants {
            dest,
            src_uv,
            output_size: [output_width as f32, output_height as f32],
            force_opaque: if force_opaque { 1.0 } else { 0.0 },
            buffer_transform: buffer_transform as u32,
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
        recorder.draw_fullscreen_quad();
    }

    fn draw_solid(
        &self,
        device: &Device,
        recorder: &mut CommandBufferRecorder,
        dest: [f32; 4],
        color: [f32; 4],
        output_width: u32,
        output_height: u32,
        half_width: f32,
        p0: [f32; 2],
        p1: [f32; 2],
        clip: Option<&vk::Rect2D>,
    ) {
        if dest[2] <= 0.0 || dest[3] <= 0.0 {
            return;
        }
        if let Some(clip) = clip {
            if !dest_intersects_clip(dest, clip) {
                return;
            }
        }
        recorder.bind_pipeline(&self.solid_pipeline);
        let push = SolidPushConstants {
            dest,
            color,
            output_size: [output_width as f32, output_height as f32],
            half_width,
            _pad: 0.0,
            p0,
            p1,
        };
        unsafe {
            device.handle().cmd_push_constants(
                recorder.command_buffer(),
                self.solid_pipeline.layout(),
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

/// Texture kept alive until the GPU submits that may still sample it complete.
struct RetiredTexture {
    texture: SurfaceTexture,
    /// Submit epochs that were in flight when this texture was retired. Safe to
    /// free once none of these epochs remain in flight.
    blocked_by: HashSet<u64>,
}

pub struct SurfaceTextureCache {
    /// Current texture bound for compositing, keyed by `(owner_id, surface_id)`.
    textures: HashMap<(u32, u32), SurfaceTexture>,
    /// Parked DMA-BUF imports retained across buffer flips, keyed by `(owner_id, buffer_id)`.
    dmabuf_by_buffer: HashMap<(u32, u32), SurfaceTexture>,
    /// Content last uploaded for each guide-label texture index.
    guide_label_uploaded: HashMap<u32, GuideLabelKey>,
    /// Textures no longer referenced by the cache, kept alive until in-flight GPU
    /// submits that may still sample them have finished.
    retired: Vec<RetiredTexture>,
    /// Epochs of GPU submits that have been installed but not yet finished.
    in_flight_epochs: HashSet<u64>,
    next_submit_epoch: u64,
}

/// Soft cap on parked DMA-BUF imports per client (beyond the currently bound ones).
const MAX_PARKED_DMABUFS_PER_CLIENT: usize = 8;

impl SurfaceTextureCache {
    pub fn new() -> Self {
        Self {
            textures: HashMap::new(),
            dmabuf_by_buffer: HashMap::new(),
            guide_label_uploaded: HashMap::new(),
            retired: Vec::new(),
            in_flight_epochs: HashSet::new(),
            next_submit_epoch: 1,
        }
    }

    pub fn clear(&mut self) {
        let blocking = self.in_flight_epochs.clone();
        for (_, tex) in self.textures.drain() {
            self.retired.push(RetiredTexture {
                texture: tex,
                blocked_by: blocking.clone(),
            });
        }
        for (_, tex) in self.dmabuf_by_buffer.drain() {
            self.retired.push(RetiredTexture {
                texture: tex,
                blocked_by: blocking.clone(),
            });
        }
        self.guide_label_uploaded.clear();
        self.in_flight_epochs.clear();
    }

    fn retire(&mut self, tex: SurfaceTexture) {
        self.retired.push(RetiredTexture {
            texture: tex,
            blocked_by: self.in_flight_epochs.clone(),
        });
    }

    /// Allocate a submit epoch and mark it in flight. Pair with [`Self::complete_submit`].
    pub fn begin_submit(&mut self) -> u64 {
        let epoch = self.next_submit_epoch;
        self.next_submit_epoch = self.next_submit_epoch.wrapping_add(1).max(1);
        self.in_flight_epochs.insert(epoch);
        epoch
    }

    /// Mark a submit finished so retired textures blocked only on it can flush.
    pub fn complete_submit(&mut self, epoch: u64) {
        self.in_flight_epochs.remove(&epoch);
    }

    /// Drop tracking for epochs that are no longer referenced by any live pending submit.
    ///
    /// Handles the rare case where a [`PendingGpuSubmit`] is dropped without
    /// [`Self::complete_submit`] (epoch would otherwise pin retired textures forever).
    pub fn retain_in_flight(&mut self, live_epochs: &HashSet<u64>) {
        self.in_flight_epochs
            .retain(|epoch| live_epochs.contains(epoch));
    }

    /// Whether any textures are waiting to be freed after GPU work completes.
    pub fn has_retired(&self) -> bool {
        !self.retired.is_empty()
    }

    /// Free descriptor sets and drop retired textures whose blocking submits finished.
    pub fn flush_retired(
        &mut self,
        device: &Device,
        pool: &DescriptorPool,
    ) -> anyhow::Result<()> {
        let mut i = 0;
        while i < self.retired.len() {
            let unblocked = self.retired[i]
                .blocked_by
                .iter()
                .all(|epoch| !self.in_flight_epochs.contains(epoch));
            if unblocked {
                let retired = self.retired.swap_remove(i);
                pool.free_set(device, retired.texture.descriptor_set)?;
            } else {
                i += 1;
            }
        }
        Ok(())
    }

    /// Drop retired textures without touching the descriptor pool (device gone).
    pub fn forget_retired(&mut self) {
        self.retired.clear();
        self.in_flight_epochs.clear();
    }

    pub fn remove(&mut self, key: (u32, u32)) {
        let Some(tex) = self.textures.remove(&key) else {
            return;
        };
        if matches!(tex.backing, TextureBacking::Dmabuf(_)) {
            let buf_key = (key.0, tex.buffer_id);
            self.dmabuf_by_buffer.insert(buf_key, tex);
        } else {
            // SHM images may still be sampled by an in-flight submit.
            self.retire(tex);
        }
    }

    pub fn remove_client(&mut self, owner_id: u32) {
        let texture_keys: Vec<(u32, u32)> = self
            .textures
            .keys()
            .copied()
            .filter(|(owner, _)| *owner == owner_id)
            .collect();
        for key in texture_keys {
            if let Some(tex) = self.textures.remove(&key) {
                self.retire(tex);
            }
        }
        let buffer_keys: Vec<(u32, u32)> = self
            .dmabuf_by_buffer
            .keys()
            .copied()
            .filter(|(owner, _)| *owner == owner_id)
            .collect();
        for key in buffer_keys {
            if let Some(tex) = self.dmabuf_by_buffer.remove(&key) {
                self.retire(tex);
            }
        }
    }

    /// Drop a parked or currently-bound DMA-BUF import when the `wl_buffer` is destroyed.
    pub fn remove_dmabuf_buffer(
        &mut self,
        _device: &Device,
        _pool: &DescriptorPool,
        owner_id: u32,
        buffer_id: u32,
    ) -> anyhow::Result<()> {
        let buf_key = (owner_id, buffer_id);
        if let Some(tex) = self.dmabuf_by_buffer.remove(&buf_key) {
            self.retire(tex);
        }
        let doomed: Vec<(u32, u32)> = self
            .textures
            .iter()
            .filter_map(|(&key, tex)| {
                if key.0 == owner_id
                    && tex.buffer_id == buffer_id
                    && matches!(tex.backing, TextureBacking::Dmabuf(_))
                {
                    Some(key)
                } else {
                    None
                }
            })
            .collect();
        for key in doomed {
            if let Some(tex) = self.textures.remove(&key) {
                self.retire(tex);
            }
        }
        Ok(())
    }

    /// Drop cached DMA-BUF entries without freeing descriptor sets (Vulkan already gone).
    pub fn forget_dmabuf_buffer(&mut self, owner_id: u32, buffer_id: u32) {
        if let Some(tex) = self.dmabuf_by_buffer.remove(&(owner_id, buffer_id)) {
            self.retire(tex);
        }
        let doomed: Vec<(u32, u32)> = self
            .textures
            .iter()
            .filter_map(|(&key, tex)| {
                if key.0 == owner_id
                    && tex.buffer_id == buffer_id
                    && matches!(tex.backing, TextureBacking::Dmabuf(_))
                {
                    Some(key)
                } else {
                    None
                }
            })
            .collect();
        for key in doomed {
            if let Some(tex) = self.textures.remove(&key) {
                self.retire(tex);
            }
        }
    }

    fn texture(&self, key: (u32, u32)) -> Option<&SurfaceTexture> {
        self.textures.get(&key)
    }

    fn replace_texture(
        &mut self,
        _device: &Device,
        _pool: &DescriptorPool,
        key: (u32, u32),
        texture: SurfaceTexture,
    ) -> anyhow::Result<()> {
        if let Some(old) = self.textures.remove(&key) {
            match old.backing {
                TextureBacking::Dmabuf(_) => {
                    let buf_key = (key.0, old.buffer_id);
                    if let Some(dup) = self.dmabuf_by_buffer.insert(buf_key, old) {
                        self.retire(dup);
                    }
                    self.trim_parked_dmabufs(key.0);
                }
                TextureBacking::Shm(_) => {
                    self.retire(old);
                }
            }
        }
        self.textures.insert(key, texture);
        Ok(())
    }

    fn trim_parked_dmabufs(&mut self, owner_id: u32) {
        let parked: Vec<(u32, u32)> = self
            .dmabuf_by_buffer
            .keys()
            .copied()
            .filter(|(owner, _)| *owner == owner_id)
            .collect();
        let excess = parked.len().saturating_sub(MAX_PARKED_DMABUFS_PER_CLIENT);
        for buf_key in parked.into_iter().take(excess) {
            if let Some(tex) = self.dmabuf_by_buffer.remove(&buf_key) {
                self.retire(tex);
            }
        }
    }

    fn dmabuf_params_match(tex: &SurfaceTexture, frame: &SurfaceFrame, dmabuf: &DmabufAttachment) -> bool {
        tex.wl_format == frame.format
            && tex.dmabuf_modifier == dmabuf.modifier
            && tex.dmabuf_stride == frame.stride as u32
            && tex.dmabuf_offset == dmabuf.offset
            && tex.dmabuf_width == frame.width as u32
            && tex.dmabuf_height == frame.height as u32
    }

    fn sync_cursor(
        &mut self,
        vulkan: &mut VulkanContext,
        compositor: &GpuCompositor,
        batch: &mut GpuWorkBatch,
        cursor: CursorDraw<'_>,
    ) -> anyhow::Result<()> {
        match cursor {
            CursorDraw::Client(frame) => {
                let key = (frame.owner_id, frame.surface_id);
                if let Some(dmabuf) = frame.dmabuf.as_ref() {
                    let surface = cursor_surface_view(frame);
                    self.sync_dmabuf(vulkan, compositor, batch, key, &surface, dmabuf)
                } else {
                    self.sync_shm_pixels(
                        vulkan,
                        compositor,
                        batch,
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
            CursorDraw::Default => {
                let default = default_cursor_frame();
                self.sync_shm_pixels(
                    vulkan,
                    compositor,
                    batch,
                    CURSOR_TEXTURE_KEY,
                    default.buffer_id,
                    &default.pixels,
                    default.width as u32,
                    default.height as u32,
                    default.stride as u32,
                    default.format,
                )
            }
            CursorDraw::Hidden => Ok(()),
        }
    }

    fn sync_frame(
        &mut self,
        vulkan: &mut VulkanContext,
        compositor: &GpuCompositor,
        batch: &mut GpuWorkBatch,
        frame: &SurfaceFrame,
        force_full_texture: bool,
        surface_buffer_damage: Option<DamageRect>,
        pending_output_damage: &[DamageRect],
    ) -> anyhow::Result<()> {
        let key = (frame.owner_id, frame.surface_id);
        if let Some(dmabuf) = frame.dmabuf.as_ref() {
            self.sync_dmabuf(vulkan, compositor, batch, key, frame, dmabuf)
        } else {
            self.sync_shm_frame(
                vulkan,
                compositor,
                batch,
                frame,
                force_full_texture,
                surface_buffer_damage,
                pending_output_damage,
            )
        }
    }

    fn sync_shm_frame(
        &mut self,
        vulkan: &mut VulkanContext,
        compositor: &GpuCompositor,
        batch: &mut GpuWorkBatch,
        frame: &SurfaceFrame,
        force_full_texture: bool,
        surface_buffer_damage: Option<DamageRect>,
        pending_output_damage: &[DamageRect],
    ) -> anyhow::Result<()> {
        let key = (frame.owner_id, frame.surface_id);
        let width = frame.width as u32;
        let height = frame.height as u32;
        let stride = frame.stride as u32;
        let needs_create = self.textures.get(&key).is_none_or(|tex| {
            !matches!(tex.backing, TextureBacking::Shm(_))
                || tex
                    .extent()
                    .is_none_or(|extent| extent.width != width || extent.height != height)
        });
        let buffer_id_changed = self
            .textures
            .get(&key)
            .is_some_and(|tex| tex.buffer_id != frame.buffer_id);

        if needs_create {
            let image = vulkan.create_sampled_image(width, height)?;
            let descriptor_set = compositor
                .descriptor_pool
                .allocate_sampler_set(vulkan.device(), &compositor.descriptor_layout)?;
            self.replace_texture(
                vulkan.device(),
                &compositor.descriptor_pool,
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
            )?;
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

        let upload_regions =
            if !force_full_texture && !needs_create && !buffer_id_changed && !frame.full_surface {
                collect_shm_upload_regions(
                    frame,
                    surface_buffer_damage,
                    pending_output_damage,
                    width,
                    height,
                )
            } else {
                None
            };

        if upload_regions.is_none() {
            upload_bgra_texture(
                vulkan,
                batch,
                image,
                &frame.pixels,
                width,
                height,
                stride,
                texture.uploaded,
                None,
            )?;
        } else {
            for region in upload_regions.unwrap_or_default() {
                upload_bgra_texture(
                    vulkan,
                    batch,
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
        batch: &mut GpuWorkBatch,
        key: (u32, u32),
        frame: &SurfaceFrame,
        dmabuf: &DmabufAttachment,
    ) -> anyhow::Result<()> {
        let width = frame.width as u32;
        let height = frame.height as u32;
        let stride = frame.stride as u32;
        let buf_key = (key.0, dmabuf.buffer_id);

        let currently_bound = self.textures.get(&key).is_some_and(|tex| {
            matches!(tex.backing, TextureBacking::Dmabuf(_))
                && tex.buffer_id == dmabuf.buffer_id
                && Self::dmabuf_params_match(tex, frame, dmabuf)
        });
        if currently_bound {
            let image = {
                let tex = self
                    .textures
                    .get(&key)
                    .context("Missing reused DMA-BUF texture")?;
                match &tex.backing {
                    TextureBacking::Dmabuf(image) => image.image(),
                    TextureBacking::Shm(_) => anyhow::bail!("Expected DMA-BUF backing"),
                }
            };
            acquire_dmabuf_for_sample(vulkan, batch, image, false)?;
            return Ok(());
        }

        if let Some(cached) = self.dmabuf_by_buffer.remove(&buf_key) {
            if Self::dmabuf_params_match(&cached, frame, dmabuf) {
                self.replace_texture(
                    vulkan.device(),
                    &compositor.descriptor_pool,
                    key,
                    cached,
                )?;
                let image = {
                    let tex = self
                        .textures
                        .get(&key)
                        .context("Missing restored DMA-BUF texture")?;
                    match &tex.backing {
                        TextureBacking::Dmabuf(image) => image.image(),
                        TextureBacking::Shm(_) => anyhow::bail!("Expected DMA-BUF backing"),
                    }
                };
                acquire_dmabuf_for_sample(vulkan, batch, image, false)?;
                return Ok(());
            }
            self.retire(cached);
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
        acquire_dmabuf_for_sample(vulkan, batch, imported.image(), true)?;

        let descriptor_set = compositor
            .descriptor_pool
            .allocate_sampler_set(vulkan.device(), &compositor.descriptor_layout)?;
        write_texture_descriptor(
            vulkan.device(),
            descriptor_set,
            imported.view(),
            compositor.sampler.handle(),
        );

        self.replace_texture(
            vulkan.device(),
            &compositor.descriptor_pool,
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
        )?;
        Ok(())
    }

    fn sync_shm_pixels(
        &mut self,
        vulkan: &mut VulkanContext,
        compositor: &GpuCompositor,
        batch: &mut GpuWorkBatch,
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
                || tex
                    .extent()
                    .is_none_or(|extent| extent.width != width || extent.height != height)
        });

        // Same SHM buffer already on the GPU — skip staging upload (e.g. pointer moves).
        if !needs_create
            && self.textures.get(&key).is_some_and(|tex| {
                tex.uploaded && tex.buffer_id == buffer_id && tex.wl_format == wl_format
            })
        {
            return Ok(());
        }

        if needs_create {
            let image = vulkan.create_sampled_image(width, height)?;
            let descriptor_set = compositor
                .descriptor_pool
                .allocate_sampler_set(vulkan.device(), &compositor.descriptor_layout)?;
            self.replace_texture(
                vulkan.device(),
                &compositor.descriptor_pool,
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
            )?;
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
            vulkan,
            batch,
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

    /// Upload or replace a guide label texture keyed by guide list index.
    ///
    /// Skips the GPU upload when `content_key` matches the last successful upload
    /// for this index and the texture still exists at the expected size.
    pub fn sync_guide_label(
        &mut self,
        vulkan: &mut VulkanContext,
        compositor: &GpuCompositor,
        batch: &mut GpuWorkBatch,
        guide_index: u32,
        content_key: &GuideLabelKey,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) -> anyhow::Result<()> {
        let key = (GUIDE_LABEL_OWNER, guide_index);
        if self.guide_label_uploaded.get(&guide_index) == Some(content_key)
            && self.textures.get(&key).is_some_and(|tex| {
                matches!(tex.backing, TextureBacking::Shm(_))
                    && tex.extent().is_some_and(|extent| {
                        extent.width == width && extent.height == height
                    })
            })
        {
            return Ok(());
        }

        self.sync_shm_pixels(
            vulkan,
            compositor,
            batch,
            key,
            guide_index.wrapping_add(1),
            pixels,
            width,
            height,
            width.saturating_mul(4),
            WL_SHM_FORMAT_ARGB8888,
        )?;
        self.guide_label_uploaded
            .insert(guide_index, content_key.clone());
        Ok(())
    }

    /// Drop guide-label textures (and upload tracking) for indices ≥ `live_count`.
    pub fn prune_guide_labels(
        &mut self,
        _device: &Device,
        _pool: &DescriptorPool,
        live_count: u32,
    ) -> anyhow::Result<()> {
        let doomed: Vec<(u32, u32)> = self
            .textures
            .keys()
            .copied()
            .filter(|(owner, index)| *owner == GUIDE_LABEL_OWNER && *index >= live_count)
            .collect();
        for key in doomed {
            if let Some(tex) = self.textures.remove(&key) {
                self.retire(tex);
            }
            self.guide_label_uploaded.remove(&key.1);
        }
        self.guide_label_uploaded
            .retain(|index, _| *index < live_count);
        Ok(())
    }

    pub fn sync_scene(
        &mut self,
        vulkan: &mut VulkanContext,
        compositor: &GpuCompositor,
        batch: &mut GpuWorkBatch,
        layers: &[&SurfaceFrame],
        cursor: CursorDraw<'_>,
        composite_mode: &CompositeMode,
        dirty_surfaces: &HashSet<(u32, u32)>,
        surface_buffer_damage: &HashMap<(u32, u32), DamageRect>,
        pending_output_damage: &[DamageRect],
        sync_cursor: bool,
    ) -> anyhow::Result<()> {
        let sync_all = matches!(composite_mode, CompositeMode::Full);
        for frame in layers {
            let key = (frame.owner_id, frame.surface_id);
            if sync_all || dirty_surfaces.contains(&key) {
                let buffer_damage = surface_buffer_damage.get(&key).copied();
                self.sync_frame(
                    vulkan,
                    compositor,
                    batch,
                    frame,
                    sync_all,
                    buffer_damage,
                    pending_output_damage,
                )?;
            }
        }
        if sync_all || sync_cursor {
            self.sync_cursor(vulkan, compositor, batch, cursor)?;
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
    let scale = cursor.buffer_scale.max(1);
    let transform = BufferTransform::from_raw(cursor.buffer_transform).unwrap_or_default();
    let (width, height) = transform.transformed_size(cursor.width, cursor.height);
    let surface_width = div_ceil_i32(width as i32, scale);
    let surface_height = div_ceil_i32(height as i32, scale);
    SurfaceFrame {
        owner_id: cursor.owner_id,
        surface_id: cursor.surface_id,
        buffer_id: cursor.buffer_id,
        pixels: Rc::new(Vec::new()),
        width: cursor.width,
        height: cursor.height,
        stride: cursor.stride,
        format: cursor.format,
        x: 0,
        y: 0,
        buffer_scale: cursor.buffer_scale,
        buffer_transform: cursor.buffer_transform,
        surface_width,
        surface_height,
        viewport_src: None,
        dmabuf: None,
        damage: None,
        buffer_damage: None,
        full_surface: true,
    }
}

fn collect_shm_upload_regions(
    frame: &SurfaceFrame,
    surface_buffer_damage: Option<DamageRect>,
    pending_output_damage: &[DamageRect],
    buffer_width: u32,
    buffer_height: u32,
) -> Option<Vec<UploadRect>> {
    let mut regions = Vec::new();
    if let Some(rect) = surface_buffer_damage {
        if let Some(upload) = buffer_damage_to_upload_rect(rect, buffer_width, buffer_height) {
            regions.push(upload);
        }
    }
    for rect in pending_output_damage {
        if let Some(upload) = output_damage_to_buffer_rect(frame, *rect) {
            regions.push(upload);
        }
    }
    if regions.is_empty() {
        None
    } else {
        Some(regions)
    }
}

fn dup_fd(fd: std::os::fd::RawFd) -> anyhow::Result<std::os::fd::OwnedFd> {
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!(
            "Failed to duplicate DMA-BUF fd={fd}: errno={} ({err})",
            err.raw_os_error().unwrap_or(0)
        );
    }
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(dup) })
}

fn acquire_dmabuf_for_sample(
    vulkan: &mut VulkanContext,
    batch: &mut GpuWorkBatch,
    image: vk::Image,
    first_import: bool,
) -> anyhow::Result<()> {
    let command_buffer = batch.ensure_recording(vulkan)?;
    let device = vulkan.device();
    let graphics_family = device.graphics_queue_family();
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
            command_buffer,
            src_stage,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );
    }
    Ok(())
}

/// Copies a scanout image into a back buffer before incremental compositing.
///
/// Both images stay in `GENERAL`. The source is often the live KMS front buffer;
/// transitioning it to `TRANSFER_SRC_OPTIMAL` glitches undamaged tiles for a frame
/// on some laptop GPUs. Vulkan 1.2 allows `vkCmdCopyImage` with `GENERAL`.
pub fn copy_scanout_frame(
    vulkan: &mut VulkanContext,
    batch: &mut GpuWorkBatch,
    src: &DmaBufImage,
    dst: &DmaBufImage,
    dst_was_fresh: bool,
) -> anyhow::Result<()> {
    let command_buffer = batch.ensure_recording(vulkan)?;
    let device = vulkan.device();
    let extent = src.extent();

    let dst_old_layout = if dst_was_fresh {
        vk::ImageLayout::UNDEFINED
    } else {
        vk::ImageLayout::GENERAL
    };

    // Memory barriers only — keep src in GENERAL so a displayed FB is not retiled.
    let src_barrier = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
        .old_layout(vk::ImageLayout::GENERAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(src.image())
        .subresource_range(color_subresource_range());
    let dst_src_access = if dst_was_fresh {
        vk::AccessFlags::empty()
    } else {
        vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE
    };
    let dst_barrier = vk::ImageMemoryBarrier::default()
        .src_access_mask(dst_src_access)
        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .old_layout(dst_old_layout)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(dst.image())
        .subresource_range(color_subresource_range());
    unsafe {
        device.handle().cmd_pipeline_barrier(
            command_buffer,
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
            command_buffer,
            src.image(),
            vk::ImageLayout::GENERAL,
            dst.image(),
            vk::ImageLayout::GENERAL,
            &[copy_region],
        );
    }

    let src_back = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::TRANSFER_READ)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .old_layout(vk::ImageLayout::GENERAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(src.image())
        .subresource_range(color_subresource_range());
    let dst_back = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .old_layout(vk::ImageLayout::GENERAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(dst.image())
        .subresource_range(color_subresource_range());
    unsafe {
        device.handle().cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[src_back, dst_back],
        );
    }
    Ok(())
}

pub fn composite_to_scanout(
    vulkan: &VulkanContext,
    batch: &mut GpuWorkBatch,
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
    views: &[View],
    layers: &[&SurfaceFrame],
    below_layers: &[&SurfaceFrame],
    above_layers: &[&SurfaceFrame],
    guides: &[Guide],
    cursor: CursorDraw<'_>,
    pointer_x: i32,
    pointer_y: i32,
) -> anyhow::Result<()> {
    let clear_value = vk::ClearValue {
        color: vk::ClearColorValue {
            float32: clear_color,
        },
    };

    let command_buffer = batch.recording_buffer()?;
    let device = vulkan.device();
    let mut recorder = CommandBufferRecorder::continue_recording(device, command_buffer);
    transition_scanout_for_render(
        device,
        recorder.command_buffer(),
        scanout_image,
        scanout_old_layout,
    )?;
    recorder.begin_render_pass(render_pass, framebuffer, &[clear_value])?;
    let draw_list = build_layer_draw_list(cache, layers);
    let below_list = build_layer_draw_list(cache, below_layers);
    let above_list = build_layer_draw_list(cache, above_layers);

    match composite_mode {
        CompositeMode::Full => {
            draw_views(
                compositor,
                device,
                &mut recorder,
                cache,
                views,
                &draw_list,
                &below_list,
                &above_list,
                guides,
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
                draw_views(
                    compositor,
                    device,
                    &mut recorder,
                    cache,
                    views,
                    &draw_list,
                    &below_list,
                    &above_list,
                    guides,
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
    Ok(())
}

/// Composite scene layers into an arbitrary DMA image (e.g. screencast buffer).
///
/// `cursor` / `pointer_*` are in destination (buffer) pixel space. Leaves the
/// image in `GENERAL`.
///
/// When `linear_filter` is true, textures are temporarily rebound to bilinear
/// sampling (for downscaled MemFd captures) and restored afterward.
pub fn composite_layers_to_image(
    vulkan: &VulkanContext,
    batch: &mut GpuWorkBatch,
    compositor: &GpuCompositor,
    cache: &SurfaceTextureCache,
    render_pass: &RenderPass,
    image: &DmaBufImage,
    framebuffer: &Framebuffer,
    image_old_layout: vk::ImageLayout,
    output_width: u32,
    output_height: u32,
    clear_color: [f32; 4],
    view: &View,
    layers: &[&SurfaceFrame],
    cursor: CursorDraw<'_>,
    pointer_x: i32,
    pointer_y: i32,
    linear_filter: bool,
) -> anyhow::Result<()> {
    let device = vulkan.device();
    let linear_guard = if linear_filter {
        Some(compositor.bind_linear_sampling(device, cache, layers, cursor))
    } else {
        None
    };

    let clear_value = vk::ClearValue {
        color: vk::ClearColorValue {
            float32: clear_color,
        },
    };

    let command_buffer = batch.recording_buffer()?;
    let mut recorder = CommandBufferRecorder::continue_recording(device, command_buffer);
    transition_scanout_for_render(
        device,
        recorder.command_buffer(),
        image,
        image_old_layout,
    )?;
    recorder.begin_render_pass(render_pass, framebuffer, &[clear_value])?;
    let draw_list = build_layer_draw_list(cache, layers);
    draw_scene_layers(
        compositor,
        device,
        &mut recorder,
        view,
        &draw_list,
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
    recorder.end_render_pass();

    // Transition to GENERAL for PipeWire export / CPU readback.
    let barrier = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image.image())
        .subresource_range(color_subresource_range());
    unsafe {
        device.handle().cmd_pipeline_barrier(
            recorder.command_buffer(),
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );
    }
    drop(linear_guard);
    Ok(())
}

/// Draw the cursor onto an existing screencast image without clearing it.
///
/// `pointer_*` are in destination (buffer) pixel space. Image must be in
/// `GENERAL` (typical after a blit). Leaves the image in `GENERAL`.
pub fn overlay_cursor_on_image(
    vulkan: &VulkanContext,
    batch: &mut GpuWorkBatch,
    compositor: &GpuCompositor,
    cache: &SurfaceTextureCache,
    render_pass_load: &RenderPass,
    image: &DmaBufImage,
    framebuffer: &Framebuffer,
    output_width: u32,
    output_height: u32,
    cursor: CursorDraw<'_>,
    pointer_x: i32,
    pointer_y: i32,
) -> anyhow::Result<()> {
    if matches!(cursor, CursorDraw::Hidden) {
        return Ok(());
    }

    let command_buffer = batch.recording_buffer()?;
    let device = vulkan.device();
    let mut recorder = CommandBufferRecorder::continue_recording(device, command_buffer);
    transition_scanout_for_render(
        device,
        recorder.command_buffer(),
        image,
        vk::ImageLayout::GENERAL,
    )?;
    // LOAD render pass — preserve the blit contents.
    recorder.begin_render_pass_default(render_pass_load, framebuffer)?;
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
    recorder.end_render_pass();

    let barrier = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image.image())
        .subresource_range(color_subresource_range());
    unsafe {
        device.handle().cmd_pipeline_barrier(
            recorder.command_buffer(),
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );
    }
    Ok(())
}

/// Compact per-frame draw record: texture handle + scene geometry without SHM pixels.
#[derive(Clone, Copy)]
struct LayerDrawItem {
    descriptor_set: vk::DescriptorSet,
    /// Destination rect in scene/compositor space (before view mapping).
    scene_dest: [f32; 4],
    src_uv: [f32; 4],
    force_opaque: bool,
    buffer_transform: BufferTransform,
}

fn build_layer_draw_list(
    cache: &SurfaceTextureCache,
    layers: &[&SurfaceFrame],
) -> Vec<LayerDrawItem> {
    let mut items = Vec::with_capacity(layers.len());
    for frame in layers {
        let key = (frame.owner_id, frame.surface_id);
        let Some(texture) = cache.texture(key) else {
            continue;
        };
        let scene_dest = surface_dest_rect(frame);
        if scene_dest[2] <= 0.0 || scene_dest[3] <= 0.0 {
            continue;
        }
        items.push(LayerDrawItem {
            descriptor_set: texture.descriptor_set,
            scene_dest,
            src_uv: surface_src_uv(frame),
            force_opaque: frame.format == WL_SHM_FORMAT_XRGB8888,
            buffer_transform: BufferTransform::from_raw(frame.buffer_transform)
                .unwrap_or_default(),
        });
    }
    items
}

fn draw_views(
    compositor: &GpuCompositor,
    device: &Device,
    recorder: &mut CommandBufferRecorder<'_>,
    cache: &SurfaceTextureCache,
    views: &[View],
    layers: &[LayerDrawItem],
    below_layers: &[LayerDrawItem],
    above_layers: &[LayerDrawItem],
    guides: &[Guide],
    cursor: CursorDraw<'_>,
    pointer_x: i32,
    pointer_y: i32,
    output_width: u32,
    output_height: u32,
    outer_clip: Option<&vk::Rect2D>,
) {
    // Output-local shell chrome below the desktop views (background + bottom).
    draw_output_local_layers(
        compositor,
        device,
        recorder,
        below_layers,
        output_width,
        output_height,
        outer_clip,
    );
    for view in views {
        let Some(view_clip) = view_dest_clip(view, output_width, output_height) else {
            continue;
        };
        let clip = match outer_clip {
            Some(outer) => match intersect_vk_rects(outer, &view_clip) {
                Some(combined) => combined,
                None => continue,
            },
            None => view_clip,
        };
        draw_guides(
            compositor,
            device,
            recorder,
            cache,
            view,
            guides,
            GuideLayer::Below,
            output_width,
            output_height,
            Some(&clip),
        );
        draw_scene_layers(
            compositor,
            device,
            recorder,
            view,
            layers,
            output_width,
            output_height,
            Some(&clip),
        );
        draw_guides(
            compositor,
            device,
            recorder,
            cache,
            view,
            guides,
            GuideLayer::Above,
            output_width,
            output_height,
            Some(&clip),
        );
    }
    // Output-local shell chrome above the desktop views (top + overlay).
    draw_output_local_layers(
        compositor,
        device,
        recorder,
        above_layers,
        output_width,
        output_height,
        outer_clip,
    );
    // Cursor is monitor/dest-native: draw once in output pixels, never through a view.
    draw_cursor_layer(
        compositor,
        device,
        recorder,
        cache,
        cursor,
        pointer_x,
        pointer_y,
        output_width,
        output_height,
        outer_clip,
    );
}

fn draw_output_local_layers(
    compositor: &GpuCompositor,
    device: &Device,
    recorder: &mut CommandBufferRecorder<'_>,
    layers: &[LayerDrawItem],
    output_width: u32,
    output_height: u32,
    clip: Option<&vk::Rect2D>,
) {
    if layers.is_empty() {
        return;
    }
    recorder.bind_pipeline(&compositor.pipeline);
    compositor.set_layer_viewport(recorder, output_width, output_height, clip);
    for item in layers {
        let dest = item.scene_dest;
        if dest[2] <= 0.0 || dest[3] <= 0.0 {
            continue;
        }
        if let Some(clip) = clip
            && !dest_intersects_clip(dest, clip)
        {
            continue;
        }
        compositor.draw_layer_prepared(
            device,
            recorder,
            item.descriptor_set,
            dest,
            item.src_uv,
            output_width,
            output_height,
            item.force_opaque,
            item.buffer_transform,
        );
    }
}

fn draw_guides(
    compositor: &GpuCompositor,
    device: &Device,
    recorder: &mut CommandBufferRecorder<'_>,
    cache: &SurfaceTextureCache,
    view: &View,
    guides: &[Guide],
    layer: GuideLayer,
    output_width: u32,
    output_height: u32,
    clip: Option<&vk::Rect2D>,
) {
    for (index, guide) in guides.iter().enumerate() {
        if guide.layer != layer {
            continue;
        }
        draw_guide_geometry(
            compositor,
            device,
            recorder,
            view,
            guide,
            output_width,
            output_height,
            clip,
        );
        if !guide.label.is_empty() {
            draw_guide_label(
                compositor,
                device,
                recorder,
                cache,
                view,
                guide,
                index as u32,
                output_width,
                output_height,
                clip,
            );
        }
    }
}

fn draw_guide_geometry(
    compositor: &GpuCompositor,
    device: &Device,
    recorder: &mut CommandBufferRecorder<'_>,
    view: &View,
    guide: &Guide,
    output_width: u32,
    output_height: u32,
    clip: Option<&vk::Rect2D>,
) {
    let color = guide.color.premultiplied_f32();
    let stroke = guide.stroke.max(1) as f32;
    match &guide.kind {
        GuideKind::Box {
            x,
            y,
            width,
            height,
        } => {
            if let Some(fill) = guide.fill {
                let dest = map_rect_through_view(
                    view,
                    [*x as f32, *y as f32, *width as f32, *height as f32],
                );
                compositor.draw_solid(
                    device,
                    recorder,
                    dest,
                    fill.premultiplied_f32(),
                    output_width,
                    output_height,
                    -1.0,
                    [0.0, 0.0],
                    [0.0, 0.0],
                    clip,
                );
            }
            let w = *width as f32;
            let h = *height as f32;
            let sx = *x as f32;
            let sy = *y as f32;
            // Top, bottom, left, right edges in scene space.
            let edges = [
                [sx, sy, w, stroke],
                [sx, sy + h - stroke, w, stroke],
                [sx, sy, stroke, h],
                [sx + w - stroke, sy, stroke, h],
            ];
            for edge in edges {
                if edge[2] <= 0.0 || edge[3] <= 0.0 {
                    continue;
                }
                let dest = map_rect_through_view(view, edge);
                compositor.draw_solid(
                    device,
                    recorder,
                    dest,
                    color,
                    output_width,
                    output_height,
                    -1.0,
                    [0.0, 0.0],
                    [0.0, 0.0],
                    clip,
                );
            }
        }
        GuideKind::Line { x1, y1, x2, y2 } => {
            let p0 = map_point_through_view(view, *x1 as f32, *y1 as f32);
            let p1 = map_point_through_view(view, *x2 as f32, *y2 as f32);
            let half = scale_length_through_view(view, stroke) * 0.5;
            let min_x = p0[0].min(p1[0]) - half - 1.0;
            let min_y = p0[1].min(p1[1]) - half - 1.0;
            let max_x = p0[0].max(p1[0]) + half + 1.0;
            let max_y = p0[1].max(p1[1]) + half + 1.0;
            let dest = [min_x, min_y, (max_x - min_x).max(1.0), (max_y - min_y).max(1.0)];
            compositor.draw_solid(
                device,
                recorder,
                dest,
                color,
                output_width,
                output_height,
                half.max(0.5),
                p0,
                p1,
                clip,
            );
        }
    }
}

fn draw_guide_label(
    compositor: &GpuCompositor,
    device: &Device,
    recorder: &mut CommandBufferRecorder<'_>,
    cache: &SurfaceTextureCache,
    view: &View,
    guide: &Guide,
    guide_index: u32,
    output_width: u32,
    output_height: u32,
    clip: Option<&vk::Rect2D>,
) {
    let key = (GUIDE_LABEL_OWNER, guide_index);
    let Some(texture) = cache.texture(key) else {
        return;
    };
    let (lw, lh) = bitmap_font::label_size(&guide.label);
    let (lx, ly) = label_scene_origin(guide, lw as i32, lh as i32);
    let dest = map_rect_through_view(view, [lx as f32, ly as f32, lw as f32, lh as f32]);
    if dest[2] <= 0.0 || dest[3] <= 0.0 {
        return;
    }
    if let Some(clip) = clip {
        if !dest_intersects_clip(dest, clip) {
            return;
        }
    }
    compositor.draw_layer(
        device,
        recorder,
        texture,
        dest,
        [0.0, 0.0, 1.0, 1.0],
        output_width,
        output_height,
        false,
        BufferTransform::Normal,
        clip,
    );
}

fn label_scene_origin(guide: &Guide, label_w: i32, label_h: i32) -> (i32, i32) {
    match &guide.kind {
        GuideKind::Box { x, y, .. } => (*x, *y - label_h - 2),
        GuideKind::Line { x1, y1, x2, y2 } => {
            let mx = (*x1 + *x2) / 2;
            let my = (*y1 + *y2) / 2;
            (mx - label_w / 2, my - label_h - 2)
        }
    }
}

fn map_point_through_view(view: &View, x: f32, y: f32) -> [f32; 2] {
    let rect = map_rect_through_view(view, [x, y, 1.0, 1.0]);
    [rect[0], rect[1]]
}

fn scale_length_through_view(view: &View, length: f32) -> f32 {
    let (sx, _, sw, _) = view.source;
    let (_, _, dw, _) = view.dest;
    let _ = sx;
    if sw <= 0 {
        return length;
    }
    length * (dw as f32 / sw as f32)
}

fn draw_scene_layers(
    compositor: &GpuCompositor,
    device: &Device,
    recorder: &mut CommandBufferRecorder<'_>,
    view: &View,
    layers: &[LayerDrawItem],
    output_width: u32,
    output_height: u32,
    clip: Option<&vk::Rect2D>,
) {
    if layers.is_empty() {
        return;
    }
    recorder.bind_pipeline(&compositor.pipeline);
    compositor.set_layer_viewport(recorder, output_width, output_height, clip);
    for item in layers {
        let dest = map_rect_through_view(view, item.scene_dest);
        if dest[2] <= 0.0 || dest[3] <= 0.0 {
            continue;
        }
        if let Some(clip) = clip {
            if !dest_intersects_clip(dest, clip) {
                continue;
            }
        }
        compositor.draw_layer_prepared(
            device,
            recorder,
            item.descriptor_set,
            dest,
            item.src_uv,
            output_width,
            output_height,
            item.force_opaque,
            item.buffer_transform,
        );
    }
}

fn draw_cursor_layer(
    compositor: &GpuCompositor,
    device: &Device,
    recorder: &mut CommandBufferRecorder<'_>,
    cache: &SurfaceTextureCache,
    cursor: CursorDraw<'_>,
    pointer_x: i32,
    pointer_y: i32,
    output_width: u32,
    output_height: u32,
    clip: Option<&vk::Rect2D>,
) {
    let default_owned;
    let (cursor_key, cursor_frame) = match cursor {
        CursorDraw::Client(frame) => ((frame.owner_id, frame.surface_id), frame),
        CursorDraw::Default => {
            default_owned = default_cursor_frame();
            (CURSOR_TEXTURE_KEY, &default_owned)
        }
        CursorDraw::Hidden => return,
    };
    let Some(texture) = cache.texture(cursor_key) else {
        return;
    };
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
            [0.0, 0.0, 1.0, 1.0],
            output_width,
            output_height,
            cursor_frame.format == WL_SHM_FORMAT_XRGB8888,
            BufferTransform::from_raw(cursor_frame.buffer_transform).unwrap_or_default(),
            clip,
        );
    }
}

/// Map a rectangle from global compositor space through a view into output-local pixels.
pub fn map_rect_through_view(view: &View, rect: [f32; 4]) -> [f32; 4] {
    let (sx, sy, sw, sh) = view.source;
    let (dx, dy, dw, dh) = view.dest;
    if sw <= 0 || sh <= 0 {
        return [0.0, 0.0, 0.0, 0.0];
    }
    let scale_x = dw as f32 / sw as f32;
    let scale_y = dh as f32 / sh as f32;
    [
        dx as f32 + (rect[0] - sx as f32) * scale_x,
        dy as f32 + (rect[1] - sy as f32) * scale_y,
        rect[2] * scale_x,
        rect[3] * scale_y,
    ]
}

fn view_dest_clip(view: &View, output_width: u32, output_height: u32) -> Option<vk::Rect2D> {
    let (dx, dy, dw, dh) = view.dest;
    if dw <= 0 || dh <= 0 {
        return None;
    }
    let x0 = dx.max(0) as u32;
    let y0 = dy.max(0) as u32;
    let x1 = (dx + dw).max(0) as u32;
    let y1 = (dy + dh).max(0) as u32;
    let x0 = x0.min(output_width);
    let y0 = y0.min(output_height);
    let x1 = x1.min(output_width);
    let y1 = y1.min(output_height);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D {
            x: x0 as i32,
            y: y0 as i32,
        },
        extent: vk::Extent2D {
            width: x1 - x0,
            height: y1 - y0,
        },
    })
}

fn intersect_vk_rects(a: &vk::Rect2D, b: &vk::Rect2D) -> Option<vk::Rect2D> {
    let ax0 = a.offset.x;
    let ay0 = a.offset.y;
    let ax1 = a.offset.x + a.extent.width as i32;
    let ay1 = a.offset.y + a.extent.height as i32;
    let bx0 = b.offset.x;
    let by0 = b.offset.y;
    let bx1 = b.offset.x + b.extent.width as i32;
    let by1 = b.offset.y + b.extent.height as i32;
    let x0 = ax0.max(bx0);
    let y0 = ay0.max(by0);
    let x1 = ax1.min(bx1);
    let y1 = ay1.min(by1);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        },
    })
}

fn surface_dest_rect(frame: &SurfaceFrame) -> [f32; 4] {
    [
        frame.x as f32,
        frame.y as f32,
        frame.surface_width.max(0) as f32,
        frame.surface_height.max(0) as f32,
    ]
}

fn surface_src_uv(frame: &SurfaceFrame) -> [f32; 4] {
    let transform = BufferTransform::from_raw(frame.buffer_transform).unwrap_or_default();
    let (width, height) = transform.transformed_size(frame.width, frame.height);
    let [x, y, source_width, source_height] = transform.transformed_source_rect(
        frame.width,
        frame.height,
        frame.buffer_scale,
        frame.viewport_src,
    );
    let width = width.max(1) as f64;
    let height = height.max(1) as f64;
    [
        (x / width) as f32,
        (y / height) as f32,
        ((x + source_width) / width) as f32,
        ((y + source_height) / height) as f32,
    ]
}

fn cursor_dest_rect(cursor: &CursorFrame, pointer_x: i32, pointer_y: i32) -> [f32; 4] {
    let transform = BufferTransform::from_raw(cursor.buffer_transform).unwrap_or_default();
    let (width, height) = transform.transformed_size(cursor.width, cursor.height);
    let dest_w = div_ceil_i32(width as i32, cursor.buffer_scale.max(1)) as f32;
    let dest_h = div_ceil_i32(height as i32, cursor.buffer_scale.max(1)) as f32;
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

    // LOAD_OP_LOAD (partial damage) reads undamaged tiles; CLEAR only needs write.
    let dst_access = if old_layout == vk::ImageLayout::UNDEFINED {
        vk::AccessFlags::COLOR_ATTACHMENT_WRITE
    } else {
        vk::AccessFlags::COLOR_ATTACHMENT_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE
    };

    let barrier = vk::ImageMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
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
    vulkan: &mut VulkanContext,
    batch: &mut GpuWorkBatch,
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

    let (staging, copy_stride, copy_height, image_offset, image_extent) = match region {
        Some(region) => {
            let region_row_bytes = usize::try_from(region.width)
                .context("Region width overflows")?
                .checked_mul(4)
                .context("Region row bytes overflow")?;
            let region_size = region_row_bytes
                .checked_mul(region.height as usize)
                .context("Region size overflows")?;
            let staging = vulkan.acquire_staging(region_size as vk::DeviceSize)?;
            if let Err(error) =
                staging.write_packed_region(vulkan.device(), pixels, stride, region)
            {
                vulkan.release_staging_many([staging]);
                return Err(error);
            }
            (
                staging,
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
        None => {
            let staging = vulkan.acquire_staging(full_size as vk::DeviceSize)?;
            if let Err(error) = staging.write_slice(vulkan.device(), &pixels[..full_size]) {
                vulkan.release_staging_many([staging]);
                return Err(error);
            }
            (
                staging,
                width,
                height,
                vk::Offset3D { x: 0, y: 0, z: 0 },
                vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                },
            )
        }
    };

    let command_buffer = match batch.ensure_recording(vulkan) {
        Ok(command_buffer) => command_buffer,
        Err(error) => {
            vulkan.release_staging_many([staging]);
            return Err(error);
        }
    };

    if let Err(error) = record_bgra_texture_upload(
        vulkan,
        image,
        &staging,
        previously_uploaded,
        copy_stride,
        copy_height,
        image_offset,
        image_extent,
        command_buffer,
    ) {
        vulkan.release_staging_many([staging]);
        batch.abandon(vulkan);
        return Err(error);
    }
    batch.push_staging(staging);
    Ok(())
}

fn record_bgra_texture_upload(
    vulkan: &VulkanContext,
    image: &Image,
    staging: &StagingBuffer,
    previously_uploaded: bool,
    copy_stride: u32,
    copy_height: u32,
    image_offset: vk::Offset3D,
    image_extent: vk::Extent3D,
    command_buffer: vk::CommandBuffer,
) -> anyhow::Result<()> {
    let device = vulkan.device();
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
            command_buffer,
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
            command_buffer,
            staging.buffer(),
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
            command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_sample],
        );
    }
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

/// Copies (with optional scale) a rectangular region from `src` into `dst`.
///
/// Downscales with linear filtering so MemFd screencast previews stay readable;
/// 1:1 and upscales keep nearest to avoid softening crisp content.
pub fn blit_image_region(
    vulkan: &mut VulkanContext,
    batch: &mut GpuWorkBatch,
    src: &DmaBufImage,
    dst: &DmaBufImage,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
    dst_x: u32,
    dst_y: u32,
    dst_w: u32,
    dst_h: u32,
    dst_was_undefined: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(src_w > 0 && src_h > 0 && dst_w > 0 && dst_h > 0, "blit sizes must be positive");
    let src_extent = src.extent();
    let dst_extent = dst.extent();
    anyhow::ensure!(
        src_x.saturating_add(src_w) <= src_extent.width
            && src_y.saturating_add(src_h) <= src_extent.height,
        "source blit rect out of bounds"
    );
    anyhow::ensure!(
        dst_x.saturating_add(dst_w) <= dst_extent.width
            && dst_y.saturating_add(dst_h) <= dst_extent.height,
        "destination blit rect out of bounds"
    );

    let command_buffer = batch.ensure_recording(vulkan)?;
    let device = vulkan.device();
    let dst_old_layout = if dst_was_undefined {
        vk::ImageLayout::UNDEFINED
    } else {
        vk::ImageLayout::GENERAL
    };

    let src_barrier = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
        .old_layout(vk::ImageLayout::GENERAL)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(src.image())
        .subresource_range(color_subresource_range());
    let dst_src_access = if dst_was_undefined {
        vk::AccessFlags::empty()
    } else {
        vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE
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
            command_buffer,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[src_barrier, dst_barrier],
        );
    }

    let regions = [vk::ImageBlit::default()
        .src_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        })
        .src_offsets([
            vk::Offset3D {
                x: src_x as i32,
                y: src_y as i32,
                z: 0,
            },
            vk::Offset3D {
                x: (src_x + src_w) as i32,
                y: (src_y + src_h) as i32,
                z: 1,
            },
        ])
        .dst_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        })
        .dst_offsets([
            vk::Offset3D {
                x: dst_x as i32,
                y: dst_y as i32,
                z: 0,
            },
            vk::Offset3D {
                x: (dst_x + dst_w) as i32,
                y: (dst_y + dst_h) as i32,
                z: 1,
            },
        ])];
    // Nearest on downscale picks single source pixels and makes text unreadable.
    let filter = if dst_w < src_w || dst_h < src_h {
        vk::Filter::LINEAR
    } else {
        vk::Filter::NEAREST
    };
    unsafe {
        device.handle().cmd_blit_image(
            command_buffer,
            src.image(),
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            dst.image(),
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &regions,
            filter,
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
            command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[src_back, dst_back],
        );
    }
    Ok(())
}

/// Clear a DMA image to `color` (records into `batch`, does not wait).
///
/// Used before partial/multi-region screencast blits so uncovered destination
/// pixels are defined. Leaves the image in `GENERAL`.
pub fn clear_dma_image_color(
    vulkan: &mut VulkanContext,
    batch: &mut GpuWorkBatch,
    image: &DmaBufImage,
    was_undefined: bool,
    color: [f32; 4],
) -> anyhow::Result<()> {
    let command_buffer = batch.ensure_recording(vulkan)?;
    let device = vulkan.device();
    let old_layout = if was_undefined {
        vk::ImageLayout::UNDEFINED
    } else {
        vk::ImageLayout::GENERAL
    };
    let src_access = if was_undefined {
        vk::AccessFlags::empty()
    } else {
        vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE
    };
    let to_dst = vk::ImageMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image.image())
        .subresource_range(color_subresource_range());
    unsafe {
        device.handle().cmd_pipeline_barrier(
            command_buffer,
            if was_undefined {
                vk::PipelineStageFlags::TOP_OF_PIPE
            } else {
                vk::PipelineStageFlags::ALL_COMMANDS
            },
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_dst],
        );
    }

    let clear = vk::ClearColorValue { float32: color };
    unsafe {
        device.handle().cmd_clear_color_image(
            command_buffer,
            image.image(),
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &clear,
            &[color_subresource_range()],
        );
    }

    let to_general = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image.image())
        .subresource_range(color_subresource_range());
    unsafe {
        device.handle().cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_general],
        );
    }
    Ok(())
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
    let transform = BufferTransform::from_raw(frame.buffer_transform)?;
    let dest = DamageRect {
        x: frame.x,
        y: frame.y,
        width: frame.surface_width.max(0),
        height: frame.surface_height.max(0),
    };
    let intersect = intersect_damage(damage, dest)?;
    let local_x = intersect.x.saturating_sub(frame.x);
    let local_y = intersect.y.saturating_sub(frame.y);
    // Map surface-local damage into the viewport source region in buffer pixels.
    let (src_x, src_y, src_w, src_h) = match frame.viewport_src {
        Some((sx, sy, sw, sh)) => (
            (sx * scale as f32) as i32,
            (sy * scale as f32) as i32,
            (sw * scale as f32).ceil() as i32,
            (sh * scale as f32).ceil() as i32,
        ),
        None => {
            let (width, height) = transform.transformed_size(frame.width, frame.height);
            (0, 0, width as i32, height as i32)
        }
    };
    let dest_w = frame.surface_width.max(1);
    let dest_h = frame.surface_height.max(1);
    let tx0 = src_x + local_x.saturating_mul(src_w) / dest_w;
    let ty0 = src_y + local_y.saturating_mul(src_h) / dest_h;
    let tx1 = src_x
        + div_ceil_i32(
            local_x
                .saturating_add(intersect.width)
                .saturating_mul(src_w),
            dest_w,
        );
    let ty1 = src_y
        + div_ceil_i32(
            local_y
                .saturating_add(intersect.height)
                .saturating_mul(src_h),
            dest_h,
        );
    let tx = tx0.max(0) as u32;
    let ty = ty0.max(0) as u32;
    let tw = tx1.saturating_sub(tx0).max(1) as u32;
    let th = ty1.saturating_sub(ty0).max(1) as u32;
    let (x, y, width, height) = transform.transformed_rect_to_buffer(
        tx,
        ty,
        tw,
        th,
        frame.width as u32,
        frame.height as u32,
    );
    buffer_damage_to_upload_rect(
        DamageRect {
            x: x as i32,
            y: y as i32,
            width: width as i32,
            height: height as i32,
        },
        frame.width as u32,
        frame.height as u32,
    )
}

fn intersect_damage(a: DamageRect, b: DamageRect) -> Option<DamageRect> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = a.x.saturating_add(a.width).min(b.x.saturating_add(b.width));
    let y1 =
        a.y.saturating_add(a.height)
            .min(b.y.saturating_add(b.height));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_damage_maps_through_all_buffer_transforms() {
        for transform in BufferTransform::ALL {
            let (surface_width, surface_height) = transform.transformed_size(4, 3);
            let frame = SurfaceFrame {
                owner_id: 1,
                surface_id: 2,
                buffer_id: 3,
                pixels: Rc::new(vec![0; 4 * 3 * 4]),
                width: 4,
                height: 3,
                stride: 16,
                format: 0,
                x: 10,
                y: 20,
                buffer_scale: 1,
                buffer_transform: transform as u32,
                surface_width: surface_width as i32,
                surface_height: surface_height as i32,
                viewport_src: None,
                dmabuf: None,
                damage: None,
                buffer_damage: None,
                full_surface: false,
            };
            let upload = output_damage_to_buffer_rect(
                &frame,
                DamageRect {
                    x: 10,
                    y: 20,
                    width: 1,
                    height: 1,
                },
            )
            .unwrap();
            let (expected_x, expected_y) = transform.transformed_to_buffer(0, 0, 4, 3);
            assert_eq!(
                upload,
                UploadRect {
                    x: expected_x as u32,
                    y: expected_y as u32,
                    width: 1,
                    height: 1,
                },
                "{transform:?}"
            );
        }
    }
}
