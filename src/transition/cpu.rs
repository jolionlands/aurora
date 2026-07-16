/// CPU-based transition renderer.
///
/// Supported styles: Crossfade, Dissolve.
/// For other styles we approximate with crossfade (callers degrade before calling here).
///
/// Implementation strategy:
/// - Create a nonactivating, click-through layered window covering the monitor bounds.
/// - Paint frames at ~60 fps by blending two BGRA buffers.
/// - Destroy the window when `duration_ms` elapses.
use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};

use super::{scale_to_cover, Rect, TransitionStyle};
use crate::decode::DecodedImage;

// Frame target: 60 fps ≈ 16.67 ms per frame
const FRAME_INTERVAL_MS: u64 = 17;

#[cfg(target_os = "windows")]
fn last_win32_error(operation: &str) -> anyhow::Error {
    anyhow::anyhow!("{operation}: {}", windows::core::Error::from_win32())
}

#[cfg(target_os = "windows")]
struct TransitionWindow(Option<windows::Win32::Foundation::HWND>);

#[cfg(target_os = "windows")]
impl TransitionWindow {
    fn new(hwnd: windows::Win32::Foundation::HWND) -> Self {
        Self(Some(hwnd))
    }

    fn handle(&self) -> windows::Win32::Foundation::HWND {
        self.0.expect("live transition window")
    }

    fn close(mut self) -> Result<()> {
        self.destroy()
    }

    fn destroy(&mut self) -> Result<()> {
        let Some(hwnd) = self.0.take() else {
            return Ok(());
        };
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd)
                .context("DestroyWindow failed for CPU transition")
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for TransitionWindow {
    fn drop(&mut self) {
        if let Err(error) = self.destroy() {
            tracing::error!(%error, "failed to destroy CPU transition window");
        }
    }
}

#[cfg(target_os = "windows")]
struct ScreenDc(Option<windows::Win32::Graphics::Gdi::HDC>);

#[cfg(target_os = "windows")]
impl ScreenDc {
    fn acquire() -> Result<Self> {
        let dc = unsafe { windows::Win32::Graphics::Gdi::GetDC(None) };
        if dc.is_invalid() {
            return Err(last_win32_error("GetDC(NULL) failed for CPU transition"));
        }
        Ok(Self(Some(dc)))
    }

    fn handle(&self) -> windows::Win32::Graphics::Gdi::HDC {
        self.0.expect("live screen DC")
    }

    fn close(mut self) -> Result<()> {
        self.release()
    }

    fn release(&mut self) -> Result<()> {
        let Some(dc) = self.0.take() else {
            return Ok(());
        };
        if unsafe { windows::Win32::Graphics::Gdi::ReleaseDC(None, dc) } == 0 {
            return Err(last_win32_error(
                "ReleaseDC failed for CPU transition screen DC",
            ));
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
impl Drop for ScreenDc {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            tracing::error!(%error, "failed to release CPU transition screen DC");
        }
    }
}

#[cfg(target_os = "windows")]
struct MemorySurface {
    dc: Option<windows::Win32::Graphics::Gdi::HDC>,
    bitmap: Option<windows::Win32::Graphics::Gdi::HBITMAP>,
    original: Option<windows::Win32::Graphics::Gdi::HGDIOBJ>,
}

#[cfg(target_os = "windows")]
impl MemorySurface {
    fn create(
        screen_dc: windows::Win32::Graphics::Gdi::HDC,
        width: i32,
        height: i32,
    ) -> Result<Self> {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, SelectObject,
        };

        let dc = unsafe { CreateCompatibleDC(screen_dc) };
        if dc.is_invalid() {
            return Err(last_win32_error(
                "CreateCompatibleDC failed for CPU transition",
            ));
        }

        let mut surface = Self {
            dc: Some(dc),
            bitmap: None,
            original: None,
        };
        let bitmap = unsafe { CreateCompatibleBitmap(screen_dc, width, height) };
        if bitmap.is_invalid() {
            return Err(last_win32_error(
                "CreateCompatibleBitmap failed for CPU transition",
            ));
        }
        surface.bitmap = Some(bitmap);

        let original = unsafe { SelectObject(dc, bitmap) };
        if original.is_invalid() {
            return Err(last_win32_error(
                "SelectObject failed for CPU transition bitmap",
            ));
        }
        surface.original = Some(original);
        Ok(surface)
    }

    fn dc(&self) -> windows::Win32::Graphics::Gdi::HDC {
        self.dc.expect("live memory DC")
    }

    fn close(mut self) -> Result<()> {
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<()> {
        use windows::Win32::Graphics::Gdi::{DeleteDC, DeleteObject, SelectObject};

        let Some(dc) = self.dc.take() else {
            return Ok(());
        };
        let mut bitmap = self.bitmap.take();
        let mut bitmap_selected = self.original.is_some();
        let mut errors = Vec::new();

        if let Some(original) = self.original.take() {
            if unsafe { SelectObject(dc, original) }.is_invalid() {
                errors.push(
                    last_win32_error("failed to restore original CPU transition bitmap")
                        .to_string(),
                );
            } else {
                bitmap_selected = false;
            }
        }

        if !bitmap_selected {
            if let Some(handle) = bitmap.take() {
                if !unsafe { DeleteObject(handle) }.as_bool() {
                    errors.push(
                        last_win32_error("DeleteObject failed for CPU transition bitmap")
                            .to_string(),
                    );
                }
            }
        }

        let dc_deleted = unsafe { DeleteDC(dc) }.as_bool();
        if !dc_deleted {
            errors
                .push(last_win32_error("DeleteDC failed for CPU transition memory DC").to_string());
        }

        // If restoring the stock bitmap failed, destroying the DC deselects our
        // bitmap. Only then is DeleteObject safe.
        if bitmap_selected && dc_deleted {
            if let Some(handle) = bitmap.take() {
                if !unsafe { DeleteObject(handle) }.as_bool() {
                    errors.push(
                        last_win32_error("DeleteObject failed for CPU transition bitmap")
                            .to_string(),
                    );
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            bail!("{}", errors.join("; "))
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for MemorySurface {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            tracing::error!(%error, "failed to clean up CPU transition GDI surface");
        }
    }
}

#[cfg(target_os = "windows")]
fn transition_window_ex_style() -> windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE {
    use windows::Win32::UI::WindowsAndMessaging::{
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    };

    WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_TRANSPARENT
}

#[cfg(target_os = "windows")]
fn stretch_dibits_failed(scan_lines: i32) -> bool {
    scan_lines == 0 || scan_lines == windows::Win32::Graphics::Gdi::GDI_ERROR
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
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        GetLastError, COLORREF, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, POINT,
    };
    use windows::Win32::Graphics::Gdi::{
        StretchDIBits, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        BLENDFUNCTION, DIB_RGB_COLORS, SRCCOPY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, PeekMessageW, RegisterClassExW, ShowWindow,
        TranslateMessage, UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW, MSG, PM_REMOVE,
        SW_SHOWNOACTIVATE, ULW_ALPHA, WNDCLASSEXW, WS_POPUP,
    };

    unsafe {
        let width = i32::try_from(monitor_bounds.width)
            .context("CPU transition width exceeds Win32 limits")?;
        let height = i32::try_from(monitor_bounds.height)
            .context("CPU transition height exceeds Win32 limits")?;
        if width == 0 || height == 0 {
            bail!("CPU transition requires non-zero monitor dimensions");
        }
        let width_pixels =
            usize::try_from(width).context("CPU transition width exceeds address space")?;
        let height_pixels =
            usize::try_from(height).context("CPU transition height exceeds address space")?;
        let n_pixels = width_pixels
            .checked_mul(height_pixels)
            .context("CPU transition pixel count overflow")?;
        let buffer_len = n_pixels
            .checked_mul(4)
            .context("CPU transition buffer size overflow")?;

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
        if RegisterClassExW(&wnd_class) == 0 {
            let error = GetLastError();
            if error != ERROR_CLASS_ALREADY_EXISTS {
                bail!(
                    "RegisterClassExW failed for CPU transition: Win32 error {}",
                    error.0
                );
            }
        }

        let window = TransitionWindow::new(
            CreateWindowExW(
                transition_window_ex_style(),
                PCWSTR(class_name.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                monitor_bounds.x,
                monitor_bounds.y,
                width,
                height,
                None,
                None,
                instance,
                None,
            )
            .context("CreateWindowExW failed for CPU transition")?,
        );
        let hwnd = window.handle();

        // ShowWindow returns the previous visibility state, not success/failure.
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        // Allocate a blended BGRA buffer
        let mut blend_buf = vec![0u8; buffer_len];

        // Scale old/new to monitor size with the same centered crop as Windows.
        let old_scaled = scale_to_cover(
            &old.bgra,
            old.width,
            old.height,
            width as u32,
            height as u32,
        );
        let new_scaled = scale_to_cover(
            &new.bgra,
            new.width,
            new.height,
            width as u32,
            height as u32,
        );

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

        let screen_dc = ScreenDc::acquire()?;
        let surface = MemorySurface::create(screen_dc.handle(), width, height)?;

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
            let uploaded = StretchDIBits(
                surface.dc(),
                0,
                0,
                width,
                height,
                0,
                0,
                width,
                height,
                Some(blend_buf.as_ptr() as *const std::ffi::c_void),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
            if stretch_dibits_failed(uploaded) {
                bail!("StretchDIBits failed for CPU transition frame (returned {uploaded})");
            }

            // Present the blended frame to the layered window
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let pt_src = POINT { x: 0, y: 0 };
            UpdateLayeredWindow(
                hwnd,
                None,
                None,
                None,
                surface.dc(),
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
            .context("UpdateLayeredWindow failed for CPU transition frame")?;

            // Pump messages so the window stays responsive
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            std::thread::sleep(Duration::from_millis(FRAME_INTERVAL_MS));
        }

        surface.close()?;
        screen_dc.close()?;
        window.close()?;
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
            for (i, mask) in dissolve_mask.iter().enumerate().take(n_pixels) {
                let base = i * 4;
                let src = if progress >= *mask {
                    &new[base..base + 4]
                } else {
                    &old[base..base + 4]
                };
                out[base..base + 3].copy_from_slice(&src[..3]);
                out[base + 3] = 255;
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

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn stretch_dibits_error_predicate_matches_gdi_contract() {
        use windows::Win32::Graphics::Gdi::GDI_ERROR;

        assert!(stretch_dibits_failed(0));
        assert!(stretch_dibits_failed(GDI_ERROR));
        assert!(!stretch_dibits_failed(1));
        assert!(!stretch_dibits_failed(-2));
    }

    #[test]
    fn cpu_overlay_is_click_through_and_nonactivating() {
        use windows::Win32::UI::WindowsAndMessaging::{
            WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TRANSPARENT,
        };

        let style = transition_window_ex_style();
        assert!(style.contains(WS_EX_LAYERED));
        assert!(style.contains(WS_EX_NOACTIVATE));
        assert!(style.contains(WS_EX_TRANSPARENT));
    }

    #[test]
    fn gdi_surface_upload_and_cleanup_smoke() -> Result<()> {
        use windows::Win32::Graphics::Gdi::{
            StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
        };

        let screen = ScreenDc::acquire()?;
        let surface = MemorySurface::create(screen.handle(), 2, 2)?;
        let pixels = [0xffu8; 2 * 2 * 4];
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 2,
                biHeight: -2,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let uploaded = unsafe {
            StretchDIBits(
                surface.dc(),
                0,
                0,
                2,
                2,
                0,
                0,
                2,
                2,
                Some(pixels.as_ptr().cast()),
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            )
        };
        assert!(!stretch_dibits_failed(uploaded));

        surface.close()?;
        screen.close()
    }
}
