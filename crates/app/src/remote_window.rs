//! A window shared by the other device, shown as a native window here.
//!
//! Its pixels live in a GPU texture that is updated in place: only the tiles
//! that changed are uploaded. The window is sized so one of its pixels is
//! one screen pixel, and the texture is sampled with nearest filtering, so
//! text stays exactly as sharp as on the other device.
//!
//! Mouse, scroll and keyboard input on the window is translated to the
//! shared window's own pixels and sent back.

use std::{collections::HashMap, sync::Arc};

use dextr_core::window::Surface;
use dextr_proto::{Button, Input};
use iced::{
    Rectangle, keyboard, mouse,
    wgpu,
    widget::shader::{self, Action, Viewport},
};

use crate::{Message, keymap};

/// Draws one shared window and forwards its input.
pub struct RemoteWindow {
    pub id: u32,
    pub surface: Arc<Surface>,
}

#[derive(Default)]
pub struct InputState {
    /// Buttons pressed inside the window, so their release is sent even if
    /// the pointer left it meanwhile.
    held: Vec<mouse::Button>,
}

fn button(b: mouse::Button) -> Button {
    match b {
        mouse::Button::Left => Button::Left,
        mouse::Button::Right => Button::Right,
        mouse::Button::Middle => Button::Middle,
        mouse::Button::Back => Button::Back,
        mouse::Button::Forward => Button::Forward,
        mouse::Button::Other(n) => Button::Other(n),
    }
}

impl shader::Program<Message> for RemoteWindow {
    type State = InputState;
    type Primitive = WindowPrimitive;

    fn update(&self, state: &mut InputState, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<Action<Message>> {
        let (w, h) = self.surface.size();
        if w == 0 || bounds.width <= 0.0 || bounds.height <= 0.0 {
            return None;
        }
        // Window points → shared-window pixels.
        let (sx, sy) = (w as f32 / bounds.width, h as f32 / bounds.height);
        let at = |p: iced::Point| ((p.x - bounds.x) * sx, (p.y - bounds.y) * sy);
        let send = |input: Input| Some(Action::publish(Message::RemoteInput(self.id, input)).and_capture());

        match event {
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let p = cursor.position_in(bounds).map(|p| iced::Point::new(p.x + bounds.x, p.y + bounds.y));
                let p = p.or_else(|| (!state.held.is_empty()).then(|| cursor.position()).flatten())?;
                let (x, y) = at(p);
                send(Input::Motion { x: x.clamp(0.0, w as f32 - 1.0), y: y.clamp(0.0, h as f32 - 1.0) })
            }
            iced::Event::Mouse(mouse::Event::ButtonPressed(b)) => {
                let p = cursor.position_over(bounds)?;
                state.held.push(*b);
                let (x, y) = at(p);
                send(Input::Button { button: button(*b), pressed: true, x, y })
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(b)) => {
                let i = state.held.iter().position(|h| h == b)?;
                state.held.remove(i);
                let (x, y) = cursor.position().map(at).unwrap_or_default();
                send(Input::Button { button: button(*b), pressed: false, x: x.clamp(0.0, w as f32 - 1.0), y: y.clamp(0.0, h as f32 - 1.0) })
            }
            iced::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                cursor.position_over(bounds)?;
                // iced: positive is up/left; the protocol: positive is down/right.
                match *delta {
                    mouse::ScrollDelta::Lines { x, y } => send(Input::ScrollDiscrete { dx: (-x * 120.0) as i32, dy: (-y * 120.0) as i32 }),
                    mouse::ScrollDelta::Pixels { x, y } => send(Input::Scroll { dx: -x * sx, dy: -y * sy }),
                }
            }
            iced::Event::Keyboard(keyboard::Event::KeyPressed { physical_key, repeat: false, .. }) => {
                send(Input::Key { key: keymap::to_hid(physical_key)?, pressed: true })
            }
            iced::Event::Keyboard(keyboard::Event::KeyReleased { physical_key, .. }) => {
                send(Input::Key { key: keymap::to_hid(physical_key)?, pressed: false })
            }
            _ => None,
        }
    }

    fn draw(&self, _state: &InputState, _cursor: mouse::Cursor, _bounds: Rectangle) -> WindowPrimitive {
        WindowPrimitive { id: self.id, surface: self.surface.clone() }
    }
}

#[derive(Debug)]
pub struct WindowPrimitive {
    id: u32,
    surface: Arc<Surface>,
}

struct Texture {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    size: (u32, u32),
    last_used: u64,
}

/// GPU state shared by all shared windows.
pub struct Pipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    format: wgpu::TextureFormat,
    textures: HashMap<u32, Texture>,
    frame: u64,
}

const SHADER: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    var corners = array<vec2<f32>, 6>(
        vec2(-1.0, -1.0), vec2(1.0, -1.0), vec2(-1.0, 1.0),
        vec2(-1.0, 1.0), vec2(1.0, -1.0), vec2(1.0, 1.0),
    );
    let xy = corners[i];
    var out: Out;
    out.pos = vec4(xy, 0.0, 1.0);
    out.uv = vec2((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
    return out;
}

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    // Windows are opaque; captured alpha may be undefined (BGRx).
    return vec4(textureSample(tex, samp, in.uv).rgb, 1.0);
}
"#;

impl shader::Pipeline for Pipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, target: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("dextr shared window"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("dextr shared window"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("dextr shared window"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("dextr shared window"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        // Nearest: one texel per screen pixel, no smoothing.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("dextr shared window"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        // Sampling an sRGB texture into an sRGB target round-trips exactly.
        let format = if target.is_srgb() { wgpu::TextureFormat::Bgra8UnormSrgb } else { wgpu::TextureFormat::Bgra8Unorm };
        Self { pipeline, layout, sampler, format, textures: HashMap::new(), frame: 0 }
    }

    fn trim(&mut self) {
        // Drop textures of windows not drawn for a while; they're rebuilt
        // from the surface if drawn again.
        self.frame += 1;
        let frame = self.frame;
        self.textures.retain(|_, t| frame - t.last_used < 600);
    }
}

impl shader::Primitive for WindowPrimitive {
    type Pipeline = Pipeline;

    fn prepare(&self, p: &mut Pipeline, device: &wgpu::Device, queue: &wgpu::Queue, _bounds: &Rectangle, _viewport: &Viewport) {
        let mut d = self.surface.lock();
        let (w, h) = (d.width, d.height);
        if w == 0 || h == 0 {
            return;
        }
        let stale = p.textures.get(&self.id).is_none_or(|t| t.size != (w, h));
        if stale {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("dextr shared window"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: p.format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("dextr shared window"),
                layout: &p.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&p.sampler) },
                ],
            });
            p.textures.insert(self.id, Texture { texture, bind_group, size: (w, h), last_used: p.frame });
        }
        let t = p.textures.get_mut(&self.id).unwrap();
        t.last_used = p.frame;

        let row = w * 4;
        let upload = |x: u32, y: u32, uw: u32, uh: u32| {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &t.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &d.pixels,
                wgpu::TexelCopyBufferLayout { offset: (y * row + x * 4) as u64, bytes_per_row: Some(row), rows_per_image: Some(h) },
                wgpu::Extent3d { width: uw, height: uh, depth_or_array_layers: 1 },
            );
        };
        if stale || d.full {
            upload(0, 0, w, h);
        } else {
            for r in &d.dirty {
                upload(r.x, r.y, r.w, r.h);
            }
        }
        d.dirty.clear();
        d.full = false;
    }

    fn draw(&self, p: &Pipeline, pass: &mut wgpu::RenderPass<'_>) -> bool {
        if let Some(t) = p.textures.get(&self.id) {
            pass.set_pipeline(&p.pipeline);
            pass.set_bind_group(0, &t.bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
        true
    }
}
