/// Direct2D-based GPU transition renderer.
///
/// Supported styles: Crossfade, SlideLeft, SlideRight, WipeLeft, WipeRight, Dissolve,
/// ZoomIn, ZoomOut.
///
/// Init sequence:
///   D2D1CreateFactory → CreateHwndRenderTarget → upload bitmaps → render loop
///
/// Falls back gracefully if D2D1 is unavailable.
use anyhow::{Context, Result};
use std::time::{Duration, Instant};

use super::{scale_to_cover, DecodedImage, Rect, TransitionStyle};

const FRAME_INTERVAL_MS: u64 = 17; // ~60 fps

/// Returns true if GPU (Direct2D) is likely available on this system.
pub fn is_available() -> bool {
    #[cfg(target_os = "windows")]
    {
        // Attempt to create a D2D factory as a probe.
        use windows::Win32::Graphics::Direct2D::{
            D2D1CreateFactory, ID2D1Factory, D2D1_FACTORY_TYPE_SINGLE_THREADED,
        };
        unsafe {
            let result: Result<ID2D1Factory, _> =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None);
            result.is_ok()
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}
pub fn run_transition(
    monitor_bounds: Rect,
    old: &DecodedImage,
    new: &DecodedImage,
    style: &TransitionStyle,
    duration_ms: u32,
) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        run_d2d(monitor_bounds, old, new, style, duration_ms)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (monitor_bounds, old, new, style, duration_ms);
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn run_d2d(
    monitor_bounds: Rect,
    old: &DecodedImage,
    new: &DecodedImage,
    style: &TransitionStyle,
    duration_ms: u32,
) -> Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HINSTANCE, HWND};
    use windows::Win32::Graphics::Direct2D::Common::{
        D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
        D2D_RECT_F, D2D_SIZE_U,
    };
    use windows::Win32::Graphics::Direct2D::{
        D2D1CreateFactory, ID2D1Bitmap, ID2D1Factory, ID2D1HwndRenderTarget,
        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES,
        D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
        D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_IMMEDIATELY,
        D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
        D2D1_RENDER_TARGET_USAGE_NONE,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, RegisterClassExW,
        TranslateMessage, CS_HREDRAW, CS_VREDRAW, MSG, PM_REMOVE, WNDCLASSEXW, WS_EX_LAYERED,
        WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    unsafe {
        // ----------------------------------------------------------------
        // Create D2D factory
        // ----------------------------------------------------------------
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
            .context("D2D1CreateFactory failed")?;

        // ----------------------------------------------------------------
        // Register + create a fullscreen topmost HWND
        // ----------------------------------------------------------------
        let class_name: Vec<u16> = "AuroraTransitionD2D\0".encode_utf16().collect();
        let instance = HINSTANCE::default();

        let wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(d2d_wnd_proc),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hInstance: instance,
            ..Default::default()
        };
        let _ = RegisterClassExW(&wnd_class); // ignore already-registered

        let hwnd: HWND = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TRANSPARENT,
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            monitor_bounds.x,
            monitor_bounds.y,
            monitor_bounds.width as i32,
            monitor_bounds.height as i32,
            None,
            None,
            instance,
            None,
        )
        .context("CreateWindowExW failed for D2D transition")?;

        // ----------------------------------------------------------------
        // Create HwndRenderTarget
        // ----------------------------------------------------------------
        let pixel_format = D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        };
        let rt_props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: pixel_format,
            dpiX: 0.0,
            dpiY: 0.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U {
                width: monitor_bounds.width,
                height: monitor_bounds.height,
            },
            presentOptions: D2D1_PRESENT_OPTIONS_IMMEDIATELY,
        };

        let rt: ID2D1HwndRenderTarget = factory
            .CreateHwndRenderTarget(&rt_props, &hwnd_props)
            .context("CreateHwndRenderTarget failed")?;

        // ----------------------------------------------------------------
        // Upload bitmaps
        // ----------------------------------------------------------------
        let old_scaled = scale_to_cover(
            &old.data,
            old.width,
            old.height,
            monitor_bounds.width,
            monitor_bounds.height,
        );
        let new_scaled = scale_to_cover(
            &new.data,
            new.width,
            new.height,
            monitor_bounds.width,
            monitor_bounds.height,
        );

        let bmp_props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        let bmp_size = D2D_SIZE_U {
            width: monitor_bounds.width,
            height: monitor_bounds.height,
        };
        let pitch = monitor_bounds.width * 4;

        let bmp_old: ID2D1Bitmap = rt
            .CreateBitmap(
                bmp_size,
                Some(old_scaled.as_ptr() as *const _),
                pitch,
                &bmp_props,
            )
            .context("CreateBitmap (old) failed")?;
        let bmp_new: ID2D1Bitmap = rt
            .CreateBitmap(
                bmp_size,
                Some(new_scaled.as_ptr() as *const _),
                pitch,
                &bmp_props,
            )
            .context("CreateBitmap (new) failed")?;

        // Dissolve mask (generated once before render loop)
        let n_pixels = (monitor_bounds.width * monitor_bounds.height) as usize;
        let dissolve_mask: Vec<f32> = if *style == TransitionStyle::Dissolve {
            use rand::Rng;
            let mut rng = rand::thread_rng();
            (0..n_pixels).map(|_| rng.gen::<f32>()).collect()
        } else {
            Vec::new()
        };

        let start = Instant::now();
        let total = Duration::from_millis(duration_ms as u64);
        let w = monitor_bounds.width as f32;
        let h = monitor_bounds.height as f32;

        loop {
            let elapsed = start.elapsed();
            if elapsed >= total {
                break;
            }

            let progress = (elapsed.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0);

            rt.BeginDraw();
            rt.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            }));

            let full_rect = D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: w,
                bottom: h,
            };

            match style {
                TransitionStyle::Crossfade => {
                    // Draw old at full opacity, then new on top with increasing opacity
                    rt.DrawBitmap(
                        &bmp_old,
                        Some(&full_rect),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&full_rect),
                    );
                    rt.DrawBitmap(
                        &bmp_new,
                        Some(&full_rect),
                        progress,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&full_rect),
                    );
                }

                TransitionStyle::SlideLeft => {
                    let offset = progress * w;
                    let old_dest = D2D_RECT_F {
                        left: -offset,
                        top: 0.0,
                        right: w - offset,
                        bottom: h,
                    };
                    let new_dest = D2D_RECT_F {
                        left: w - offset,
                        top: 0.0,
                        right: w * 2.0 - offset,
                        bottom: h,
                    };
                    rt.DrawBitmap(
                        &bmp_old,
                        Some(&old_dest),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&full_rect),
                    );
                    rt.DrawBitmap(
                        &bmp_new,
                        Some(&new_dest),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&full_rect),
                    );
                }

                TransitionStyle::SlideRight => {
                    let offset = progress * w;
                    let old_dest = D2D_RECT_F {
                        left: offset,
                        top: 0.0,
                        right: w + offset,
                        bottom: h,
                    };
                    let new_dest = D2D_RECT_F {
                        left: offset - w,
                        top: 0.0,
                        right: offset,
                        bottom: h,
                    };
                    rt.DrawBitmap(
                        &bmp_old,
                        Some(&old_dest),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&full_rect),
                    );
                    rt.DrawBitmap(
                        &bmp_new,
                        Some(&new_dest),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&full_rect),
                    );
                }

                TransitionStyle::WipeLeft => {
                    // Draw old fully; draw new clipped to [0, progress*w]
                    let clip_w = progress * w;
                    let clip_rect = D2D_RECT_F {
                        left: 0.0,
                        top: 0.0,
                        right: clip_w,
                        bottom: h,
                    };
                    rt.DrawBitmap(
                        &bmp_old,
                        Some(&full_rect),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&full_rect),
                    );
                    rt.DrawBitmap(
                        &bmp_new,
                        Some(&clip_rect),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&clip_rect),
                    );
                }

                TransitionStyle::WipeRight => {
                    let clip_left = w * (1.0 - progress);
                    let clip_rect = D2D_RECT_F {
                        left: clip_left,
                        top: 0.0,
                        right: w,
                        bottom: h,
                    };
                    rt.DrawBitmap(
                        &bmp_old,
                        Some(&full_rect),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&full_rect),
                    );
                    rt.DrawBitmap(
                        &bmp_new,
                        Some(&clip_rect),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&clip_rect),
                    );
                }

                TransitionStyle::Dissolve => {
                    // CPU-blend the dissolve into a scratch bitmap per frame.
                    // For GPU dissolve, we do it via a threshold on the random mask.
                    // We CPU-blend and re-upload (acceptable for dissolve which is
                    // already pixel-scattered).
                    let mut frame_buf = vec![0u8; n_pixels * 4];
                    for (i, mask) in dissolve_mask.iter().enumerate().take(n_pixels) {
                        let base = i * 4;
                        if progress >= *mask {
                            frame_buf[base..base + 4].copy_from_slice(&new_scaled[base..base + 4]);
                        } else {
                            frame_buf[base..base + 4].copy_from_slice(&old_scaled[base..base + 4]);
                        }
                    }
                    // Upload frame as a fresh bitmap
                    if let Ok(bmp_frame) = rt.CreateBitmap(
                        bmp_size,
                        Some(frame_buf.as_ptr() as *const _),
                        pitch,
                        &bmp_props,
                    ) {
                        rt.DrawBitmap(
                            &bmp_frame,
                            Some(&full_rect),
                            1.0,
                            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                            Some(&full_rect),
                        );
                    }
                }

                TransitionStyle::ZoomIn => {
                    // New image zooms in from scale 0 → 1; old fades out
                    let scale = progress;
                    let pad_x = w * (1.0 - scale) / 2.0;
                    let pad_y = h * (1.0 - scale) / 2.0;
                    let zoom_rect = D2D_RECT_F {
                        left: pad_x,
                        top: pad_y,
                        right: w - pad_x,
                        bottom: h - pad_y,
                    };
                    rt.DrawBitmap(
                        &bmp_old,
                        Some(&full_rect),
                        1.0 - progress,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&full_rect),
                    );
                    rt.DrawBitmap(
                        &bmp_new,
                        Some(&zoom_rect),
                        progress,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&full_rect),
                    );
                }

                TransitionStyle::ZoomOut => {
                    // Old image zooms out, new fades in
                    let inv = 1.0 - progress;
                    let pad_x = w * (1.0 - inv) / 2.0;
                    let pad_y = h * (1.0 - inv) / 2.0;
                    let zoom_rect = D2D_RECT_F {
                        left: pad_x,
                        top: pad_y,
                        right: w - pad_x,
                        bottom: h - pad_y,
                    };
                    rt.DrawBitmap(
                        &bmp_old,
                        Some(&zoom_rect),
                        inv,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&full_rect),
                    );
                    rt.DrawBitmap(
                        &bmp_new,
                        Some(&full_rect),
                        progress,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&full_rect),
                    );
                }

                TransitionStyle::None => {
                    rt.DrawBitmap(
                        &bmp_new,
                        Some(&full_rect),
                        1.0,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&full_rect),
                    );
                }
            }

            let _ = rt.EndDraw(None, None);

            // Pump messages
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            std::thread::sleep(Duration::from_millis(FRAME_INTERVAL_MS));
        }

        DestroyWindow(hwnd).ok();
    }

    Ok(())
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn d2d_wnd_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::DefWindowProcW;
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
