use std::env;
use std::ffi::CString;
use std::num::NonZeroU32;
use std::path::PathBuf;

use winit::event::{Event, WindowEvent, ElementState, KeyEvent, MouseButton, MouseScrollDelta};
use winit::event_loop::{EventLoop, ControlFlow};
use winit::window::WindowBuilder;
use winit::keyboard::{Key, NamedKey};

use glutin::config::{ConfigTemplateBuilder, GlConfig};
use glutin::context::{ContextAttributesBuilder, NotCurrentGlContext};
use glutin::display::GetGlDisplay;
use glutin::prelude::*;
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::HasRawWindowHandle;

use skia_safe::gpu::{gl::FramebufferInfo, SurfaceOrigin};
use skia_safe::{
    Color, Color4f, ColorType, Surface, Image, Data, AlphaType, ImageInfo, Rect, Paint, 
    Font, FontStyle, FontMgr, Point
};

use pdfium_render::prelude::*;

const MAX_TEXTURE_SIZE: i32 = 12000; 

fn main() {
    // 1. Initialize PDFium
    let pdfium_bindings = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path("./"))
        .or_else(|_| Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path("./libs")))
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
    let window_builder = WindowBuilder::new().with_title("Rust Skia PDF Viewer");
    let template = ConfigTemplateBuilder::new().with_alpha_size(8).with_transparency(true);
    let display_builder = DisplayBuilder::new().with_window_builder(Some(window_builder));

    let (window, gl_config) = display_builder
        .build(&event_loop, template, |configs| {
            configs.reduce(|accum, config| {
                if config.num_samples() > accum.num_samples() { config } else { accum }
            }).unwrap()
        })
        .unwrap();

    let window = window.unwrap();
    let raw_window_handle = window.raw_window_handle();
    let gl_display = gl_config.display();

    // 2b. Create GL Surface
    let attrs = window.build_surface_attributes(Default::default());
    let gl_surface = unsafe {
        gl_display.create_window_surface(&gl_config, &attrs).unwrap()
    };

    // 2c. Create Context
    let context_attributes = ContextAttributesBuilder::new().build(Some(raw_window_handle));
    let not_current_gl_context = unsafe {
        gl_display.create_context(&gl_config, &context_attributes).expect("failed to create context")
    };
    
    let gl_context = not_current_gl_context.make_current(&gl_surface).expect("failed to make context current");

    gl::load_with(|symbol| {
        let symbol = CString::new(symbol).unwrap();
        gl_display.get_proc_address(&symbol).cast()
    });

    // 3. Initialize Skia
    let interface = skia_safe::gpu::gl::Interface::new_native()
        .expect("Failed to create native Skia interface");
    
    let mut gr_context = skia_safe::gpu::direct_contexts::make_gl(interface, None)
        .expect("Failed to create Skia DirectContext");

    // 4. Initialize Font for UI
    let font_mgr = FontMgr::default();
    let typeface = font_mgr.match_family_style("Arial", FontStyle::normal())
        .or_else(|| font_mgr.match_family_style("Segoe UI", FontStyle::normal())) 
        .or_else(|| font_mgr.match_family_style("", FontStyle::normal())) // System Default 
        .expect("Failed to load any system font.");
    
    let ui_font = Font::from_typeface(typeface, 24.0);

    // --- APP STATE ---
    let mut surface: Option<Surface> = None;
    let mut cached_pdf_image: Option<Image> = None;
    let mut current_page_index: u16 = 0;
    
    let mut zoom_level: f32 = 1.0; 
    let mut pan_offset = (0.0f32, 0.0f32);
    let mut content_rect = Rect::default(); 
    
    let mut is_dragging = false;
    let mut last_mouse_pos = (0.0f32, 0.0f32);

    // 5. Run Loop
    event_loop.run(move |event, target| {
        target.set_control_flow(ControlFlow::Wait);

        match event {
            Event::LoopExiting => {
                gr_context.abandon();
            }
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => target.exit(),
                
                // --- INPUT HANDLING ---
                WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                    is_dragging = state == ElementState::Pressed;
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

                    let zoom_factor = 1.1;
                    if scroll_y > 0.0 {
                        zoom_level *= zoom_factor;
                    } else if scroll_y < 0.0 {
                        zoom_level /= zoom_factor;
                    }
                    if zoom_level < 0.1 { zoom_level = 0.1; }

                    cached_pdf_image = None; 
                    window.request_redraw();
                }

                WindowEvent::KeyboardInput { event: KeyEvent { logical_key, state: ElementState::Pressed, .. }, .. } => {
                    let mut needs_rerender = false;

                    match logical_key {
                        Key::Named(NamedKey::ArrowRight) => {
                            if current_page_index < total_pages as u16 - 1 {
                                current_page_index += 1;
                                pan_offset = (0.0, 0.0);
                                needs_rerender = true; 
                            }
                        }
                        Key::Named(NamedKey::ArrowLeft) => {
                            if current_page_index > 0 {
                                current_page_index -= 1;
                                pan_offset = (0.0, 0.0);
                                needs_rerender = true;
                            }
                        }
                        Key::Character(c) => match c.as_str() {
                            "+" | "=" => { zoom_level *= 1.1; needs_rerender = true; }
                            "-" => { 
                                zoom_level /= 1.1; 
                                if zoom_level < 0.1 { zoom_level = 0.1; }
                                needs_rerender = true; 
                            }
                            "0" => { zoom_level = 1.0; pan_offset = (0.0, 0.0); needs_rerender = true; }
                            _ => {}
                        }
                        _ => {}
                    }

                    if needs_rerender {
                        cached_pdf_image = None;
                        window.request_redraw();
                    }
                }

                WindowEvent::Resized(physical_size) => {
                    surface = None;
                    cached_pdf_image = None;
                    if physical_size.width > 0 && physical_size.height > 0 {
                        gl_surface.resize(&gl_context, NonZeroU32::new(physical_size.width).unwrap(), NonZeroU32::new(physical_size.height).unwrap());
                    }
                    window.request_redraw();
                }
                
                WindowEvent::RedrawRequested => {
                    let size = window.inner_size();
                    if size.width == 0 || size.height == 0 { return; }

                    if surface.is_none() {
                        let fb_info = FramebufferInfo {
                            fboid: 0, 
                            format: skia_safe::gpu::gl::Format::RGBA8.into(),
                            protected: skia_safe::gpu::Protected::No,
                        };
                        let backend_render_target = skia_safe::gpu::backend_render_targets::make_gl(
                            (size.width as i32, size.height as i32),
                            None,
                            8,
                            fb_info,
                        );
                        
                        surface = skia_safe::gpu::surfaces::wrap_backend_render_target(
                            &mut gr_context,
                            &backend_render_target,
                            SurfaceOrigin::BottomLeft,
                            ColorType::RGBA8888, 
                            None,
                            None,
                        );
                    }

                    // --- RENDER PDF ---
                    if cached_pdf_image.is_none() {
                        let page = document.pages().get(current_page_index).expect("Error loading page");
                        
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

                        if texture_width > MAX_TEXTURE_SIZE || texture_height > MAX_TEXTURE_SIZE {
                            let scale = if texture_width > texture_height {
                                MAX_TEXTURE_SIZE as f32 / texture_width as f32
                            } else {
                                MAX_TEXTURE_SIZE as f32 / texture_height as f32
                            };
                            texture_width = (texture_width as f32 * scale) as i32;
                            texture_height = (texture_height as f32 * scale) as i32;
                        }

                        let render_config = PdfRenderConfig::new()
                            .set_target_width(texture_width)
                            .set_target_height(texture_height)
                            .set_format(PdfBitmapFormat::BGRA);

                        let bitmap = page.render_with_config(&render_config).expect("Failed to render page");

                        let image_info = ImageInfo::new(
                            (texture_width, texture_height),
                            ColorType::RGBA8888, 
                            AlphaType::Premul,
                            None,
                        );
                        
                        let data = Data::new_copy(&bitmap.as_raw_bytes());
                        let row_bytes = texture_width as usize * 4;
                        
                        cached_pdf_image = skia_safe::images::raster_from_data(&image_info, data, row_bytes);

                        let x = (size.width as f32 - final_width as f32) / 2.0;
                        let y = (size.height as f32 - final_height as f32) / 2.0;
                        
                        content_rect = Rect::from_xywh(x, y, final_width as f32, final_height as f32);
                    }

                    if let Some(surface) = &mut surface {
                        let canvas = surface.canvas();
                        canvas.clear(Color::from_rgb(30, 30, 30));

                        if let Some(image) = &cached_pdf_image {
                            let paint = Paint::default();
                            
                            let display_rect = Rect::from_xywh(
                                content_rect.x() + pan_offset.0,
                                content_rect.y() + pan_offset.1,
                                content_rect.width(),
                                content_rect.height()
                            );

                            canvas.draw_image_rect(image, None, display_rect, &paint);
                        }

                        // --- DRAW ZOOM PERCENTAGE ---
                        let real_zoom = zoom_level * 77.4;
                        let text = format!("{:.1}%", real_zoom);
                        
                        // FIX 1: Convert Color to Color4f for Paint
                        let text_paint = Paint::new(Color4f::from(Color::WHITE), None);
                        let (text_width, _) = ui_font.measure_str(&text, Some(&text_paint));
                        
                        let padding = 10.0;
                        let box_width = text_width + (padding * 2.0);
                        let box_height = 40.0;

                        // Position Bottom Right
                        let box_x = size.width as f32 - box_width - 20.0;
                        let box_y = size.height as f32 - box_height - 20.0;

                        // FIX 2: Convert Color to Color4f for Background Paint
                        let bg_paint = Paint::new(Color4f::from(Color::from_argb(180, 0, 0, 0)), None);
                        let bg_rect = Rect::from_xywh(box_x, box_y, box_width, box_height);
                        canvas.draw_rect(bg_rect, &bg_paint);

                        // Draw Text
                        canvas.draw_str(
                            &text, 
                            Point::new(box_x + padding, box_y + 28.0), 
                            &ui_font, 
                            &text_paint
                        );
                        
                        gr_context.flush_and_submit();
                        gl_surface.swap_buffers(&gl_context).unwrap();
                    }
                }
                _ => (),
            },
            _ => (),
        }
    }).unwrap();
}

fn pdf_path_from_args() -> PathBuf {
    env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("test.pdf"))
}
