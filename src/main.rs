use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{render, RenderSettings};
use vello::kurbo::{Affine, Rect};
use vello::peniko::{Blob, Color, Fill, ImageAlphaType, ImageBrush, ImageData, ImageFormat};
use vello::util::{RenderContext, RenderSurface};
use vello::wgpu::{self, SurfaceError};
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Icon, Window, WindowAttributes};

const BASE_ZOOM: f32 = 0.92;
const ZOOM_STEP: f32 = 1.10;
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 8.0;
const MAX_TEXTURE_SIZE: u32 = 8192;
const RASTER_OVERSAMPLE: f32 = 1.2;
const RERASTERIZE_THRESHOLD: f32 = 0.12;
const INTERACTION_SETTLE_DELAY: Duration = Duration::from_millis(120);

struct CachedPageImage {
    page_index: usize,
    target_width: u32,
    target_height: u32,
    image: ImageBrush,
}

struct RenderState {
    window: Arc<Window>,
    surface: RenderSurface<'static>,
    valid_surface: bool,
}

struct App {
    pdf: Pdf,
    page_sizes: Vec<(f32, f32)>,
    interpreter_settings: InterpreterSettings,

    context: RenderContext,
    renderer: Vec<Option<Renderer>>,
    render_state: Option<RenderState>,

    scene: Scene,
    cached_page: Option<CachedPageImage>,

    page_index: usize,
    zoom: f32,
    pan: (f32, f32),
    dragging: bool,
    last_cursor: Option<(f64, f64)>,
    pending_high_quality_render: bool,
    last_view_change: Instant,
}

impl App {
    fn new(pdf: Pdf, page_sizes: Vec<(f32, f32)>) -> Self {
        Self {
            pdf,
            page_sizes,
            interpreter_settings: InterpreterSettings::default(),
            context: RenderContext::new(),
            renderer: Vec::new(),
            render_state: None,
            scene: Scene::new(),
            cached_page: None,
            page_index: 0,
            zoom: 1.0,
            pan: (0.0, 0.0),
            dragging: false,
            last_cursor: None,
            pending_high_quality_render: true,
            last_view_change: Instant::now(),
        }
    }

    fn update_title(&self, window: &Window) {
        window.set_title(&format!(
            "OxideReader · Page {}/{} · {:.0}%",
            self.page_index + 1,
            self.page_sizes.len(),
            BASE_ZOOM * self.zoom * 100.0
        ));
    }

    fn invalidate_cached_page(&mut self) {
        self.cached_page = None;
        self.pending_high_quality_render = true;
    }

    fn mark_view_changed(&mut self) {
        self.pending_high_quality_render = true;
        self.last_view_change = Instant::now();
    }

    fn current_display_size(&self, width: u32, height: u32) -> (f32, f32) {
        let viewport_w = width.max(1) as f32;
        let viewport_h = height.max(1) as f32;
        let (page_w, page_h) = self.page_sizes[self.page_index];
        let page_aspect = (page_w / page_h).max(0.01);

        let fit_w = viewport_w * 0.9;
        let fit_h = viewport_h * 0.9;
        let display_w = fit_w.min(fit_h * page_aspect) * self.zoom;
        let display_h = display_w / page_aspect;

        (display_w.max(1.0), display_h.max(1.0))
    }

    fn target_texture_size(&self, display_w: f32, display_h: f32) -> (u32, u32) {
        let mut target_w = display_w * RASTER_OVERSAMPLE;
        let mut target_h = display_h * RASTER_OVERSAMPLE;
        if target_h > MAX_TEXTURE_SIZE as f32 {
            let scale = MAX_TEXTURE_SIZE as f32 / target_h;
            target_h *= scale;
            target_w *= scale;
        }
        if target_w > MAX_TEXTURE_SIZE as f32 {
            let scale = MAX_TEXTURE_SIZE as f32 / target_w;
            target_w *= scale;
            target_h *= scale;
        }

        (
            target_w.round().clamp(32.0, MAX_TEXTURE_SIZE as f32) as u32,
            target_h.round().clamp(32.0, MAX_TEXTURE_SIZE as f32) as u32,
        )
    }

    fn ensure_page_image(&mut self, width: u32, height: u32) {
        let (display_width, display_height) = self.current_display_size(width, height);
        let (target_width, target_height) = self.target_texture_size(display_width, display_height);
        let settled = self.last_view_change.elapsed() >= INTERACTION_SETTLE_DELAY;

        if let Some(cached) = &self.cached_page {
            if cached.page_index == self.page_index {
                let width_delta = 1.0 - (target_width as f32 / cached.target_width.max(1) as f32);
                let height_delta =
                    1.0 - (target_height as f32 / cached.target_height.max(1) as f32);
                let close_enough =
                    width_delta.abs().max(height_delta.abs()) <= RERASTERIZE_THRESHOLD;

                if close_enough {
                    if settled {
                        self.pending_high_quality_render = false;
                    }
                    return;
                }

                if !settled {
                    return;
                }
            }
        }

        if !self.pending_high_quality_render {
            return;
        }

        let pages = self.pdf.pages();
        let page = &pages[self.page_index];
        let (page_w, page_h) = self.page_sizes[self.page_index];
        let render_settings = RenderSettings {
            x_scale: target_width as f32 / page_w,
            y_scale: target_height as f32 / page_h,
            width: Some(u16::try_from(target_width).expect("Page render width must fit in u16")),
            height: Some(u16::try_from(target_height).expect("Page render height must fit in u16")),
            bg_color: WHITE,
        };

        let pixmap = render(
            page,
            &hayro::RenderCache::new(),
            &self.interpreter_settings,
            &render_settings,
        );

        let rgba = Arc::new(pixmap.data_as_u8_slice().to_vec());
        let image_data = ImageData {
            data: Blob::new(rgba),
            format: ImageFormat::Rgba8,
            width: target_width,
            height: target_height,
            alpha_type: ImageAlphaType::Alpha,
        };

        self.cached_page = Some(CachedPageImage {
            page_index: self.page_index,
            target_width,
            target_height,
            image: image_data.into(),
        });
        self.pending_high_quality_render = false;
    }

    fn draw_frame(&mut self) {
        let Some((width, height, valid_surface, dev_id)) =
            self.render_state.as_ref().map(|state| {
                (
                    state.surface.config.width,
                    state.surface.config.height,
                    state.valid_surface,
                    state.surface.dev_id,
                )
            })
        else {
            return;
        };
        if !valid_surface {
            return;
        }

        self.ensure_page_image(width, height);
        let (display_width, display_height) = self.current_display_size(width, height);
        let state = self
            .render_state
            .as_mut()
            .expect("Render state should exist during draw");

        self.scene.reset();
        self.scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            Color::from_rgb8(28, 30, 34),
            None,
            &Rect::new(0.0, 0.0, width as f64, height as f64),
        );

        if let Some(cached) = &self.cached_page {
            let x = (width as f32 - display_width) * 0.5 + self.pan.0;
            let y = (height as f32 - display_height) * 0.5 + self.pan.1;
            let scale_x = display_width / cached.target_width.max(1) as f32;
            let scale_y = display_height / cached.target_height.max(1) as f32;
            self.scene.draw_image(
                &cached.image,
                Affine::new([scale_x as f64, 0.0, 0.0, scale_y as f64, x as f64, y as f64]),
            );
        }

        let device_handle = &self.context.devices[dev_id];
        let renderer = self.renderer[dev_id]
            .as_mut()
            .expect("Renderer should be initialized");

        renderer
            .render_to_texture(
                &device_handle.device,
                &device_handle.queue,
                &self.scene,
                &state.surface.target_view,
                &RenderParams {
                    base_color: Color::from_rgb8(28, 30, 34),
                    width,
                    height,
                    antialiasing_method: AaConfig::Area,
                },
            )
            .expect("failed to render frame with vello");

        let surface_texture = match state.surface.surface.get_current_texture() {
            Ok(surface_texture) => surface_texture,
            Err(SurfaceError::Outdated) | Err(SurfaceError::Lost) => {
                let width = state.surface.config.width;
                let height = state.surface.config.height;
                self.context
                    .resize_surface(&mut state.surface, width, height);
                state.window.request_redraw();
                return;
            }
            Err(SurfaceError::Timeout) => {
                state.window.request_redraw();
                return;
            }
            Err(SurfaceError::OutOfMemory) => {
                panic!("surface out of memory")
            }
            Err(SurfaceError::Other) => {
                state.window.request_redraw();
                return;
            }
        };

        let mut encoder =
            device_handle
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("surface blit"),
                });
        state.surface.blitter.copy(
            &device_handle.device,
            &mut encoder,
            &state.surface.target_view,
            &surface_texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default()),
        );
        device_handle.queue.submit([encoder.finish()]);
        surface_texture.present();
    }

    fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.mark_view_changed();
    }

    fn go_to_page(&mut self, page_index: usize) {
        if page_index != self.page_index {
            self.page_index = page_index;
            self.pan = (0.0, 0.0);
            self.invalidate_cached_page();
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.render_state.is_some() {
            return;
        }

        let window = Arc::new(
            event_loop
                .create_window(window_attributes(load_app_icon()))
                .expect("failed to create window"),
        );
        self.update_title(&window);

        let size = window.inner_size();
        let surface = pollster::block_on(self.context.create_surface(
            window.clone(),
            size.width.max(1),
            size.height.max(1),
            wgpu::PresentMode::AutoVsync,
        ))
        .expect("failed to create vello surface");

        self.renderer
            .resize_with(self.context.devices.len(), || None);
        let dev_id = surface.dev_id;
        self.renderer[dev_id].get_or_insert_with(|| {
            Renderer::new(
                &self.context.devices[dev_id].device,
                RendererOptions::default(),
            )
            .expect("failed to create vello renderer")
        });

        self.render_state = Some(RenderState {
            window,
            surface,
            valid_surface: true,
        });
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        let Some(current_window_id) = self.render_state.as_ref().map(|s| s.window.id()) else {
            return;
        };
        if current_window_id != window_id {
            return;
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if size.width > 0 && size.height > 0 {
                    self.mark_view_changed();
                    if let Some(state) = &mut self.render_state {
                        self.context
                            .resize_surface(&mut state.surface, size.width, size.height);
                        state.valid_surface = true;
                    }
                } else {
                    if let Some(state) = &mut self.render_state {
                        state.valid_surface = false;
                    }
                }
                if let Some(state) = &self.render_state {
                    state.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                self.draw_frame();
            }
            WindowEvent::MouseInput {
                state: mouse_state,
                button: MouseButton::Left,
                ..
            } => {
                self.dragging = mouse_state == ElementState::Pressed;
                if !self.dragging {
                    self.last_cursor = None;
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.dragging {
                    if let Some((last_x, last_y)) = self.last_cursor {
                        self.pan.0 += (position.x - last_x) as f32;
                        self.pan.1 += (position.y - last_y) as f32;
                    }
                    if let Some(state) = &self.render_state {
                        state.window.request_redraw();
                    }
                }
                self.last_cursor = Some((position.x, position.y));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let y = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(pos) => pos.y / 32.0,
                };
                if y > 0.0 {
                    self.zoom_by(ZOOM_STEP);
                } else if y < 0.0 {
                    self.zoom_by(1.0 / ZOOM_STEP);
                }
                if let Some(state) = &self.render_state {
                    self.update_title(&state.window);
                    state.window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::ArrowRight) => {
                        let next = (self.page_index + 1).min(self.page_sizes.len() - 1);
                        self.go_to_page(next);
                    }
                    Key::Named(NamedKey::ArrowLeft) => {
                        self.go_to_page(self.page_index.saturating_sub(1));
                    }
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Character("+") | Key::Character("=") => self.zoom_by(ZOOM_STEP),
                    Key::Character("-") => self.zoom_by(1.0 / ZOOM_STEP),
                    Key::Character("0") => {
                        self.zoom = 1.0;
                        self.pan = (0.0, 0.0);
                        self.mark_view_changed();
                    }
                    _ => {}
                }
                if let Some(state) = &self.render_state {
                    self.update_title(&state.window);
                    state.window.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = &self.render_state {
            if self.pending_high_quality_render
                && self.last_view_change.elapsed() >= INTERACTION_SETTLE_DELAY
            {
                state.window.request_redraw();
            }
        }
    }
}

fn main() {
    let pdf_path = pdf_path_from_args();
    if !pdf_path.exists() {
        eprintln!(
            "ERROR: PDF file not found at '{}'. Provide a path as the first argument or place a 'test.pdf' next to the binary.",
            pdf_path.display()
        );
        std::process::exit(1);
    }

    let pdf_bytes = std::fs::read(&pdf_path).unwrap_or_else(|error| {
        eprintln!(
            "CRITICAL: Failed to read PDF file at '{}': {}",
            pdf_path.display(),
            error
        );
        std::process::exit(1);
    });

    let pdf = Pdf::new(pdf_bytes).unwrap_or_else(|error| {
        eprintln!(
            "CRITICAL: Failed to parse PDF file at '{}': {:?}",
            pdf_path.display(),
            error
        );
        std::process::exit(1);
    });

    let page_sizes = pdf
        .pages()
        .iter()
        .map(|page| page.render_dimensions())
        .collect::<Vec<_>>();

    if page_sizes.is_empty() {
        eprintln!("ERROR: PDF has no pages to display.");
        std::process::exit(1);
    }

    let event_loop = EventLoop::new().expect("failed to create event loop");
    let mut app = App::new(pdf, page_sizes);
    event_loop
        .run_app(&mut app)
        .expect("event loop exited with error");
}

fn window_attributes(icon: Option<Icon>) -> WindowAttributes {
    Window::default_attributes()
        .with_title("OxideReader")
        .with_inner_size(LogicalSize::new(1200, 900))
        .with_window_icon(icon)
        .with_resizable(true)
}

fn pdf_path_from_args() -> PathBuf {
    env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("test.pdf"))
}

fn load_app_icon() -> Option<Icon> {
    let icon_path = PathBuf::from("assets/app_icon.ico");
    if !icon_path.exists() {
        return None;
    }

    let bytes = std::fs::read(&icon_path).ok()?;
    let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Ico).ok()?;
    let rgba = image.into_rgba8();
    let (width, height) = rgba.dimensions();
    Icon::from_rgba(rgba.into_raw(), width, height).ok()
}
