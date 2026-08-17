use std::collections::{HashMap, HashSet};
use std::env;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

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
const REQUEST_OVERSAMPLE: f32 = 1.15;
const TILE_SIZE: u32 = 512;
const DIM_QUANTIZE: u32 = 128;
const MAX_LOD_LEVEL: u8 = 2;
const MAX_CACHE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct CacheKey {
    page_index: usize,
    lod: u8,
    width: u32,
    height: u32,
}

#[derive(Clone)]
struct CachedTile {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    image: ImageBrush,
}

struct CachedPageLevel {
    key: CacheKey,
    full_width: u32,
    full_height: u32,
    tiles_per_row: u32,
    tiles_per_col: u32,
    tiles: Vec<CachedTile>,
    bytes: usize,
    last_used_tick: u64,
}

struct DisplayItem {
    image: ImageBrush,
    transform: Affine,
}

enum WorkerRequest {
    Render(CacheKey),
    Shutdown,
}

struct WorkerResponse {
    key: CacheKey,
    rgba: Vec<u8>,
}

struct RenderState {
    window: Arc<Window>,
    surface: RenderSurface<'static>,
    valid_surface: bool,
}

struct App {
    page_sizes: Vec<(f32, f32)>,

    context: RenderContext,
    renderer: Vec<Option<Renderer>>,
    render_state: Option<RenderState>,

    scene: Scene,

    page_cache: HashMap<CacheKey, CachedPageLevel>,
    pending_requests: HashSet<CacheKey>,
    cache_bytes: usize,
    usage_tick: u64,

    worker_txs: Vec<Sender<WorkerRequest>>,
    worker_rx: Receiver<WorkerResponse>,
    next_worker: usize,

    page_index: usize,
    zoom: f32,
    pan: (f32, f32),
    dragging: bool,
    last_cursor: Option<(f64, f64)>,
    view_dirty: bool,
}

impl Drop for App {
    fn drop(&mut self) {
        for tx in &self.worker_txs {
            let _ = tx.send(WorkerRequest::Shutdown);
        }
    }
}

impl App {
    fn new(pdf_bytes: Vec<u8>, page_sizes: Vec<(f32, f32)>) -> Self {
        let (worker_txs, worker_rx) = spawn_render_workers(pdf_bytes);
        Self {
            page_sizes,
            context: RenderContext::new(),
            renderer: Vec::new(),
            render_state: None,
            scene: Scene::new(),
            page_cache: HashMap::new(),
            pending_requests: HashSet::new(),
            cache_bytes: 0,
            usage_tick: 0,
            worker_txs,
            worker_rx,
            next_worker: 0,
            page_index: 0,
            zoom: 1.0,
            pan: (0.0, 0.0),
            dragging: false,
            last_cursor: None,
            view_dirty: true,
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

    fn mark_view_changed(&mut self) {
        self.view_dirty = true;
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

    fn lod_for_zoom(zoom: f32) -> u8 {
        if zoom < 0.65 {
            2
        } else if zoom < 1.6 {
            1
        } else {
            0
        }
    }

    fn quantize_dim(value: u32) -> u32 {
        let quantized = value.div_ceil(DIM_QUANTIZE) * DIM_QUANTIZE;
        quantized.clamp(32, MAX_TEXTURE_SIZE)
    }

    fn make_cache_key_for_lod(
        &self,
        width: u32,
        height: u32,
        display_w: f32,
        display_h: f32,
        lod: u8,
    ) -> CacheKey {
        let lod_scale = 1.0 / ((1u32 << lod) as f32);
        let requested_w = (display_w * REQUEST_OVERSAMPLE * lod_scale)
            .round()
            .clamp(32.0, MAX_TEXTURE_SIZE as f32) as u32;
        let requested_h = (display_h * REQUEST_OVERSAMPLE * lod_scale)
            .round()
            .clamp(32.0, MAX_TEXTURE_SIZE as f32) as u32;

        let requested_w = requested_w.min(width.max(32));
        let requested_h = requested_h.min(height.max(32));

        CacheKey {
            page_index: self.page_index,
            lod,
            width: Self::quantize_dim(requested_w),
            height: Self::quantize_dim(requested_h),
        }
    }

    fn request_render_for_view(&mut self, width: u32, height: u32) {
        let (display_w, display_h) = self.current_display_size(width, height);
        let desired_lod = Self::lod_for_zoom(self.zoom);

        let mut lod_sequence = Vec::with_capacity(3);
        lod_sequence.push(desired_lod);
        if desired_lod < MAX_LOD_LEVEL {
            lod_sequence.push(desired_lod + 1);
        }
        if desired_lod > 0 {
            lod_sequence.push(desired_lod - 1);
        }

        for lod in lod_sequence {
            let key = self.make_cache_key_for_lod(width, height, display_w, display_h, lod);
            if self.page_cache.contains_key(&key) || self.pending_requests.contains(&key) {
                continue;
            }

            if self.dispatch_render_request(key) {
                self.pending_requests.insert(key);
            }
        }

        self.view_dirty = false;
    }

    fn dispatch_render_request(&mut self, key: CacheKey) -> bool {
        if self.worker_txs.is_empty() {
            return false;
        }

        for offset in 0..self.worker_txs.len() {
            let index = (self.next_worker + offset) % self.worker_txs.len();
            if self.worker_txs[index]
                .send(WorkerRequest::Render(key))
                .is_ok()
            {
                self.next_worker = (index + 1) % self.worker_txs.len();
                return true;
            }
        }

        false
    }

    fn poll_worker_responses(&mut self) {
        while let Ok(response) = self.worker_rx.try_recv() {
            self.pending_requests.remove(&response.key);
            self.insert_cached_level(response);
        }
    }

    fn insert_cached_level(&mut self, response: WorkerResponse) {
        if let Some(old) = self.page_cache.remove(&response.key) {
            self.cache_bytes = self.cache_bytes.saturating_sub(old.bytes);
        }

        let cached_level = build_tiled_level(response, self.usage_tick);
        self.usage_tick = self.usage_tick.wrapping_add(1);

        self.cache_bytes = self.cache_bytes.saturating_add(cached_level.bytes);
        self.page_cache.insert(cached_level.key, cached_level);
        self.evict_cache_if_needed();
    }

    fn evict_cache_if_needed(&mut self) {
        while self.cache_bytes > MAX_CACHE_BYTES {
            let oldest_key = self
                .page_cache
                .iter()
                .min_by_key(|(_, level)| level.last_used_tick)
                .map(|(key, _)| *key);

            let Some(oldest_key) = oldest_key else {
                break;
            };

            if let Some(removed) = self.page_cache.remove(&oldest_key) {
                self.cache_bytes = self.cache_bytes.saturating_sub(removed.bytes);
            }
        }
    }

    fn select_best_cached_key(
        &self,
        desired_lod: u8,
        desired_width: u32,
        desired_height: u32,
    ) -> Option<CacheKey> {
        self.page_cache
            .iter()
            .filter(|(key, _)| key.page_index == self.page_index)
            .min_by_key(|(key, _)| {
                let lod_score = (i32::from(key.lod) - i32::from(desired_lod)).abs() as u64 * 10_000;
                let width_score = key.width.abs_diff(desired_width) as u64;
                let height_score = key.height.abs_diff(desired_height) as u64;
                lod_score + width_score + height_score
            })
            .map(|(key, _)| *key)
    }

    fn build_display_list(
        &mut self,
        width: u32,
        height: u32,
        display_w: f32,
        display_h: f32,
        scale_factor: f64,
    ) -> Vec<DisplayItem> {
        let desired_lod = Self::lod_for_zoom(self.zoom);
        let desired_key =
            self.make_cache_key_for_lod(width, height, display_w, display_h, desired_lod);
        let Some(best_key) =
            self.select_best_cached_key(desired_lod, desired_key.width, desired_key.height)
        else {
            return Vec::new();
        };

        let Some(level) = self.page_cache.get_mut(&best_key) else {
            return Vec::new();
        };
        level.last_used_tick = self.usage_tick;
        self.usage_tick = self.usage_tick.wrapping_add(1);

        let base_x = (width as f32 - display_w) * 0.5 + self.pan.0;
        let base_y = (height as f32 - display_h) * 0.5 + self.pan.1;
        let page_scale_x = display_w / level.full_width.max(1) as f32;
        let page_scale_y = display_h / level.full_height.max(1) as f32;
        let subpixel_step = 1.0 / (scale_factor.max(1.0) * 3.0);

        let viewport = Rect::new(0.0, 0.0, width as f64, height as f64);
        let mut display_list = Vec::with_capacity(level.tiles.len().min(256));

        let page_visible_x0 = ((0.0f32 - base_x) / page_scale_x)
            .floor()
            .clamp(0.0, level.full_width as f32) as u32;
        let page_visible_y0 = ((0.0f32 - base_y) / page_scale_y)
            .floor()
            .clamp(0.0, level.full_height as f32) as u32;
        let page_visible_x1 = ((width as f32 - base_x) / page_scale_x)
            .ceil()
            .clamp(0.0, level.full_width as f32) as u32;
        let page_visible_y1 = ((height as f32 - base_y) / page_scale_y)
            .ceil()
            .clamp(0.0, level.full_height as f32) as u32;

        let tile_x_start = (page_visible_x0 / TILE_SIZE).min(level.tiles_per_row.saturating_sub(1));
        let tile_y_start = (page_visible_y0 / TILE_SIZE).min(level.tiles_per_col.saturating_sub(1));
        let tile_x_end = (page_visible_x1.div_ceil(TILE_SIZE)).min(level.tiles_per_row);
        let tile_y_end = (page_visible_y1.div_ceil(TILE_SIZE)).min(level.tiles_per_col);

        for tile_row in tile_y_start..tile_y_end {
            for tile_col in tile_x_start..tile_x_end {
                let tile_index = (tile_row * level.tiles_per_row + tile_col) as usize;
                let Some(tile) = level.tiles.get(tile_index) else {
                    continue;
                };

                let tile_x = base_x as f64 + tile.x as f64 * page_scale_x as f64;
                let tile_y = base_y as f64 + tile.y as f64 * page_scale_y as f64;
                let tile_w = tile.width as f64 * page_scale_x as f64;
                let tile_h = tile.height as f64 * page_scale_y as f64;

                let snapped_x0 = (tile_x / subpixel_step).round() * subpixel_step;
                let snapped_y0 = (tile_y / subpixel_step).round() * subpixel_step;
                let snapped_x1 = ((tile_x + tile_w) / subpixel_step).round() * subpixel_step;
                let snapped_y1 = ((tile_y + tile_h) / subpixel_step).round() * subpixel_step;

                if snapped_x1 <= snapped_x0 || snapped_y1 <= snapped_y0 {
                    continue;
                }

                let tile_rect = Rect::new(snapped_x0, snapped_y0, snapped_x1, snapped_y1);
                if !rects_intersect(&viewport, &tile_rect) {
                    continue;
                }

                let tile_scale_x = (snapped_x1 - snapped_x0) / tile.width.max(1) as f64;
                let tile_scale_y = (snapped_y1 - snapped_y0) / tile.height.max(1) as f64;
                display_list.push(DisplayItem {
                    image: tile.image.clone(),
                    transform: Affine::new([
                        tile_scale_x,
                        0.0,
                        0.0,
                        tile_scale_y,
                        snapped_x0,
                        snapped_y0,
                    ]),
                });
            }
        }

        display_list
    }

    fn draw_frame(&mut self) {
        self.poll_worker_responses();

        let Some((width, height, valid_surface, dev_id, scale_factor)) =
            self.render_state.as_ref().map(|state| {
                (
                    state.surface.config.width,
                    state.surface.config.height,
                    state.valid_surface,
                    state.surface.dev_id,
                    state.window.scale_factor(),
                )
            })
        else {
            return;
        };
        if !valid_surface {
            return;
        }

        if self.view_dirty {
            self.request_render_for_view(width, height);
        }

        let (display_width, display_height) = self.current_display_size(width, height);
        let display_list =
            self.build_display_list(width, height, display_width, display_height, scale_factor);

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

        for item in display_list {
            self.scene.draw_image(&item.image, item.transform);
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
            self.mark_view_changed();
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
        self.mark_view_changed();
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
                } else if let Some(state) = &mut self.render_state {
                    state.valid_surface = false;
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
        self.poll_worker_responses();

        if let Some(state) = &self.render_state {
            if !self.pending_requests.is_empty() || self.view_dirty {
                state.window.request_redraw();
            }
        }
    }
}

fn spawn_render_workers(
    pdf_bytes: Vec<u8>,
) -> (Vec<Sender<WorkerRequest>>, Receiver<WorkerResponse>) {
    let (response_tx, response_rx) = mpsc::channel::<WorkerResponse>();

    let worker_count = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(2)
        .clamp(1, 4);

    let mut worker_txs = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let (request_tx, request_rx) = mpsc::channel::<WorkerRequest>();
        let response_tx = response_tx.clone();
        let worker_pdf_bytes = pdf_bytes.clone();

        thread::spawn(move || {
            let pdf = Pdf::new(worker_pdf_bytes).expect("failed to parse PDF in render worker");
            let render_cache = hayro::RenderCache::new();
            let interpreter_settings = InterpreterSettings::default();
            let pages = pdf.pages();

            while let Ok(mut message) = request_rx.recv() {
                while let Ok(next_message) = request_rx.try_recv() {
                    message = next_message;
                }

                match message {
                    WorkerRequest::Shutdown => break,
                    WorkerRequest::Render(key) => {
                        let Some(page) = pages.get(key.page_index) else {
                            continue;
                        };
                        let (page_w, page_h) = page.render_dimensions();

                        let render_settings = RenderSettings {
                            x_scale: key.width as f32 / page_w,
                            y_scale: key.height as f32 / page_h,
                            width: Some(
                                u16::try_from(key.width)
                                    .expect("Page render width must fit in u16"),
                            ),
                            height: Some(
                                u16::try_from(key.height)
                                    .expect("Page render height must fit in u16"),
                            ),
                            bg_color: WHITE,
                        };

                        let pixmap =
                            render(page, &render_cache, &interpreter_settings, &render_settings);
                        let rgba = pixmap.data_as_u8_slice().to_vec();

                        if response_tx.send(WorkerResponse { key, rgba }).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        worker_txs.push(request_tx);
    }

    (worker_txs, response_rx)
}

fn build_tiled_level(response: WorkerResponse, last_used_tick: u64) -> CachedPageLevel {
    let full_width = response.key.width;
    let full_height = response.key.height;
    let tiles_per_row = full_width.div_ceil(TILE_SIZE);
    let tiles_per_col = full_height.div_ceil(TILE_SIZE);
    let mut tiles = Vec::new();
    let mut total_bytes = 0usize;

    let stride = full_width as usize * 4;

    for tile_y in (0..full_height).step_by(TILE_SIZE as usize) {
        for tile_x in (0..full_width).step_by(TILE_SIZE as usize) {
            let tile_width = (full_width - tile_x).min(TILE_SIZE);
            let tile_height = (full_height - tile_y).min(TILE_SIZE);

            let mut tile_rgba = vec![0u8; (tile_width * tile_height * 4) as usize];
            for row in 0..tile_height as usize {
                let src_start = (tile_y as usize + row) * stride + tile_x as usize * 4;
                let src_end = src_start + tile_width as usize * 4;
                let dst_start = row * tile_width as usize * 4;
                let dst_end = dst_start + tile_width as usize * 4;
                tile_rgba[dst_start..dst_end].copy_from_slice(&response.rgba[src_start..src_end]);
            }

            total_bytes += tile_rgba.len();

            let image_data = ImageData {
                data: Blob::new(Arc::new(tile_rgba)),
                format: ImageFormat::Rgba8,
                width: tile_width,
                height: tile_height,
                alpha_type: ImageAlphaType::Alpha,
            };

            tiles.push(CachedTile {
                x: tile_x,
                y: tile_y,
                width: tile_width,
                height: tile_height,
                image: image_data.into(),
            });
        }
    }

    CachedPageLevel {
        key: response.key,
        full_width,
        full_height,
        tiles_per_row,
        tiles_per_col,
        tiles,
        bytes: total_bytes,
        last_used_tick,
    }
}

fn rects_intersect(a: &Rect, b: &Rect) -> bool {
    a.x0 < b.x1 && a.x1 > b.x0 && a.y0 < b.y1 && a.y1 > b.y0
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

    let pdf = Pdf::new(pdf_bytes.clone()).unwrap_or_else(|error| {
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
    let mut app = App::new(pdf_bytes, page_sizes);
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
