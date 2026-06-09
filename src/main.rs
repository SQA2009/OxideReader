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
use winit::window::{CursorIcon, Icon, WindowBuilder};

use raw_window_handle::{HasRawDisplayHandle, HasRawWindowHandle};

use ash::extensions::khr;
use ash::vk::{self as avk, Handle};

use skia_safe::gpu::vk as skia_vk;
use skia_safe::gpu::SurfaceOrigin;
use skia_safe::{
    font::Edging, AlphaType, Color, Color4f, ColorSpace, ColorType, CubicResampler, Data, Font,
    FontHinting, FontMgr, FontStyle, Image, ImageInfo, Paint, PathEffect, Point, Rect,
    SamplingOptions,
};

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{render, RenderCache, RenderSettings};

const MAX_TEXTURE_SIZE: i32 = 16384;
const ZOOM_DEBOUNCE_MS: u64 = 150;
const ZOOM_FACTOR: f32 = 1.10;
/// Multiplier to convert internal zoom_level to displayed zoom percentage.
/// At zoom_level=1.0 the page fits the window width, which corresponds to ~77.4% of the PDF's native size.
const ZOOM_TO_PERCENT: f32 = 77.4;
const MAX_ZOOM_PERCENT: f32 = 6200.0;
const MAX_ZOOM_LEVEL: f32 = MAX_ZOOM_PERCENT / ZOOM_TO_PERCENT;

/// Layout constants
const SIDEBAR_WIDTH: f32 = 200.0;
const PAGE_GAP: f32 = 20.0;
const SCROLL_SPEED: f32 = 60.0;

/// Layout constants for the antialiasing settings menu.
const SETTINGS_MENU_X: f32 = SIDEBAR_WIDTH + 20.0;
const SETTINGS_MENU_Y: f32 = 20.0;
const SETTINGS_MENU_WIDTH: f32 = 280.0;
const SETTINGS_HEADER_HEIGHT: f32 = 40.0;
const SETTINGS_ROW_HEIGHT: f32 = 32.0;
const SETTINGS_NUM_ITEMS: usize = 5;

/// Sidebar zoom textbox layout
const ZOOM_TEXTBOX_X: f32 = 10.0;
const ZOOM_TEXTBOX_Y: f32 = 50.0;
const ZOOM_TEXTBOX_W: f32 = SIDEBAR_WIDTH - 20.0;
const ZOOM_TEXTBOX_H: f32 = 32.0;

#[derive(Clone, Copy, PartialEq)]
enum ToolMode {
    Hand,
    Selection,
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
    let interpreter_settings = InterpreterSettings::default();

    let total_pages = pdf.pages().len();

    // Pre-compute page sizes (width, height in points) for layout
    let page_sizes: Vec<(f32, f32)> = (0..total_pages)
        .map(|i| {
            let page = &pdf.pages()[i];
            page.render_dimensions()
        })
        .collect();

    // 2. Setup Windowing
    // Load app icon from assets/app_icon.ico (place your .ico file in the assets/ directory)
    let window_icon = load_app_icon();

    let event_loop = EventLoop::new().unwrap();
    let window = WindowBuilder::new()
        .with_title("Rust Skia PDF Viewer")
        .with_window_icon(window_icon)
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
    // Per-page cached rendered images
    let mut page_images: Vec<Option<Image>> = vec![None; total_pages as usize];

    let mut zoom_level: f32 = 1.0;
    let mut rendered_zoom: f32 = 0.0;
    let mut pan_offset = (0.0f32, 0.0f32);

    let mut is_dragging = false;
    let mut last_mouse_pos = (0.0f32, 0.0f32);
    // Anchor-point drag: record start position so the point under the cursor stays locked
    let mut drag_start_mouse = (0.0f32, 0.0f32);
    let mut drag_start_pan = (0.0f32, 0.0f32);

    // Debounced zoom state
    let mut last_zoom_time: Option<Instant> = None;

    // Zoom cache: (zoom_key, window_width, window_height, page_index, image, content_rect)
    let mut zoom_cache: Vec<(i32, u32, u32, u16, Image, Rect)> = Vec::new();
    let mut cached_pdf_image: Option<Image> = None;

    // Antialiasing settings
    let mut text_smoothing = false;
    let mut path_smoothing = false;
    let mut image_smoothing = false;
    // Color management: force halftone for higher quality image stretching
    let mut force_halftone = false;
    // RGB subpixel anti-aliasing for improved text readability
    let mut lcd_text_rendering = true;
    let mut show_settings_menu = false;

    let mut current_tool = ToolMode::Hand;
    let mut ctrl_held = false;

    // Sidebar zoom textbox state
    let mut zoom_input_active = false;
    let mut zoom_input_text = String::new();

    window.set_cursor_icon(CursorIcon::Grab);

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

                    WindowEvent::ModifiersChanged(modifiers) => {
                        ctrl_held = modifiers.state().control_key();
                    }

                    // --- INPUT HANDLING ---
                    WindowEvent::MouseInput {
                        state,
                        button: MouseButton::Left,
                        ..
                    } => {
                        let (mx, my) = last_mouse_pos;

                        if state == ElementState::Pressed {
                            // Check sidebar zoom textbox click
                            if mx >= ZOOM_TEXTBOX_X
                                && mx <= ZOOM_TEXTBOX_X + ZOOM_TEXTBOX_W
                                && my >= ZOOM_TEXTBOX_Y
                                && my <= ZOOM_TEXTBOX_Y + ZOOM_TEXTBOX_H
                            {
                                if !zoom_input_active {
                                    zoom_input_active = true;
                                    zoom_input_text =
                                        format!("{:.1}", zoom_level * ZOOM_TO_PERCENT);
                                }
                                window.request_redraw();
                                return;
                            } else if zoom_input_active {
                                // Clicked outside textbox — deactivate
                                zoom_input_active = false;
                                window.request_redraw();
                            }

                            // Check if click is within the settings menu
                            if show_settings_menu {
                                let menu_height = SETTINGS_HEADER_HEIGHT
                                    + SETTINGS_ROW_HEIGHT * SETTINGS_NUM_ITEMS as f32
                                    + 10.0;

                                if mx >= SETTINGS_MENU_X
                                    && mx <= SETTINGS_MENU_X + SETTINGS_MENU_WIDTH
                                    && my >= SETTINGS_MENU_Y
                                    && my <= SETTINGS_MENU_Y + menu_height
                                {
                                    let row_y_start =
                                        SETTINGS_MENU_Y + SETTINGS_HEADER_HEIGHT;
                                    if my >= row_y_start {
                                        let row_index = ((my - row_y_start)
                                            / SETTINGS_ROW_HEIGHT)
                                            as usize;
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
                                            3 => {
                                                force_halftone = !force_halftone;
                                                true
                                            }
                                            4 => {
                                                lcd_text_rendering = !lcd_text_rendering;
                                                true
                                            }
                                            _ => false,
                                        };
                                        if toggled {
                                            match row_index {
                                                0..=3 => {
                                                    page_images
                                                        .iter_mut()
                                                        .for_each(|img| *img = None);
                                                }
                                                4 => {
                                                    let _ = cached_pdf_image.take();
                                                    zoom_cache.clear();
                                                }
                                                _ => {}
                                            }
                                            rendered_zoom = 0.0;
                                            window.request_redraw();
                                        }
                                    }
                                    // Don't start dragging when clicking inside the menu
                                    return;
                                }
                            }

                            // Only start drag in the viewport area (right of sidebar)
                            if current_tool == ToolMode::Hand && mx >= SIDEBAR_WIDTH {
                                is_dragging = true;
                                drag_start_mouse = last_mouse_pos;
                                drag_start_pan = pan_offset;
                                window.set_cursor_icon(CursorIcon::Grabbing);
                            }
                        } else {
                            // Mouse released
                            if is_dragging {
                                is_dragging = false;
                                if current_tool == ToolMode::Hand {
                                    window.set_cursor_icon(CursorIcon::Grab);
                                }
                            }
                        }
                    }

                    WindowEvent::CursorMoved { position, .. } => {
                        let current_x = position.x as f32;
                        let current_y = position.y as f32;

                        if is_dragging {
                            // Anchor-point panning: always compute pan from the drag start
                            // so the point under the cursor stays perfectly locked.
                            pan_offset.0 =
                                drag_start_pan.0 + (current_x - drag_start_mouse.0);
                            pan_offset.1 =
                                drag_start_pan.1 + (current_y - drag_start_mouse.1);
                            window.request_redraw();
                        }
                        last_mouse_pos = (current_x, current_y);
                    }

                    WindowEvent::MouseWheel { delta, .. } => {
                        if ctrl_held {
                            // Ctrl+scroll = zoom
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
                            zoom_level = zoom_level.clamp(0.1, MAX_ZOOM_LEVEL);

                            // Zoom-to-cursor: adjust pan so the point under the
                            // mouse stays fixed after the zoom change.
                            let win = window.inner_size();
                            let viewport_width = win.width as f32 - SIDEBAR_WIDTH;
                            zoom_to_cursor(
                                old_zoom,
                                zoom_level,
                                last_mouse_pos,
                                (SIDEBAR_WIDTH + viewport_width / 2.0, 0.0),
                                &mut pan_offset,
                            );

                            // Debounce: record time, don't invalidate cache yet
                            last_zoom_time = Some(Instant::now());
                            window.request_redraw();
                            target.set_control_flow(ControlFlow::WaitUntil(
                                Instant::now() + Duration::from_millis(ZOOM_DEBOUNCE_MS),
                            ));
                        } else {
                            // Plain scroll = pan vertically
                            let scroll_amount = match delta {
                                MouseScrollDelta::LineDelta(_, y) => y * SCROLL_SPEED,
                                MouseScrollDelta::PixelDelta(pos) => pos.y as f32,
                            };
                            pan_offset.1 += scroll_amount;
                            window.request_redraw();
                        }
                    }

                    WindowEvent::KeyboardInput {
                        event:
                            KeyEvent {
                                logical_key,
                                state: ElementState::Pressed,
                                repeat,
                                ..
                            },
                        ..
                    } => {
                        // Handle zoom textbox input first
                        if zoom_input_active {
                            match &logical_key {
                                Key::Named(NamedKey::Enter) => {
                                    // Apply zoom value
                                    if let Ok(val) = zoom_input_text.parse::<f32>() {
                                        if val > 0.0 {
                                            let new_zoom = val / ZOOM_TO_PERCENT;
                                            zoom_level = new_zoom.clamp(0.1, MAX_ZOOM_LEVEL);
                                            page_images
                                                .iter_mut()
                                                .for_each(|img| *img = None);
                                            rendered_zoom = zoom_level;
                                            last_zoom_time = None;
                                            zoom_input_active = false;
                                        }
                                        // else: invalid value, keep textbox active
                                    }
                                    // Parse failed: keep textbox active so user can fix input
                                    window.request_redraw();
                                    return;
                                }
                                Key::Named(NamedKey::Escape) => {
                                    zoom_input_active = false;
                                    window.request_redraw();
                                    return;
                                }
                                Key::Named(NamedKey::Backspace) => {
                                    zoom_input_text.pop();
                                    window.request_redraw();
                                    return;
                                }
                                Key::Character(c) => {
                                    let c_str = c.as_str();
                                    if c_str
                                        .chars()
                                        .all(|ch| ch.is_ascii_digit() || ch == '.')
                                    {
                                        // Only allow one decimal point
                                        if !c_str.contains('.')
                                            || !zoom_input_text.contains('.')
                                        {
                                            zoom_input_text.push_str(c_str);
                                            window.request_redraw();
                                        }
                                    }
                                    return;
                                }
                                _ => {
                                    return;
                                }
                            }
                        }

                        let mut needs_rerender = false;

                        match logical_key {
                            Key::Named(NamedKey::Alt) if !repeat => {
                                current_tool = match current_tool {
                                    ToolMode::Hand => ToolMode::Selection,
                                    ToolMode::Selection => ToolMode::Hand,
                                };
                                is_dragging = false;
                                match current_tool {
                                    ToolMode::Hand => {
                                        window.set_cursor_icon(CursorIcon::Grab)
                                    }
                                    ToolMode::Selection => {
                                        window.set_cursor_icon(CursorIcon::Default)
                                    }
                                }
                                window.request_redraw();
                            }
                            Key::Character(c) => match c.as_str() {
                                "+" | "=" => {
                                    let old_zoom = zoom_level;
                                    zoom_level *= ZOOM_FACTOR;
                                    zoom_level = zoom_level.clamp(0.1, MAX_ZOOM_LEVEL);
                                    let win = window.inner_size();
                                    let viewport_width = win.width as f32 - SIDEBAR_WIDTH;
                                    zoom_to_cursor(
                                        old_zoom,
                                        zoom_level,
                                        last_mouse_pos,
                                        (SIDEBAR_WIDTH + viewport_width / 2.0, 0.0),
                                        &mut pan_offset,
                                    );
                                    needs_rerender = true;
                                }
                                "-" => {
                                    let old_zoom = zoom_level;
                                    zoom_level /= ZOOM_FACTOR;
                                    zoom_level = zoom_level.clamp(0.1, MAX_ZOOM_LEVEL);
                                    let win = window.inner_size();
                                    let viewport_width = win.width as f32 - SIDEBAR_WIDTH;
                                    zoom_to_cursor(
                                        old_zoom,
                                        zoom_level,
                                        last_mouse_pos,
                                        (SIDEBAR_WIDTH + viewport_width / 2.0, 0.0),
                                        &mut pan_offset,
                                    );
                                    needs_rerender = true;
                                }
                                "0" => {
                                    zoom_level = 1.0;
                                    pan_offset = (0.0, 0.0);
                                    page_images.iter_mut().for_each(|img| *img = None);
                                    rendered_zoom = zoom_level;
                                    last_zoom_time = None;
                                    window.request_redraw();
                                }
                                "s" | "S" => {
                                    show_settings_menu = !show_settings_menu;
                                    window.request_redraw();
                                }
                                "1" => {
                                    if show_settings_menu {
                                        text_smoothing = !text_smoothing;
                                        page_images.iter_mut().for_each(|img| *img = None);
                                        rendered_zoom = 0.0;
                                        window.request_redraw();
                                    }
                                }
                                "2" => {
                                    if show_settings_menu {
                                        path_smoothing = !path_smoothing;
                                        page_images.iter_mut().for_each(|img| *img = None);
                                        rendered_zoom = 0.0;
                                        window.request_redraw();
                                    }
                                }
                                "3" => {
                                    if show_settings_menu {
                                        image_smoothing = !image_smoothing;
                                        page_images.iter_mut().for_each(|img| *img = None);
                                        rendered_zoom = 0.0;
                                        window.request_redraw();
                                    }
                                }
                                "4" => {
                                    if show_settings_menu {
                                        force_halftone = !force_halftone;
                                        page_images.iter_mut().for_each(|img| *img = None);
                                        rendered_zoom = 0.0;
                                        window.request_redraw();
                                    }
                                }
                                "5" => {
                                    if show_settings_menu {
                                        lcd_text_rendering = !lcd_text_rendering;
                                        let _ = cached_pdf_image.take();
                                        zoom_cache.clear();
                                        rendered_zoom = 0.0;
                                        window.request_redraw();
                                    }
                                }
                                _ => {}
                            },
                            _ => {}
                        }

                        if needs_rerender {
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
                            page_images.iter_mut().for_each(|img| *img = None);
                            rendered_zoom = 0.0;
                        }
                        window.request_redraw();
                    }

                    WindowEvent::RedrawRequested => {
                        let size = window.inner_size();
                        if size.width == 0 || size.height == 0 {
                            return;
                        }

                        let viewport_width =
                            (size.width as f32 - SIDEBAR_WIDTH).max(1.0);
                        let viewport_height = size.height as f32;

                        // --- DEBOUNCED ZOOM LOGIC ---
                        if zoom_level != rendered_zoom {
                            if let Some(last_time) = last_zoom_time {
                                if last_time.elapsed()
                                    >= Duration::from_millis(ZOOM_DEBOUNCE_MS)
                                {
                                    // Debounce settled - clear caches and re-render
                                    page_images
                                        .iter_mut()
                                        .for_each(|img| *img = None);
                                    rendered_zoom = zoom_level;
                                    last_zoom_time = None;
                                } else {
                                    target.set_control_flow(ControlFlow::WaitUntil(
                                        last_time
                                            + Duration::from_millis(ZOOM_DEBOUNCE_MS),
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
                                unsafe { device.device_wait_idle().unwrap() };
                                swapchain_state = create_swapchain(
                                    &surface_loader,
                                    &swapchain_loader,
                                    physical_device,
                                    vk_surface,
                                    &window,
                                    Some(swapchain_state.swapchain),
                                );
                                page_images
                                    .iter_mut()
                                    .for_each(|img| *img = None);
                                rendered_zoom = 0.0;
                                window.request_redraw();
                                return;
                            }
                            Err(e) => {
                                panic!("Failed to acquire swapchain image: {:?}", e)
                            }
                        };

                        let swapchain_image =
                            swapchain_state.images[image_index as usize];

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
                                ColorSpace::new_srgb(),
                                None,
                            )
                            .expect(
                                "Failed to create Skia surface from Vulkan render target",
                            );

                        // --- COMPUTE PAGE LAYOUTS ---
                        // Each page is fit to viewport_width at zoom_level.
                        // page_layouts: (y_offset, display_width, display_height)
                        let mut page_layouts: Vec<(f32, f32, f32)> = Vec::new();
                        {
                            let mut y_cursor = 0.0f32;
                            for &(pw, ph) in &page_sizes {
                                let aspect = pw / ph;
                                let display_width = viewport_width * zoom_level;
                                let display_height = display_width / aspect;
                                page_layouts.push((y_cursor, display_width, display_height));
                                y_cursor += display_height + PAGE_GAP;
                            }
                        }

                        // --- RENDER VISIBLE PAGES ---
                        // Only render pages when not in the middle of a zoom debounce
                        let is_debouncing = last_zoom_time.is_some();
                        if !is_debouncing {
                            let pages = pdf.pages();
                            let render_cache = RenderCache::new();
                            for (i, &(page_y, _, _)) in
                                page_layouts.iter().enumerate()
                            {
                                let (pw, ph) = page_sizes[i];
                                let aspect = pw / ph;
                                let render_width =
                                    (viewport_width * zoom_level).round() as i32;
                                let render_height =
                                    (render_width as f32 / aspect).round() as i32;

                                // Check visibility
                                let screen_y = page_y + pan_offset.1;
                                let screen_bottom = screen_y + render_height as f32;
                                if screen_bottom < -100.0
                                    || screen_y > viewport_height + 100.0
                                {
                                    continue;
                                }

                                if page_images[i].is_some() {
                                    continue;
                                }

                                let mut texture_width = render_width;
                                let mut texture_height = render_height;

                                if texture_width > MAX_TEXTURE_SIZE
                                    || texture_height > MAX_TEXTURE_SIZE
                                {
                                    let scale =
                                        if texture_width > texture_height {
                                            MAX_TEXTURE_SIZE as f32
                                                / texture_width as f32
                                        } else {
                                            MAX_TEXTURE_SIZE as f32
                                                / texture_height as f32
                                        };
                                    texture_width =
                                        (texture_width as f32 * scale) as i32;
                                    texture_height =
                                        (texture_height as f32 * scale) as i32;
                                }

                                if texture_width <= 0 || texture_height <= 0 {
                                    continue;
                                }

                                let page = &pages[i];
                                let scale_x = texture_width as f32 / pw;
                                let scale_y = texture_height as f32 / ph;
                                let target_width =
                                    u16::try_from(texture_width).expect(
                                        "Render width must fit in u16",
                                    );
                                let target_height =
                                    u16::try_from(texture_height).expect(
                                        "Render height must fit in u16",
                                    );
                                let render_settings = RenderSettings {
                                    x_scale: scale_x,
                                    y_scale: scale_y,
                                    width: Some(target_width),
                                    height: Some(target_height),
                                    bg_color: WHITE,
                                };
                                let pixmap = render(
                                    page,
                                    &render_cache,
                                    &interpreter_settings,
                                    &render_settings,
                                );

                                let image_info = ImageInfo::new(
                                    (texture_width, texture_height),
                                    // hayro pixmaps are premultiplied RGBA8.
                                    ColorType::RGBA8888,
                                    AlphaType::Premul,
                                    ColorSpace::new_srgb(),
                                );

                                let raw_bytes = pixmap.data_as_u8_slice();
                                let row_bytes = texture_width as usize * 4;
                                let data = Data::new_copy(raw_bytes);
                                let raster_image = skia_safe::images::raster_from_data(
                                    &image_info, data, row_bytes,
                                );
                                let gpu_image = raster_image.as_ref().and_then(|image| {
                                    skia_safe::gpu::images::texture_from_image(
                                        &mut gr_context,
                                        image,
                                        skia_safe::gpu::Mipmapped::No,
                                        skia_safe::gpu::Budgeted::Yes,
                                    )
                                });
                                page_images[i] = gpu_image.or(raster_image);
                            }

                            // Sync rendered_zoom
                            rendered_zoom = zoom_level;
                        }

                        // --- DRAW ---
                        let canvas = skia_surface.canvas();
                        canvas.clear(Color::from_rgb(30, 30, 30));

                        // Clip to viewport area (right of sidebar)
                        canvas.save();
                        canvas.clip_rect(
                            Rect::from_xywh(
                                SIDEBAR_WIDTH,
                                0.0,
                                viewport_width,
                                viewport_height,
                            ),
                            None,
                            false,
                        );

                        // Draw pages
                        let sampling = if image_smoothing {
                            SamplingOptions::from(CubicResampler::catmull_rom())
                        } else {
                            SamplingOptions::default()
                        };
                        let paint = Paint::default();

                        for (i, &(page_y, page_w, page_h)) in
                            page_layouts.iter().enumerate()
                        {
                            let screen_x = SIDEBAR_WIDTH
                                + (viewport_width - page_w) / 2.0
                                + pan_offset.0;
                            let screen_y = page_y + pan_offset.1;

                            // Skip if not visible
                            if screen_y + page_h < 0.0
                                || screen_y > viewport_height
                            {
                                continue;
                            }

                            let display_rect = Rect::from_xywh(
                                screen_x, screen_y, page_w, page_h,
                            );

                            if let Some(image) = &page_images[i] {
                                canvas
                                    .draw_image_rect_with_sampling_options(
                                        image,
                                        None,
                                        display_rect,
                                        sampling,
                                        &paint,
                                    );
                            }
                        }

                        // Draw dashed separators between pages
                        let intervals = [8.0f32, 6.0];
                        if let Some(dash_effect) =
                            PathEffect::dash(&intervals, 0.0)
                        {
                            let mut dash_paint = Paint::new(
                                Color4f::from(Color::from_argb(
                                    150, 180, 180, 180,
                                )),
                                None,
                            );
                            dash_paint
                                .set_style(skia_safe::PaintStyle::Stroke);
                            dash_paint.set_stroke_width(1.0);
                            dash_paint.set_path_effect(dash_effect);

                            for i in 0..page_layouts.len().saturating_sub(1) {
                                let (pg_y, _, pg_h) = page_layouts[i];
                                let line_y =
                                    pg_y + pg_h + PAGE_GAP / 2.0 + pan_offset.1;
                                if line_y >= 0.0 && line_y <= viewport_height {
                                    canvas.draw_line(
                                        Point::new(SIDEBAR_WIDTH + 10.0, line_y),
                                        Point::new(
                                            size.width as f32 - 10.0,
                                            line_y,
                                        ),
                                        &dash_paint,
                                    );
                                }
                            }
                        }

                        canvas.restore(); // Restore viewport clip

                        // --- DRAW SIDEBAR ---
                        let mut sidebar_bg = Paint::new(
                            Color4f::from(Color::from_argb(240, 35, 35, 35)),
                            None,
                        );
                        sidebar_bg.set_anti_alias(true);
                        canvas.draw_rect(
                            Rect::from_xywh(
                                0.0,
                                0.0,
                                SIDEBAR_WIDTH,
                                viewport_height,
                            ),
                            &sidebar_bg,
                        );

                        // Sidebar separator line
                        let mut sidebar_sep = Paint::new(
                            Color4f::from(Color::from_argb(100, 80, 80, 80)),
                            None,
                        );
                        sidebar_sep.set_style(skia_safe::PaintStyle::Stroke);
                        sidebar_sep.set_stroke_width(1.0);
                        canvas.draw_line(
                            Point::new(SIDEBAR_WIDTH, 0.0),
                            Point::new(SIDEBAR_WIDTH, viewport_height),
                            &sidebar_sep,
                        );

                        // Zoom label
                        let small_font =
                            Font::from_typeface(ui_font.typeface(), 16.0);
                        let mut white_paint =
                            Paint::new(Color4f::from(Color::WHITE), None);
                        white_paint.set_anti_alias(true);
                        canvas.draw_str(
                            "Zoom",
                            Point::new(15.0, 40.0),
                            &small_font,
                            &white_paint,
                        );

                        // Zoom textbox
                        let textbox_rect = Rect::from_xywh(
                            ZOOM_TEXTBOX_X,
                            ZOOM_TEXTBOX_Y,
                            ZOOM_TEXTBOX_W,
                            ZOOM_TEXTBOX_H,
                        );
                        let textbox_bg_color = if zoom_input_active {
                            Color::from_rgb(55, 55, 55)
                        } else {
                            Color::from_rgb(45, 45, 45)
                        };
                        let textbox_bg = Paint::new(
                            Color4f::from(textbox_bg_color),
                            None,
                        );
                        canvas.draw_rect(textbox_rect, &textbox_bg);

                        let border_color = if zoom_input_active {
                            Color::from_rgb(100, 150, 255)
                        } else {
                            Color::from_rgb(80, 80, 80)
                        };
                        let mut border_paint = Paint::new(
                            Color4f::from(border_color),
                            None,
                        );
                        border_paint
                            .set_style(skia_safe::PaintStyle::Stroke);
                        border_paint.set_stroke_width(1.0);
                        border_paint.set_anti_alias(true);
                        canvas.draw_rect(textbox_rect, &border_paint);

                        let zoom_display_text = if zoom_input_active {
                            format!("{}%", zoom_input_text)
                        } else {
                            format!(
                                "{:.1}%",
                                zoom_level * ZOOM_TO_PERCENT
                            )
                        };
                        canvas.draw_str(
                            &zoom_display_text,
                            Point::new(
                                ZOOM_TEXTBOX_X + 8.0,
                                ZOOM_TEXTBOX_Y + 22.0,
                            ),
                            &small_font,
                            &white_paint,
                        );

                        // Tool mode indicator in sidebar
                        let tool_text = match current_tool {
                            ToolMode::Hand => "Hand Tool",
                            ToolMode::Selection => "Select Tool",
                        };
                        canvas.draw_str(
                            tool_text,
                            Point::new(15.0, 110.0),
                            &small_font,
                            &white_paint,
                        );

                        let shortcut_font =
                            Font::from_typeface(ui_font.typeface(), 12.0);
                        let mut dim_paint = Paint::new(
                            Color4f::from(Color::from_argb(
                                150, 180, 180, 180,
                            )),
                            None,
                        );
                        dim_paint.set_anti_alias(true);
                        canvas.draw_str(
                            "(Alt to switch)",
                            Point::new(15.0, 128.0),
                            &shortcut_font,
                            &dim_paint,
                        );

                        // Page count in sidebar
                        let page_info =
                            format!("{} pages", total_pages);
                        canvas.draw_str(
                            &page_info,
                            Point::new(15.0, 160.0),
                            &small_font,
                            &dim_paint,
                        );

                        // Scroll hint
                        canvas.draw_str(
                            "Scroll: navigate",
                            Point::new(15.0, 190.0),
                            &shortcut_font,
                            &dim_paint,
                        );
                        canvas.draw_str(
                            "Ctrl+Scroll: zoom",
                            Point::new(15.0, 208.0),
                            &shortcut_font,
                            &dim_paint,
                        );

                        // --- DRAW SETTINGS MENU ---
                        if show_settings_menu {
                            let items: [(&str, bool); SETTINGS_NUM_ITEMS] = [
                                ("1  Text Smoothing", text_smoothing),
                                ("2  Path Smoothing", path_smoothing),
                                ("3  Image Smoothing", image_smoothing),
                                ("4  Force Halftone", force_halftone),
                                ("5  Subpixel Text", lcd_text_rendering),
                            ];
                            let menu_height = SETTINGS_HEADER_HEIGHT
                                + SETTINGS_ROW_HEIGHT * items.len() as f32
                                + 10.0;

                            // Background
                            let mut menu_bg = Paint::new(
                                Color4f::from(Color::from_argb(
                                    210, 25, 25, 25,
                                )),
                                None,
                            );
                            menu_bg.set_anti_alias(true);
                            canvas.draw_rect(
                                Rect::from_xywh(
                                    SETTINGS_MENU_X,
                                    SETTINGS_MENU_Y,
                                    SETTINGS_MENU_WIDTH,
                                    menu_height,
                                ),
                                &menu_bg,
                            );

                            // Header
                            let mut header_paint = Paint::new(
                                Color4f::from(Color::WHITE),
                                None,
                            );
                            header_paint.set_anti_alias(true);
                            canvas.draw_str(
                                "Render Settings",
                                Point::new(
                                    SETTINGS_MENU_X + 10.0,
                                    SETTINGS_MENU_Y + 28.0,
                                ),
                                &ui_font,
                                &header_paint,
                            );

                            // Separator line
                            let mut menu_sep = Paint::new(
                                Color4f::from(Color::from_argb(
                                    100, 255, 255, 255,
                                )),
                                None,
                            );
                            menu_sep.set_anti_alias(true);
                            menu_sep.set_stroke_width(1.0);
                            menu_sep
                                .set_style(skia_safe::PaintStyle::Stroke);
                            canvas.draw_line(
                                Point::new(
                                    SETTINGS_MENU_X + 10.0,
                                    SETTINGS_MENU_Y + SETTINGS_HEADER_HEIGHT,
                                ),
                                Point::new(
                                    SETTINGS_MENU_X + SETTINGS_MENU_WIDTH
                                        - 10.0,
                                    SETTINGS_MENU_Y + SETTINGS_HEADER_HEIGHT,
                                ),
                                &menu_sep,
                            );

                            // Items
                            let menu_item_font = Font::from_typeface(
                                ui_font.typeface(),
                                18.0,
                            );
                            for (i, (label, enabled)) in
                                items.iter().enumerate()
                            {
                                let iy = SETTINGS_MENU_Y
                                    + SETTINGS_HEADER_HEIGHT
                                    + SETTINGS_ROW_HEIGHT * i as f32
                                    + 24.0;
                                let status = if *enabled {
                                    "  ON"
                                } else {
                                    "  OFF"
                                };
                                let item_text =
                                    format!("{}{}", label, status);

                                let color = if *enabled {
                                    Color::from_rgb(100, 220, 100)
                                } else {
                                    Color::from_rgb(180, 80, 80)
                                };
                                let mut item_paint = Paint::new(
                                    Color4f::from(color),
                                    None,
                                );
                                item_paint.set_anti_alias(true);
                                canvas.draw_str(
                                    &item_text,
                                    Point::new(SETTINGS_MENU_X + 14.0, iy),
                                    &menu_item_font,
                                    &item_paint,
                                );
                            }
                        }

                        gr_context.flush_and_submit();

                        // Synchronize before presenting.
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

                        if let Err(avk::Result::ERROR_OUT_OF_DATE_KHR) =
                            present_result
                        {
                            unsafe { device.device_wait_idle().unwrap() };
                            swapchain_state = create_swapchain(
                                &surface_loader,
                                &swapchain_loader,
                                physical_device,
                                vk_surface,
                                &window,
                                Some(swapchain_state.swapchain),
                            );
                            page_images
                                .iter_mut()
                                .for_each(|img| *img = None);
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
/// `center` is the screen-space anchor: for horizontal, the center of the viewport;
/// for vertical, 0.0 since the document starts at the top.
fn zoom_to_cursor(
    old_zoom: f32,
    new_zoom: f32,
    mouse: (f32, f32),
    center: (f32, f32),
    pan: &mut (f32, f32),
) {
    let k = new_zoom / old_zoom;
    pan.0 = (1.0 - k) * (mouse.0 - center.0) + k * pan.0;
    pan.1 = (1.0 - k) * (mouse.1 - center.1) + k * pan.1;
}

fn pdf_path_from_args() -> PathBuf {
    env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("test.pdf"))
}

/// Load the application icon from `assets/app_icon.ico`.
///
/// Place your `.ico` file at `assets/app_icon.ico` relative to the executable
/// or the project root. Returns `None` if the icon file is not found or cannot
/// be decoded, allowing the application to fall back to the OS default icon.
fn load_app_icon() -> Option<Icon> {
    let icon_paths = [
        // Relative to the working directory (project root)
        PathBuf::from("assets/app_icon.ico"),
        // Relative to the executable location
        env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("assets/app_icon.ico")))
            .unwrap_or_default(),
    ];

    for icon_path in &icon_paths {
        if !icon_path.exists() {
            continue;
        }

        match image::open(icon_path) {
            Ok(img) => {
                let rgba = img.into_rgba8();
                let (width, height) = (rgba.width(), rgba.height());
                match Icon::from_rgba(rgba.into_raw(), width, height) {
                    Ok(icon) => return Some(icon),
                    Err(e) => {
                        eprintln!("Warning: Failed to create window icon: {}", e);
                    }
                }
            }
            Err(e) => {
                eprintln!("Warning: Failed to load icon from {}: {}", icon_path.display(), e);
            }
        }
    }

    None
}
