use std::env;
use std::ffi::CString;
use std::os::raw;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use winit::event::{
    ElementState, Event, KeyEvent, MouseButton, MouseScrollDelta, StartCause, WindowEvent,
};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::WindowBuilder;

use raw_window_handle::{HasRawDisplayHandle, HasRawWindowHandle};

use ash::extensions::khr;
use ash::vk::{self as avk, Handle};

use skia_safe::gpu::vk as skia_vk;
use skia_safe::gpu::SurfaceOrigin;
use skia_safe::{
    font::Edging, AlphaType, Color, Color4f, ColorType, CubicResampler, Data, Font, FontHinting,
    FontMgr, FontStyle, Image, ImageInfo, Paint, Point, Rect, SamplingOptions,
};

use pdfium_render::prelude::*;

const MAX_TEXTURE_SIZE: i32 = 16384;
const ZOOM_DEBOUNCE_MS: u64 = 150;
const ZOOM_CACHE_MAX_ENTRIES: usize = 5;
const ZOOM_FACTOR: f32 = 1.10;
/// Multiplier to convert internal zoom_level to displayed zoom percentage.
/// At zoom_level=1.0 the page fits the window, which corresponds to ~77.4% of the PDF's native size.
const ZOOM_TO_PERCENT: f32 = 77.4;
const MAX_ZOOM_PERCENT: f32 = 6200.0;
const MAX_ZOOM_LEVEL: f32 = MAX_ZOOM_PERCENT / ZOOM_TO_PERCENT;

fn main() {
    // 1. Initialize PDFium
    let pdfium_bindings =
        Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path("./"))
            .or_else(|_| {
                Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path("./libs"))
            })
            .expect("CRITICAL: Could not find PDFium library.");

    let pdfium = Pdfium::new(pdfium_bindings);
    let pdf_path = pdf_path_from_args();
    if !pdf_path.exists() {
        eprintln!(
            "ERROR: PDF file not found at '{}'. Provide a path as the first argument or place a 'test.pdf' next to the binary.",
            pdf_path.display()
        );
        std::process::exit(1);
    }

    let document = pdfium
        .load_pdf_from_file(&pdf_path, None)
        .expect("CRITICAL: Failed to open the PDF file.");

    let total_pages = document.pages().len();

    // 2. Setup Windowing
    let event_loop = EventLoop::new().unwrap();
    let window = WindowBuilder::new()
        .with_title("Rust Skia PDF Viewer")
        .build(&event_loop)
        .unwrap();

    // 3. Setup Vulkan
    // Note: Box::leak is used intentionally for Vulkan resources that must outlive the event loop
    // closure. The GetProc callback and BackendContext require 'static references to entry/instance/device.
    // These are cleaned up by the OS on process exit.
    let entry: &'static ash::Entry = Box::leak(Box::new(
        unsafe { ash::Entry::load() }.expect("Failed to load Vulkan library"),
    ));

    let instance: &'static ash::Instance = Box::leak(Box::new({
        let app_name = CString::new("Rust PDF Viewer").unwrap();
        let engine_name = CString::new("Skia").unwrap();
        let app_info = avk::ApplicationInfo::builder()
            .application_name(&app_name)
            .application_version(avk::make_api_version(0, 1, 0, 0))
            .engine_name(&engine_name)
            .engine_version(avk::make_api_version(0, 1, 0, 0))
            .api_version(avk::make_api_version(0, 1, 2, 0));

        let display_handle = window.raw_display_handle();
        let required_extensions =
            ash_window::enumerate_required_extensions(display_handle)
                .expect("Failed to enumerate required Vulkan extensions");

        let create_info = avk::InstanceCreateInfo::builder()
            .application_info(&app_info)
            .enabled_extension_names(required_extensions);

        unsafe {
            entry
                .create_instance(&create_info, None)
                .expect("Failed to create Vulkan instance")
        }
    }));

    let vk_surface = unsafe {
        ash_window::create_surface(
            entry,
            instance,
            window.raw_display_handle(),
            window.raw_window_handle(),
            None,
        )
        .expect("Failed to create Vulkan surface")
    };
    let surface_loader = khr::Surface::new(entry, instance);

    // Pick physical device with graphics + present support
    let (physical_device, queue_family_index) = {
        let devices = unsafe {
            instance
                .enumerate_physical_devices()
                .expect("No Vulkan devices found")
        };
        devices
            .into_iter()
            .find_map(|pd| {
                let queue_families =
                    unsafe { instance.get_physical_device_queue_family_properties(pd) };
                queue_families.iter().enumerate().find_map(|(idx, qf)| {
                    let supports_graphics = qf.queue_flags.contains(avk::QueueFlags::GRAPHICS);
                    let supports_present = unsafe {
                        surface_loader
                            .get_physical_device_surface_support(pd, idx as u32, vk_surface)
                            .unwrap_or(false)
                    };
                    if supports_graphics && supports_present {
                        Some((pd, idx as u32))
                    } else {
                        None
                    }
                })
            })
            .expect("No suitable GPU found with graphics and present support")
    };

    // Create logical device
    let device: &'static ash::Device = Box::leak(Box::new({
        let queue_priorities = [1.0f32];
        let queue_create_info = avk::DeviceQueueCreateInfo::builder()
            .queue_family_index(queue_family_index)
            .queue_priorities(&queue_priorities);

        let swapchain_ext = khr::Swapchain::name();
        let device_extensions = [swapchain_ext.as_ptr()];

        let device_create_info = avk::DeviceCreateInfo::builder()
            .queue_create_infos(std::slice::from_ref(&queue_create_info))
            .enabled_extension_names(&device_extensions);

        unsafe {
            instance
                .create_device(physical_device, &device_create_info, None)
                .expect("Failed to create Vulkan device")
        }
    }));

    let graphics_queue = unsafe { device.get_device_queue(queue_family_index, 0) };
    let swapchain_loader = khr::Swapchain::new(instance, device);

    // Create swapchain
    let mut swapchain_state = create_swapchain(
        &surface_loader,
        &swapchain_loader,
        physical_device,
        vk_surface,
        &window,
        None,
    );

    // Create synchronization primitive for image acquisition
    let semaphore_info = avk::SemaphoreCreateInfo::builder();
    let image_available_semaphore =
        unsafe { device.create_semaphore(&semaphore_info, None).unwrap() };

    // 4. Create Skia Vulkan context
    let get_proc = |of: skia_vk::GetProcOf| -> skia_vk::GetProcResult {
        unsafe {
            match of {
                skia_vk::GetProcOf::Instance(inst, name) => {
                    let ash_inst = avk::Instance::from_raw(inst as u64);
                    let fp = entry.get_instance_proc_addr(ash_inst, name);
                    fp.map_or(std::ptr::null(), |f| f as *const raw::c_void)
                }
                skia_vk::GetProcOf::Device(dev, name) => {
                    let ash_dev = avk::Device::from_raw(dev as u64);
                    let fp = instance.get_device_proc_addr(ash_dev, name);
                    fp.map_or(std::ptr::null(), |f| f as *const raw::c_void)
                }
            }
        }
    };

    let backend_context = unsafe {
        skia_vk::BackendContext::new(
            instance.handle().as_raw() as skia_vk::Instance,
            physical_device.as_raw() as skia_vk::PhysicalDevice,
            device.handle().as_raw() as skia_vk::Device,
            (
                graphics_queue.as_raw() as skia_vk::Queue,
                queue_family_index as usize,
            ),
            &get_proc,
        )
    };

    let mut gr_context = skia_safe::gpu::direct_contexts::make_vulkan(&backend_context, None)
        .expect("Failed to create Skia Vulkan DirectContext");

    // 5. Initialize Font for UI (TrueType with proper hinting)
    let font_mgr = FontMgr::default();
    let typeface = font_mgr
        .match_family_style("Arial", FontStyle::normal())
        .or_else(|| font_mgr.match_family_style("Segoe UI", FontStyle::normal()))
        .or_else(|| font_mgr.match_family_style("", FontStyle::normal()))
        .expect("Failed to load any system font.");

    let mut ui_font = Font::from_typeface(typeface, 24.0);
    ui_font.set_hinting(FontHinting::Full);
    ui_font.set_subpixel(true);
    ui_font.set_edging(Edging::SubpixelAntiAlias);

    // --- APP STATE ---
    let mut cached_pdf_image: Option<Image> = None;
    let mut current_page_index: u16 = 0;

    let mut zoom_level: f32 = 1.0;
    let mut rendered_zoom: f32 = 0.0;
    let mut pan_offset = (0.0f32, 0.0f32);
    let mut content_rect = Rect::default();

    let mut is_dragging = false;
    let mut last_mouse_pos = (0.0f32, 0.0f32);

    // Debounced zoom state
    let mut last_zoom_time: Option<Instant> = None;

    // Zoom cache: (zoom_key, window_width, window_height, page_index, image, content_rect)
    let mut zoom_cache: Vec<(i32, u32, u32, u16, Image, Rect)> = Vec::new();

    // Antialiasing settings
    let mut text_smoothing = true;
    let mut path_smoothing = true;
    let mut image_smoothing = true;
    let mut show_settings_menu = false;

    // 6. Run Loop
    event_loop
        .run(move |event, target| {
            // Only go to sleep when no zoom timer is pending; otherwise
            // keep the WaitUntil deadline so the debounce fires reliably.
            if last_zoom_time.is_none() {
                target.set_control_flow(ControlFlow::Wait);
            }

            match event {
                Event::LoopExiting => {
                    gr_context.abandon();
                }

                // Wake up after zoom debounce period to trigger re-render
                Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                    if last_zoom_time.is_some() {
                        window.request_redraw();
                    }
                }

                Event::WindowEvent { event, .. } => match event {
                    WindowEvent::CloseRequested => target.exit(),

                    // --- INPUT HANDLING ---
                    WindowEvent::MouseInput {
                        state,
                        button: MouseButton::Left,
                        ..
                    } => {
                        if state == ElementState::Pressed && show_settings_menu {
                            // Check if click is within the settings menu
                            let menu_x = 20.0_f32;
                            let menu_y = 20.0_f32;
                            let menu_width = 280.0_f32;
                            let header_height = 40.0_f32;
                            let row_height = 32.0_f32;
                            let num_items = 3;
                            let menu_height =
                                header_height + row_height * num_items as f32 + 10.0;
                            let (mx, my) = last_mouse_pos;

                            if mx >= menu_x
                                && mx <= menu_x + menu_width
                                && my >= menu_y
                                && my <= menu_y + menu_height
                            {
                                // Determine which row was clicked
                                let row_y_start = menu_y + header_height;
                                if my >= row_y_start {
                                    let row_index =
                                        ((my - row_y_start) / row_height) as usize;
                                    let toggled = match row_index {
                                        0 => {
                                            text_smoothing = !text_smoothing;
                                            true
                                        }
                                        1 => {
                                            path_smoothing = !path_smoothing;
                                            true
                                        }
                                        2 => {
                                            image_smoothing = !image_smoothing;
                                            true
                                        }
                                        _ => false,
                                    };
                                    if toggled {
                                        cached_pdf_image = None;
                                        zoom_cache.clear();
                                        rendered_zoom = 0.0;
                                        window.request_redraw();
                                    }
                                }
                                // Don't start dragging when clicking inside the menu
                            } else {
                                is_dragging = true;
                            }
                        } else {
                            is_dragging = state == ElementState::Pressed;
                        }
                    }

                    WindowEvent::CursorMoved { position, .. } => {
                        let current_x = position.x as f32;
                        let current_y = position.y as f32;

                        if is_dragging {
                            let dx = current_x - last_mouse_pos.0;
                            let dy = current_y - last_mouse_pos.1;
                            pan_offset.0 += dx;
                            pan_offset.1 += dy;
                            window.request_redraw();
                        }
                        last_mouse_pos = (current_x, current_y);
                    }

                    WindowEvent::MouseWheel { delta, .. } => {
                        let scroll_y = match delta {
                            MouseScrollDelta::LineDelta(_, y) => y,
                            MouseScrollDelta::PixelDelta(pos) => pos.y as f32,
                        };

                        let old_zoom = zoom_level;
                        if scroll_y > 0.0 {
                            zoom_level *= ZOOM_FACTOR;
                        } else if scroll_y < 0.0 {
                            zoom_level /= ZOOM_FACTOR;
                        }
                        if zoom_level < 0.1 {
                            zoom_level = 0.1;
                        }
                        if zoom_level > MAX_ZOOM_LEVEL {
                            zoom_level = MAX_ZOOM_LEVEL;
                        }

                        // Zoom-to-cursor: adjust pan so the point under the
                        // mouse stays fixed after the zoom change.
                        let win = window.inner_size();
                        zoom_to_cursor(
                            old_zoom,
                            zoom_level,
                            last_mouse_pos,
                            (win.width as f32, win.height as f32),
                            &mut pan_offset,
                        );

                        // Debounce: record time, don't invalidate cache yet
                        last_zoom_time = Some(Instant::now());
                        window.request_redraw();
                        target.set_control_flow(ControlFlow::WaitUntil(
                            Instant::now() + Duration::from_millis(ZOOM_DEBOUNCE_MS),
                        ));
                    }

                    WindowEvent::KeyboardInput {
                        event:
                            KeyEvent {
                                logical_key,
                                state: ElementState::Pressed,
                                ..
                            },
                        ..
                    } => {
                        let mut needs_rerender = false;
                        let mut page_changed = false;

                        match logical_key {
                            Key::Named(NamedKey::ArrowRight) => {
                                if current_page_index < total_pages as u16 - 1 {
                                    current_page_index += 1;
                                    pan_offset = (0.0, 0.0);
                                    page_changed = true;
                                }
                            }
                            Key::Named(NamedKey::ArrowLeft) => {
                                if current_page_index > 0 {
                                    current_page_index -= 1;
                                    pan_offset = (0.0, 0.0);
                                    page_changed = true;
                                }
                            }
                            Key::Character(c) => match c.as_str() {
                                "+" | "=" => {
                                    let old_zoom = zoom_level;
                                    zoom_level *= ZOOM_FACTOR;
                                    if zoom_level > MAX_ZOOM_LEVEL {
                                        zoom_level = MAX_ZOOM_LEVEL;
                                    }
                                    let win = window.inner_size();
                                    zoom_to_cursor(
                                        old_zoom,
                                        zoom_level,
                                        last_mouse_pos,
                                        (win.width as f32, win.height as f32),
                                        &mut pan_offset,
                                    );
                                    needs_rerender = true;
                                }
                                "-" => {
                                    let old_zoom = zoom_level;
                                    zoom_level /= ZOOM_FACTOR;
                                    if zoom_level < 0.1 {
                                        zoom_level = 0.1;
                                    }
                                    let win = window.inner_size();
                                    zoom_to_cursor(
                                        old_zoom,
                                        zoom_level,
                                        last_mouse_pos,
                                        (win.width as f32, win.height as f32),
                                        &mut pan_offset,
                                    );
                                    needs_rerender = true;
                                }
                                "0" => {
                                    zoom_level = 1.0;
                                    pan_offset = (0.0, 0.0);
                                    cached_pdf_image = None;
                                    rendered_zoom = zoom_level;
                                    last_zoom_time = None;
                                    zoom_cache.clear();
                                    window.request_redraw();
                                }
                                "s" | "S" => {
                                    show_settings_menu = !show_settings_menu;
                                    window.request_redraw();
                                }
                                "1" => {
                                    if show_settings_menu {
                                        text_smoothing = !text_smoothing;
                                        cached_pdf_image = None;
                                        zoom_cache.clear();
                                        rendered_zoom = 0.0;
                                        window.request_redraw();
                                    }
                                }
                                "2" => {
                                    if show_settings_menu {
                                        path_smoothing = !path_smoothing;
                                        cached_pdf_image = None;
                                        zoom_cache.clear();
                                        rendered_zoom = 0.0;
                                        window.request_redraw();
                                    }
                                }
                                "3" => {
                                    if show_settings_menu {
                                        image_smoothing = !image_smoothing;
                                        cached_pdf_image = None;
                                        zoom_cache.clear();
                                        rendered_zoom = 0.0;
                                        window.request_redraw();
                                    }
                                }
                                _ => {}
                            },
                            _ => {}
                        }

                        if page_changed {
                            cached_pdf_image = None;
                            zoom_cache.clear();
                            rendered_zoom = 0.0;
                            window.request_redraw();
                        } else if needs_rerender {
                            last_zoom_time = Some(Instant::now());
                            window.request_redraw();
                            target.set_control_flow(ControlFlow::WaitUntil(
                                Instant::now() + Duration::from_millis(ZOOM_DEBOUNCE_MS),
                            ));
                        }
                    }

                    WindowEvent::Resized(physical_size) => {
                        if physical_size.width > 0 && physical_size.height > 0 {
                            unsafe { device.device_wait_idle().unwrap() };
                            swapchain_state = create_swapchain(
                                &surface_loader,
                                &swapchain_loader,
                                physical_device,
                                vk_surface,
                                &window,
                                Some(swapchain_state.swapchain),
                            );
                            cached_pdf_image = None;
                            zoom_cache.clear();
                            rendered_zoom = 0.0;
                        }
                        window.request_redraw();
                    }

                    WindowEvent::RedrawRequested => {
                        let size = window.inner_size();
                        if size.width == 0 || size.height == 0 {
                            return;
                        }

                        // --- DEBOUNCED ZOOM LOGIC ---
                        // Check if zoom has settled after debounce period
                        if zoom_level != rendered_zoom {
                            if let Some(last_time) = last_zoom_time {
                                if last_time.elapsed()
                                    >= Duration::from_millis(ZOOM_DEBOUNCE_MS)
                                {
                                    // Debounce settled - check cache or trigger full render
                                    let zoom_key = (zoom_level * 1000.0).round() as i32;
                                    if let Some(entry) = zoom_cache.iter().find(
                                        |(k, w, h, p, _, _)| {
                                            *k == zoom_key
                                                && *w == size.width
                                                && *h == size.height
                                                && *p == current_page_index
                                        },
                                    ) {
                                        cached_pdf_image = Some(entry.4.clone());
                                        content_rect = entry.5;
                                    } else {
                                        cached_pdf_image = None;
                                    }
                                    rendered_zoom = zoom_level;
                                    last_zoom_time = None;
                                }
                                // else: still zooming rapidly, show scaled preview
                                // Ensure we wake up after the debounce period to re-render
                                else {
                                    target.set_control_flow(ControlFlow::WaitUntil(
                                        last_time + Duration::from_millis(ZOOM_DEBOUNCE_MS),
                                    ));
                                }
                            }
                        }

                        // Acquire swapchain image
                        let acquire_result = unsafe {
                            swapchain_loader.acquire_next_image(
                                swapchain_state.swapchain,
                                u64::MAX,
                                image_available_semaphore,
                                avk::Fence::null(),
                            )
                        };

                        let image_index = match acquire_result {
                            Ok((index, _)) => index,
                            Err(avk::Result::ERROR_OUT_OF_DATE_KHR) => {
                                // Swapchain needs recreation
                                unsafe { device.device_wait_idle().unwrap() };
                                swapchain_state = create_swapchain(
                                    &surface_loader,
                                    &swapchain_loader,
                                    physical_device,
                                    vk_surface,
                                    &window,
                                    Some(swapchain_state.swapchain),
                                );
                                cached_pdf_image = None;
                                zoom_cache.clear();
                                rendered_zoom = 0.0;
                                window.request_redraw();
                                return;
                            }
                            Err(e) => panic!("Failed to acquire swapchain image: {:?}", e),
                        };

                        let swapchain_image = swapchain_state.images[image_index as usize];

                        // Create Skia surface from swapchain image
                        let vk_image_info = unsafe {
                            skia_vk::ImageInfo::new(
                                swapchain_image.as_raw() as skia_vk::Image,
                                skia_vk::Alloc::default(),
                                skia_vk::ImageTiling::OPTIMAL,
                                skia_vk::ImageLayout::UNDEFINED,
                                vk_format_to_skia(swapchain_state.format),
                                1,
                                queue_family_index,
                                None,
                                None,
                                None,
                            )
                        };

                        let backend_render_target =
                            skia_safe::gpu::backend_render_targets::make_vk(
                                (
                                    swapchain_state.extent.width as i32,
                                    swapchain_state.extent.height as i32,
                                ),
                                &vk_image_info,
                            );

                        let mut skia_surface =
                            skia_safe::gpu::surfaces::wrap_backend_render_target(
                                &mut gr_context,
                                &backend_render_target,
                                SurfaceOrigin::TopLeft,
                                ColorType::BGRA8888,
                                None,
                                None,
                            )
                            .expect("Failed to create Skia surface from Vulkan render target");

                        // --- RENDER PDF ---
                        if cached_pdf_image.is_none() {
                            let page = document
                                .pages()
                                .get(current_page_index)
                                .expect("Error loading page");

                            let page_width_pts = page.width().value;
                            let page_height_pts = page.height().value;
                            let aspect_ratio = page_width_pts / page_height_pts;
                            let window_ratio = size.width as f32 / size.height as f32;

                            let (fit_width, fit_height) = if window_ratio > aspect_ratio {
                                let h = size.height as f32;
                                let w = h * aspect_ratio;
                                (w, h)
                            } else {
                                let w = size.width as f32;
                                let h = w / aspect_ratio;
                                (w, h)
                            };

                            let final_width = (fit_width * zoom_level).round() as i32;
                            let final_height = (fit_height * zoom_level).round() as i32;

                            let mut texture_width = final_width;
                            let mut texture_height = final_height;

                            if texture_width > MAX_TEXTURE_SIZE
                                || texture_height > MAX_TEXTURE_SIZE
                            {
                                let scale = if texture_width > texture_height {
                                    MAX_TEXTURE_SIZE as f32 / texture_width as f32
                                } else {
                                    MAX_TEXTURE_SIZE as f32 / texture_height as f32
                                };
                                texture_width = (texture_width as f32 * scale) as i32;
                                texture_height = (texture_height as f32 * scale) as i32;
                            }

                            // Anti-aliasing: smooth strokes, enhance fine lines, smooth images
                            let mut render_config = PdfRenderConfig::new()
                                .set_target_width(texture_width)
                                .set_target_height(texture_height)
                                .set_format(PdfBitmapFormat::BGRA)
                                .use_print_quality(true)
                                .render_annotations(true);

                            if text_smoothing {
                                render_config = render_config.set_text_smoothing(true);
                            }
                            if path_smoothing {
                                render_config = render_config.set_path_smoothing(true);
                            }
                            if image_smoothing {
                                render_config = render_config.set_image_smoothing(true);
                            }

                            let bitmap = page
                                .render_with_config(&render_config)
                                .expect("Failed to render page");

                            let image_info = ImageInfo::new(
                                (texture_width, texture_height),
                                ColorType::BGRA8888,
                                AlphaType::Premul,
                                None,
                            );

                            let data = Data::new_copy(&bitmap.as_raw_bytes());
                            let row_bytes = texture_width as usize * 4;

                            cached_pdf_image =
                                skia_safe::images::raster_from_data(&image_info, data, row_bytes);

                            let x = (size.width as f32 - final_width as f32) / 2.0;
                            let y = (size.height as f32 - final_height as f32) / 2.0;

                            content_rect = Rect::from_xywh(
                                x,
                                y,
                                final_width as f32,
                                final_height as f32,
                            );

                            // Store in zoom cache
                            if let Some(ref image) = cached_pdf_image {
                                let zoom_key = (zoom_level * 1000.0).round() as i32;
                                // Remove old entry with same key if present
                                zoom_cache.retain(|(k, w, h, p, _, _)| {
                                    !(*k == zoom_key
                                        && *w == size.width
                                        && *h == size.height
                                        && *p == current_page_index)
                                });
                                if zoom_cache.len() >= ZOOM_CACHE_MAX_ENTRIES {
                                    zoom_cache.remove(0);
                                }
                                zoom_cache.push((
                                    zoom_key,
                                    size.width,
                                    size.height,
                                    current_page_index,
                                    image.clone(),
                                    content_rect,
                                ));
                            }

                            // Sync rendered_zoom so debounce logic knows this level is current
                            rendered_zoom = zoom_level;
                        }

                        // --- DRAW ---
                        let canvas = skia_surface.canvas();
                        canvas.clear(Color::from_rgb(30, 30, 30));

                        if let Some(image) = &cached_pdf_image {
                            // During debounced zoom, scale the cached image as a preview
                            let display_rect =
                                if last_zoom_time.is_some() && rendered_zoom > 0.0 {
                                    let zoom_scale = zoom_level / rendered_zoom;
                                    let preview_width = content_rect.width() * zoom_scale;
                                    let preview_height = content_rect.height() * zoom_scale;
                                    let preview_x =
                                        (size.width as f32 - preview_width) / 2.0;
                                    let preview_y =
                                        (size.height as f32 - preview_height) / 2.0;
                                    Rect::from_xywh(
                                        preview_x + pan_offset.0,
                                        preview_y + pan_offset.1,
                                        preview_width,
                                        preview_height,
                                    )
                                } else {
                                    Rect::from_xywh(
                                        content_rect.x() + pan_offset.0,
                                        content_rect.y() + pan_offset.1,
                                        content_rect.width(),
                                        content_rect.height(),
                                    )
                                };

                            // High-quality image scaling with CatmullRom cubic resampler
                            let sampling =
                                SamplingOptions::from(CubicResampler::catmull_rom());
                            let paint = Paint::default();
                            canvas.draw_image_rect_with_sampling_options(
                                image, None, display_rect, sampling, &paint,
                            );
                        }

                        // --- DRAW SETTINGS MENU ---
                        if show_settings_menu {
                            let menu_x = 20.0_f32;
                            let menu_y = 20.0_f32;
                            let menu_width = 280.0_f32;
                            let header_height = 40.0_f32;
                            let row_height = 32.0_f32;
                            let items: [(&str, bool); 3] = [
                                ("1  Text Smoothing", text_smoothing),
                                ("2  Path Smoothing", path_smoothing),
                                ("3  Image Smoothing", image_smoothing),
                            ];
                            let menu_height =
                                header_height + row_height * items.len() as f32 + 10.0;

                            // Background
                            let mut menu_bg = Paint::new(
                                Color4f::from(Color::from_argb(210, 25, 25, 25)),
                                None,
                            );
                            menu_bg.set_anti_alias(true);
                            canvas.draw_rect(
                                Rect::from_xywh(menu_x, menu_y, menu_width, menu_height),
                                &menu_bg,
                            );

                            // Header
                            let mut header_paint =
                                Paint::new(Color4f::from(Color::WHITE), None);
                            header_paint.set_anti_alias(true);
                            canvas.draw_str(
                                "Antialiasing Settings",
                                Point::new(menu_x + 10.0, menu_y + 28.0),
                                &ui_font,
                                &header_paint,
                            );

                            // Separator line
                            let mut sep_paint = Paint::new(
                                Color4f::from(Color::from_argb(100, 255, 255, 255)),
                                None,
                            );
                            sep_paint.set_anti_alias(true);
                            sep_paint.set_stroke_width(1.0);
                            sep_paint.set_style(skia_safe::PaintStyle::Stroke);
                            canvas.draw_line(
                                Point::new(menu_x + 10.0, menu_y + header_height),
                                Point::new(
                                    menu_x + menu_width - 10.0,
                                    menu_y + header_height,
                                ),
                                &sep_paint,
                            );

                            // Items
                            let small_font =
                                Font::from_typeface(ui_font.typeface(), 18.0);
                            for (i, (label, enabled)) in items.iter().enumerate() {
                                let iy = menu_y
                                    + header_height
                                    + row_height * i as f32
                                    + 24.0;
                                let status = if *enabled { "  ON" } else { "  OFF" };
                                let item_text = format!("{}{}", label, status);

                                let color = if *enabled {
                                    Color::from_rgb(100, 220, 100)
                                } else {
                                    Color::from_rgb(180, 80, 80)
                                };
                                let mut item_paint =
                                    Paint::new(Color4f::from(color), None);
                                item_paint.set_anti_alias(true);
                                canvas.draw_str(
                                    &item_text,
                                    Point::new(menu_x + 14.0, iy),
                                    &small_font,
                                    &item_paint,
                                );
                            }
                        }

                        // --- DRAW ZOOM PERCENTAGE ---
                        let real_zoom = zoom_level * ZOOM_TO_PERCENT;
                        let text = format!("{:.1}%", real_zoom);

                        let mut text_paint =
                            Paint::new(Color4f::from(Color::WHITE), None);
                        text_paint.set_anti_alias(true);
                        let (text_width, _) =
                            ui_font.measure_str(&text, Some(&text_paint));

                        let padding = 10.0;
                        let box_width = text_width + (padding * 2.0);
                        let box_height = 40.0;

                        let box_x = size.width as f32 - box_width - 20.0;
                        let box_y = size.height as f32 - box_height - 20.0;

                        let mut bg_paint = Paint::new(
                            Color4f::from(Color::from_argb(180, 0, 0, 0)),
                            None,
                        );
                        bg_paint.set_anti_alias(true);
                        let bg_rect =
                            Rect::from_xywh(box_x, box_y, box_width, box_height);
                        canvas.draw_rect(bg_rect, &bg_paint);

                        canvas.draw_str(
                            &text,
                            Point::new(box_x + padding, box_y + 28.0),
                            &ui_font,
                            &text_paint,
                        );

                        gr_context.flush_and_submit();

                        // Synchronize before presenting. For a PDF viewer with infrequent
                        // redraws, device_wait_idle is acceptable and simpler than full
                        // semaphore-based synchronization.
                        unsafe {
                            device.device_wait_idle().unwrap();
                        }

                        // Present
                        let swapchains = [swapchain_state.swapchain];
                        let image_indices = [image_index];
                        let present_info = avk::PresentInfoKHR::builder()
                            .swapchains(&swapchains)
                            .image_indices(&image_indices);

                        let present_result = unsafe {
                            swapchain_loader
                                .queue_present(graphics_queue, &present_info)
                        };

                        if let Err(avk::Result::ERROR_OUT_OF_DATE_KHR) = present_result {
                            unsafe { device.device_wait_idle().unwrap() };
                            swapchain_state = create_swapchain(
                                &surface_loader,
                                &swapchain_loader,
                                physical_device,
                                vk_surface,
                                &window,
                                Some(swapchain_state.swapchain),
                            );
                            cached_pdf_image = None;
                            zoom_cache.clear();
                            rendered_zoom = 0.0;
                        }
                    }
                    _ => (),
                },
                _ => (),
            }
        })
        .unwrap();
}

// --- Swapchain Management ---

struct SwapchainState {
    swapchain: avk::SwapchainKHR,
    images: Vec<avk::Image>,
    format: avk::Format,
    extent: avk::Extent2D,
}

fn create_swapchain(
    surface_loader: &khr::Surface,
    swapchain_loader: &khr::Swapchain,
    physical_device: avk::PhysicalDevice,
    surface: avk::SurfaceKHR,
    window: &winit::window::Window,
    old_swapchain: Option<avk::SwapchainKHR>,
) -> SwapchainState {
    let caps = unsafe {
        surface_loader
            .get_physical_device_surface_capabilities(physical_device, surface)
            .unwrap()
    };

    let formats = unsafe {
        surface_loader
            .get_physical_device_surface_formats(physical_device, surface)
            .unwrap()
    };

    let format = formats
        .iter()
        .find(|f| {
            f.format == avk::Format::B8G8R8A8_UNORM
                && f.color_space == avk::ColorSpaceKHR::SRGB_NONLINEAR
        })
        .unwrap_or(&formats[0]);

    let present_modes = unsafe {
        surface_loader
            .get_physical_device_surface_present_modes(physical_device, surface)
            .unwrap()
    };

    let present_mode = if present_modes.contains(&avk::PresentModeKHR::MAILBOX) {
        avk::PresentModeKHR::MAILBOX
    } else {
        avk::PresentModeKHR::FIFO
    };

    let window_size = window.inner_size();
    let extent = if caps.current_extent.width != u32::MAX {
        caps.current_extent
    } else {
        avk::Extent2D {
            width: window_size
                .width
                .clamp(caps.min_image_extent.width, caps.max_image_extent.width),
            height: window_size
                .height
                .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
        }
    };

    let image_count = {
        let desired = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            desired.min(caps.max_image_count)
        } else {
            desired
        }
    };

    let create_info = avk::SwapchainCreateInfoKHR::builder()
        .surface(surface)
        .min_image_count(image_count)
        .image_format(format.format)
        .image_color_space(format.color_space)
        .image_extent(extent)
        .image_array_layers(1)
        .image_usage(
            avk::ImageUsageFlags::COLOR_ATTACHMENT | avk::ImageUsageFlags::TRANSFER_DST,
        )
        .image_sharing_mode(avk::SharingMode::EXCLUSIVE)
        .pre_transform(caps.current_transform)
        .composite_alpha(avk::CompositeAlphaFlagsKHR::OPAQUE)
        .present_mode(present_mode)
        .clipped(true)
        .old_swapchain(old_swapchain.unwrap_or(avk::SwapchainKHR::null()));

    let swapchain = unsafe {
        swapchain_loader
            .create_swapchain(&create_info, None)
            .expect("Failed to create swapchain")
    };

    let images = unsafe {
        swapchain_loader
            .get_swapchain_images(swapchain)
            .expect("Failed to get swapchain images")
    };

    SwapchainState {
        swapchain,
        images,
        format: format.format,
        extent,
    }
}

/// Map Vulkan format (ash) to Skia Vulkan format.
/// Both are #[repr(i32)] enums representing VkFormat values.
fn vk_format_to_skia(format: avk::Format) -> skia_vk::Format {
    // Safety: Both ash::vk::Format and skia_vk::Format are #[repr(i32)]
    // representations of the same Vulkan VkFormat enum values.
    unsafe { std::mem::transmute(format.as_raw()) }
}

/// Adjust pan offset so the point under the mouse cursor stays fixed after a zoom change.
fn zoom_to_cursor(
    old_zoom: f32,
    new_zoom: f32,
    mouse: (f32, f32),
    win_size: (f32, f32),
    pan: &mut (f32, f32),
) {
    let k = new_zoom / old_zoom;
    let cx = win_size.0 / 2.0;
    let cy = win_size.1 / 2.0;
    pan.0 = (1.0 - k) * (mouse.0 - cx) + k * pan.0;
    pan.1 = (1.0 - k) * (mouse.1 - cy) + k * pan.1;
}

fn pdf_path_from_args() -> PathBuf {
    env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("test.pdf"))
}
