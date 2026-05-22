/// CPU-based transition renderer.
///
/// Supported styles: Crossfade, Dissolve.
/// For other styles we approximate with crossfade (callers degrade before calling here).
///
/// Implementation strategy:
/// - Create a WS_EX_LAYERED | WS_EX_TOPMOST window covering the monitor bounds.
/// - Paint frames at ~60 fps by blending two BGRA buffers.
/// - Destroy the window when `duration_ms` elapses.
use anyhow::{Context, Result};
use std::time::{Duration, Instant};

use super::{DecodedImage, Rect, TransitionStyle};

// Frame target: 60 fps ≈ 16.67 ms per frame
const FRAME_INTERVAL_MS: u64 = 17;

pub fn run_transition(
    monitor_bounds: Rect,
    old: &DecodedImage,
    new: &DecodedImage,
    style: &TransitionStyle,
    duration_ms: u32,
) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        run_windows(monitor_bounds, old, new, style, duration_ms)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (monitor_bounds, old, new, style, duration_ms);
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn run_windows(
    monitor_bounds: Rect,
    old: &DecodedImage,
    new: &DecodedImage,
    style: &TransitionStyle,
    duration_ms: u32,
) -> Result<()> {
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
        GetDC, ReleaseDC, SelectObject, SetBitmapBits, BITMAPINFO, BITMAPINFOHEADER,
        BI_RGB, DIB_RGB_COLORS, HBITMAP, SRCCOPY, StretchDIBits, STRETCH_HALFTONE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
        RegisterClassExW, TranslateMessage, UpdateLayeredWindow, HWND_TOPMOST,
        MSG, PM_REMOVE, ULW_ALPHA, WINDOW_EX_STYLE, WM_PAINT, WNDCLASSEXW,
        WS_EX_LAYERED, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, CS_HREDRAW,
        CS_VREDRAW, SetWindowPos, SWP_NOACTIVATE,
    };
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::COLORREF;

    unsafe {
        let class_name: Vec<u16> = "AuroraTransitionCPU\0".encode_utf16().collect();
        let instance = HINSTANCE::default();

        let wnd_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(transition_wnd_proc),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hInstance: instance,
            ..Default::default()
        };
        // Ignore "already registered" error
        let _ = RegisterClassExW(&wnd_class);

        let hwnd = CreateWindowExW(
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
        .context("CreateWindowExW failed")?;

        let width = monitor_bounds.width as i32;
        let height = monitor_bounds.height as i32;
        let n_pixels = (width * height) as usize;

        // Allocate a blended BGRA buffer
        let mut blend_buf = vec![0u8; n_pixels * 4];

        // Scale old/new to monitor size (simple nearest-neighbour)
        let old_scaled = scale_to(&old.data, old.width, old.height, width as u32, height as u32);
        let new_scaled = scale_to(&new.data, new.width, new.height, width as u32, height as u32);

        // Pre-generate dissolve mask (random per pixel)
        let dissolve_mask: Vec<f32> = if *style == TransitionStyle::Dissolve {
            use rand::Rng;
            let mut rng = rand::thread_rng();
            (0..n_pixels).map(|_| rng.gen::<f32>()).collect()
        } else {
            Vec::new()
        };

        let start = Instant::now();
        let total = Duration::from_millis(duration_ms as u64);

        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bmp: HBITMAP = CreateCompatibleBitmap(screen_dc, width, height);
        let _old_bmp = SelectObject(mem_dc, bmp);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        loop {
            let elapsed = start.elapsed();
            if elapsed >= total {
                break;
            }

            let progress = elapsed.as_secs_f32() / total.as_secs_f32();
            let progress = progress.clamp(0.0, 1.0);

            blend_frame(
                &mut blend_buf,
                &old_scaled,
                &new_scaled,
                &dissolve_mask,
                style,
                progress,
                n_pixels,
            );

            // Upload to DC
            StretchDIBits(
                mem_dc,
                0, 0, width, height,
                0, 0, width, height,
                Some(blend_buf.as_ptr() as *const std::ffi::c_void),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );

            // Pump messages so the window stays responsive
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            std::thread::sleep(Duration::from_millis(FRAME_INTERVAL_MS));
        }

        DeleteDC(mem_dc);
        ReleaseDC(None, screen_dc);
        DeleteObject(bmp);
        DestroyWindow(hwnd).ok();
    }

    Ok(())
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn transition_wnd_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::DefWindowProcW;
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Blend two BGRA frames according to `style` and `progress` ∈ [0, 1].
fn blend_frame(
    out: &mut [u8],
    old: &[u8],
    new: &[u8],
    dissolve_mask: &[f32],
    style: &TransitionStyle,
    progress: f32,
    n_pixels: usize,
) {
    match style {
        TransitionStyle::Crossfade => {
            let alpha_new = progress;
            let alpha_old = 1.0 - progress;
            for i in 0..n_pixels {
                let base = i * 4;
                out[base] = blend_u8(old[base], new[base], alpha_old, alpha_new);
                out[base + 1] = blend_u8(old[base + 1], new[base + 1], alpha_old, alpha_new);
                out[base + 2] = blend_u8(old[base + 2], new[base + 2], alpha_old, alpha_new);
                out[base + 3] = 255;
            }
        }
        TransitionStyle::Dissolve => {
            // Each pixel transitions from old → new when progress > mask[pixel]
            for i in 0..n_pixels {
                let base = i * 4;
                let src = if progress >= dissolve_mask[i] {
                    &new[base..base + 4]
                } else {
                    &old[base..base + 4]
                };
                out[base..base + 4].copy_from_slice(src);
            }
        }
        // For any other style (shouldn't reach here in v1), fall back to crossfade
        _ => {
            let alpha_new = progress;
            let alpha_old = 1.0 - progress;
            for i in 0..n_pixels {
                let base = i * 4;
                out[base] = blend_u8(old[base], new[base], alpha_old, alpha_new);
                out[base + 1] = blend_u8(old[base + 1], new[base + 1], alpha_old, alpha_new);
                out[base + 2] = blend_u8(old[base + 2], new[base + 2], alpha_old, alpha_new);
                out[base + 3] = 255;
            }
        }
    }
}

#[inline]
fn blend_u8(a: u8, b: u8, wa: f32, wb: f32) -> u8 {
    ((a as f32 * wa) + (b as f32 * wb)).clamp(0.0, 255.0) as u8
}

/// Nearest-neighbour scale of a BGRA buffer.
fn scale_to(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (dst_w * dst_h * 4) as usize];
    for dy in 0..dst_h {
        let sy = (dy as f32 * src_h as f32 / dst_h as f32) as u32;
        for dx in 0..dst_w {
            let sx = (dx as f32 * src_w as f32 / dst_w as f32) as u32;
            let src_idx = ((sy * src_w + sx) * 4) as usize;
            let dst_idx = ((dy * dst_w + dx) * 4) as usize;
            if src_idx + 3 < src.len() && dst_idx + 3 < out.len() {
                out[dst_idx..dst_idx + 4].copy_from_slice(&src[src_idx..src_idx + 4]);
            }
        }
    }
    out
}
