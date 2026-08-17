use slash0_core::node::{Node, NodeIdx};
use slash0_core::slab::{SlabRead, VecSlab};
use slash0_core::thin::ThinData;
use slash0_core::timestamp::Timestamp;
use slash0_core::uniforms::RenderUniforms;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wgpu::{
    BindGroup, BindGroupDescriptor, BindGroupLayout, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingType, Buffer, BufferBindingType, BufferDescriptor, BufferSize,
    BufferUsages, Device, Queue, RenderPipeline, ShaderStages, Surface,
};

const NODE_SIZE: u64 = size_of::<Node<ThinData>>() as u64;
const UNIFORMS_SIZE: u64 = size_of::<RenderUniforms>() as u64;

pub struct RenderState {
    device: Device,
    queue: Queue,
    surface: Surface<'static>,
    pipeline: RenderPipeline,
    bind_group: BindGroup,
    slab_buffer: Buffer,
    uniform_buffer: Buffer,
}

impl RenderState {
    pub async fn init(canvas_id: &str, shader_name: &str) -> Result<Self, JsValue> {
        let canvas = get_canvas(canvas_id)?;
        let width = canvas.width().max(1);
        let height = canvas.height().max(1);

        let instance = create_instance();
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let adapter = request_adapter(&instance, &surface).await?;
        let (device, queue) = request_device(&adapter).await?;
        let format = configure_surface(&surface, &adapter, &device, width, height);

        let shader = create_shader(&device);
        let slab_buffer = create_storage_buffer(&device, "slab-buffer", slab_size_bytes(&device));
        let uniform_buffer = create_storage_buffer(&device, "render-uniforms", UNIFORMS_SIZE);

        let bind_group_layout = create_bind_group_layout(&device);
        let bind_group =
            create_bind_group(&device, &bind_group_layout, &slab_buffer, &uniform_buffer);
        let pipeline = create_pipeline(&device, &bind_group_layout, &shader, format, shader_name);

        Ok(RenderState {
            device,
            queue,
            surface,
            pipeline,
            bind_group,
            slab_buffer,
            uniform_buffer,
        })
    }

    /// Upload the entire slab to the GPU in a single write. Index-aligned: slab
    /// slot `i` lands at byte `i * NODE_SIZE`, matching how the shader indexes
    /// it. Used to seed the buffer from a freshly downloaded snapshot.
    pub fn upload_slab(&mut self, slab: &VecSlab<Node<ThinData>>) {
        self.queue
            .write_buffer(&self.slab_buffer, 0, bytemuck::cast_slice(slab.as_slice()));
    }

    /// Flush the given touched slab nodes to the GPU buffer. The writes are
    /// staged by wgpu and applied at the next `render` submit.
    pub fn update(&mut self, slab: &VecSlab<Node<ThinData>>, dirty: &[NodeIdx]) {
        for &idx in dirty {
            let offset = idx.get() as u64 * NODE_SIZE;
            self.queue
                .write_buffer(&self.slab_buffer, offset, bytemuck::bytes_of(slab.get(idx)));
        }
    }

    /// Draw one frame from the current slab contents.
    pub fn render(&mut self, root: Option<NodeIdx>, now: Timestamp) -> Result<(), JsValue> {
        let uniforms = RenderUniforms { root, now };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            other => {
                return Err(JsValue::from_str(&format!(
                    "surface texture unavailable: {other:?}",
                )));
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: "slab-pass".into(),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rpass.set_pipeline(&self.pipeline);
            rpass.set_bind_group(0, &self.bind_group, &[]);
            rpass.draw(0..3, 0..1);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(frame);
        Ok(())
    }
}

fn get_canvas(canvas_id: &str) -> Result<web_sys::HtmlCanvasElement, JsValue> {
    web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(canvas_id)
        .ok_or_else(|| JsValue::from_str("canvas element not found"))?
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .map_err(Into::into)
}

fn create_instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::BROWSER_WEBGPU,
        flags: wgpu::InstanceFlags::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        backend_options: wgpu::BackendOptions::default(),
        display: None,
    })
}

async fn request_adapter(
    instance: &wgpu::Instance,
    surface: &Surface<'static>,
) -> Result<wgpu::Adapter, JsValue> {
    instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        })
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

async fn request_device(adapter: &wgpu::Adapter) -> Result<(Device, Queue), JsValue> {
    adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("slash0-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        })
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Configures the surface for the canvas size and returns the chosen texture
/// format (needed later to build the pipeline's color target).
fn configure_surface(
    surface: &Surface<'static>,
    adapter: &wgpu::Adapter,
    device: &Device,
    width: u32,
    height: u32,
) -> wgpu::TextureFormat {
    let caps = surface.get_capabilities(adapter);
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .unwrap_or(caps.formats[0]);
    surface.configure(
        device,
        &wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width,
            height,
            present_mode: caps.present_modes[0],
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        },
    );
    format
}

fn create_shader(device: &Device) -> wgpu::ShaderModule {
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: "slash0-shader".into(),
        source: wgpu::ShaderSource::Wgsl(include_str!(env!("SLASH0_SHADER_WGSL")).into()),
    })
}

/// Size the slab at the device's storage-binding ceiling (~128 MiB by default),
/// node-aligned, so the trie never has to grow the buffer.
fn slab_size_bytes(device: &Device) -> u64 {
    let capacity_bytes = device.limits().max_storage_buffer_binding_size;
    (capacity_bytes / NODE_SIZE) * NODE_SIZE
}

fn create_storage_buffer(device: &Device, label: &str, size: u64) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some(label),
        size,
        usage: BufferUsages::COPY_DST | BufferUsages::STORAGE,
        mapped_at_creation: false,
    })
}

fn create_bind_group_layout(device: &Device) -> BindGroupLayout {
    let storage_entry = |binding, min_size| BindGroupLayoutEntry {
        binding,
        visibility: ShaderStages::FRAGMENT,
        ty: BindingType::Buffer {
            ty: BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: BufferSize::new(min_size),
        },
        count: None,
    };
    device.create_bind_group_layout(&BindGroupLayoutDescriptor {
        label: "slab-bind-group-layout".into(),
        entries: &[storage_entry(0, NODE_SIZE), storage_entry(1, UNIFORMS_SIZE)],
    })
}

fn create_bind_group(
    device: &Device,
    layout: &BindGroupLayout,
    slab_buffer: &Buffer,
    uniform_buffer: &Buffer,
) -> BindGroup {
    device.create_bind_group(&BindGroupDescriptor {
        label: "slab-bind-group".into(),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: slab_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: uniform_buffer.as_entire_binding(),
            },
        ],
    })
}

fn create_pipeline(
    device: &Device,
    bind_group_layout: &BindGroupLayout,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    shader_name: &str,
) -> RenderPipeline {
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: "layout".into(),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: "render-pipeline".into(),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(shader_name),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}
