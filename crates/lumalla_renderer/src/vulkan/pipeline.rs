//! Graphics pipeline management

use anyhow::Context;
use ash::vk;
use log::debug;

use super::{Device, RenderPass};

/// Represents a Vulkan graphics pipeline.
///
/// A graphics pipeline defines the complete rendering state, including
/// shaders, vertex input, rasterization, blending, etc.
pub struct GraphicsPipeline {
    /// The Vulkan pipeline handle
    handle: vk::Pipeline,
    /// The pipeline layout (defines descriptor sets and push constants)
    layout: vk::PipelineLayout,
    /// The device that owns this pipeline
    device: ash::Device,
}

/// Represents a compiled shader module.
pub struct ShaderModule {
    /// The Vulkan shader module handle
    handle: vk::ShaderModule,
    /// The device that owns this shader module
    device: ash::Device,
}

impl ShaderModule {
    /// Creates a shader module from SPIR-V bytecode.
    ///
    /// The bytecode should be valid SPIR-V code compiled from GLSL or HLSL.
    pub fn from_spirv(device: &Device, spirv: &[u32]) -> anyhow::Result<Self> {
        let create_info = vk::ShaderModuleCreateInfo::default().code(spirv);

        let handle = unsafe { device.handle().create_shader_module(&create_info, None) }
            .context("Failed to create shader module")?;

        Ok(Self {
            handle,
            device: device.handle().clone(),
        })
    }

    /// Returns the shader module handle.
    pub fn handle(&self) -> vk::ShaderModule {
        self.handle
    }
}

impl Drop for ShaderModule {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_shader_module(self.handle, None);
        }
        debug!("Destroyed shader module");
    }
}

/// Builder for creating graphics pipelines.
pub struct GraphicsPipelineBuilder<'a> {
    device: &'a Device,
    render_pass: &'a RenderPass,
    vertex_shader: Option<&'a ShaderModule>,
    fragment_shader: Option<&'a ShaderModule>,
    descriptor_set_layouts: Vec<vk::DescriptorSetLayout>,
    push_constant_ranges: Vec<vk::PushConstantRange>,
}

impl<'a> GraphicsPipelineBuilder<'a> {
    /// Creates a new pipeline builder.
    pub fn new(device: &'a Device, render_pass: &'a RenderPass) -> Self {
        Self {
            device,
            render_pass,
            vertex_shader: None,
            fragment_shader: None,
            descriptor_set_layouts: Vec::new(),
            push_constant_ranges: Vec::new(),
        }
    }

    /// Sets the vertex shader.
    pub fn vertex_shader(mut self, shader: &'a ShaderModule) -> Self {
        self.vertex_shader = Some(shader);
        self
    }

    /// Sets the fragment shader.
    pub fn fragment_shader(mut self, shader: &'a ShaderModule) -> Self {
        self.fragment_shader = Some(shader);
        self
    }

    /// Adds a descriptor set layout.
    pub fn descriptor_set_layout(mut self, layout: vk::DescriptorSetLayout) -> Self {
        self.descriptor_set_layouts.push(layout);
        self
    }

    /// Adds a push constant range.
    pub fn push_constant_range(mut self, range: vk::PushConstantRange) -> Self {
        self.push_constant_ranges.push(range);
        self
    }

    /// Builds the graphics pipeline.
    pub fn build(self) -> anyhow::Result<GraphicsPipeline> {
        // Create pipeline layout
        let layout_create_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&self.descriptor_set_layouts)
            .push_constant_ranges(&self.push_constant_ranges);

        let layout = unsafe { self.device.handle().create_pipeline_layout(&layout_create_info, None) }
            .context("Failed to create pipeline layout")?;

        // Build shader stage create infos
        let mut shader_stages = Vec::new();

        if let Some(vertex_shader) = self.vertex_shader {
            shader_stages.push(
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX)
                    .module(vertex_shader.handle())
                    .name(c"main"),
            );
        }

        if let Some(fragment_shader) = self.fragment_shader {
            shader_stages.push(
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(fragment_shader.handle())
                    .name(c"main"),
            );
        }

        if shader_stages.is_empty() {
            anyhow::bail!("At least one shader stage must be provided");
        }

        // Vertex input state
        // For a fullscreen quad, we'll use no vertex input (generated in shader)
        let vertex_input_state = vk::PipelineVertexInputStateCreateInfo::default();

        // Input assembly state
        let input_assembly_state = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST)
            .primitive_restart_enable(false);

        // Viewport state
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);

        // Rasterization state
        let rasterization_state = vk::PipelineRasterizationStateCreateInfo::default()
            .depth_clamp_enable(false)
            .rasterizer_discard_enable(false)
            .polygon_mode(vk::PolygonMode::FILL)
            .line_width(1.0)
            .cull_mode(vk::CullModeFlags::NONE) // Don't cull - we want to see both sides
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .depth_bias_enable(false);

        // Multisample state
        let multisample_state = vk::PipelineMultisampleStateCreateInfo::default()
            .sample_shading_enable(false)
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);

        // Color blend attachment state
        let color_blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(
                vk::ColorComponentFlags::R
                    | vk::ColorComponentFlags::G
                    | vk::ColorComponentFlags::B
                    | vk::ColorComponentFlags::A,
            )
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD);

        // Store array in variable to ensure it lives long enough
        let color_blend_attachments = [color_blend_attachment];

        // Color blend state
        let color_blend_state = vk::PipelineColorBlendStateCreateInfo::default()
            .logic_op_enable(false)
            .attachments(&color_blend_attachments);

        // Dynamic state (viewport and scissor can be set dynamically)
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state = vk::PipelineDynamicStateCreateInfo::default()
            .dynamic_states(&dynamic_states);

        // Create graphics pipeline
        let pipeline_create_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&shader_stages)
            .vertex_input_state(&vertex_input_state)
            .input_assembly_state(&input_assembly_state)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization_state)
            .multisample_state(&multisample_state)
            .color_blend_state(&color_blend_state)
            .dynamic_state(&dynamic_state)
            .layout(layout)
            .render_pass(self.render_pass.handle())
            .subpass(0);

        let result = unsafe {
            self.device
                .handle()
                .create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_create_info], None)
        };

        let pipelines = match result {
            Ok(pipelines) => pipelines,
            Err((_pipelines, err)) => {
                // Even on error, some pipelines might have been created
                // For now, we'll fail, but in the future we could handle partial success
                anyhow::bail!("Failed to create graphics pipeline: {:?}", err);
            }
        };

        if pipelines.is_empty() {
            anyhow::bail!("No pipelines were created");
        }

        let handle = pipelines[0];

        debug!("Created graphics pipeline");

        Ok(GraphicsPipeline {
            handle,
            layout,
            device: self.device.handle().clone(),
        })
    }
}

impl GraphicsPipeline {
    /// Returns the pipeline handle.
    pub fn handle(&self) -> vk::Pipeline {
        self.handle
    }

    /// Returns the pipeline layout.
    pub fn layout(&self) -> vk::PipelineLayout {
        self.layout
    }
}

impl Drop for GraphicsPipeline {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_pipeline(self.handle, None);
            self.device.destroy_pipeline_layout(self.layout, None);
        }
        debug!("Destroyed graphics pipeline");
    }
}
