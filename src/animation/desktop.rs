//! Desktop-layer windows that sit between the wallpaper and the desktop icons.
//!
//! Two shell layouts exist:
//!
//! * **Raised desktop** (Windows 11 24H2 and later): `Progman` is a
//!   `WS_EX_NOREDIRECTIONBITMAP` window whose children are the icon view
//!   (`SHELLDLL_DefView`) and, after message `0x052C`, a wallpaper `WorkerW`.
//!   A player window is created top-level with `WS_EX_LAYERED` (required for
//!   it to be composed under a no-redirection parent), re-parented onto
//!   `Progman`, z-ordered directly below the icon view, and drawn with
//!   Direct2D (see [`PlayerWindow`]).
//! * **Classic** (Windows 10 and earlier Windows 11): message `0x052C` spawns
//!   a top-level `WorkerW` behind the icon host; player windows are its
//!   children.
//!
//! Aurora never redraws, raises, or destroys shell windows: doing so on a
//! raised desktop can blank the wallpaper layer.

use std::cell::Cell;

use anyhow::{bail, Context, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, BOOL, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, MapWindowPoints, PAINTSTRUCT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, EnumWindows, FindWindowExW, FindWindowW,
    GetWindowLongPtrW, IsWindow, RegisterClassExW, SendMessageTimeoutW, SetLayeredWindowAttributes,
    SetParent, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, GWL_STYLE, HWND_BOTTOM, LWA_ALPHA,
    SMTO_NORMAL, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    WM_ERASEBKGND, WM_PAINT, WNDCLASSEXW, WS_CHILD, WS_CLIPSIBLINGS, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};

use super::frames::Animation;
use super::render::{Gpu, PresentError, Surface};
use crate::apply::WallpaperFit;
use crate::transition::Rect;

/// Undocumented Progman message that makes Explorer create the wallpaper
/// `WorkerW`. `wParam = 0xD, lParam = 1` is the single-message form that works
/// on raised desktops without tearing down an existing `WorkerW`.
const SPAWN_WORKERW: u32 = 0x052C;
const CLASS_NAME: PCWSTR = w!("AuroraAnimatedWallpaper");

/// Where player windows are attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopHost {
    Raised {
        progman: HWND,
        icons: HWND,
        workerw: Option<HWND>,
    },
    Classic {
        workerw: HWND,
    },
}

impl DesktopHost {
    /// Locate (and if necessary ask Explorer to create) the wallpaper layer.
    pub fn find() -> Result<Self> {
        unsafe {
            let progman = FindWindowW(w!("Progman"), PCWSTR::null())
                .context("find the Explorer desktop window (Progman)")?;
            let mut result = 0usize;
            SendMessageTimeoutW(
                progman,
                SPAWN_WORKERW,
                WPARAM(0xD),
                LPARAM(1),
                SMTO_NORMAL,
                1000,
                Some(&mut result),
            );

            let ex_style = GetWindowLongPtrW(progman, GWL_EXSTYLE) as u32;
            let icons_in_progman = FindWindowExW(
                progman,
                HWND::default(),
                w!("SHELLDLL_DefView"),
                PCWSTR::null(),
            )
            .ok();
            if let Some(icons) = icons_in_progman {
                if is_raised_layout(ex_style) {
                    let workerw =
                        FindWindowExW(progman, HWND::default(), w!("WorkerW"), PCWSTR::null()).ok();
                    return Ok(Self::Raised {
                        progman,
                        icons,
                        workerw,
                    });
                }
            }

            // Classic: the WorkerW after the top-level window hosting the icons.
            let mut icon_host = HWND::default();
            let _ = EnumWindows(
                Some(find_icon_host),
                LPARAM(&mut icon_host as *mut HWND as isize),
            );
            if icon_host.is_invalid() {
                bail!("could not find the desktop icon host (SHELLDLL_DefView)");
            }
            let workerw = FindWindowExW(HWND::default(), icon_host, w!("WorkerW"), PCWSTR::null())
                .context("Explorer did not create a wallpaper WorkerW")?;
            Ok(Self::Classic { workerw })
        }
    }

    fn parent(&self) -> HWND {
        match *self {
            Self::Raised { progman, .. } => progman,
            Self::Classic { workerw } => workerw,
        }
    }

    /// False once Explorer has restarted and the shell windows are gone.
    pub fn is_alive(&self) -> bool {
        let alive = |hwnd: HWND| unsafe { IsWindow(hwnd).as_bool() };
        match *self {
            Self::Raised { progman, icons, .. } => alive(progman) && alive(icons),
            Self::Classic { workerw } => alive(workerw),
        }
    }

    pub fn is_raised(&self) -> bool {
        matches!(self, Self::Raised { .. })
    }
}

fn is_raised_layout(progman_ex_style: u32) -> bool {
    progman_ex_style & WS_EX_NOREDIRECTIONBITMAP.0 != 0
}

unsafe extern "system" fn find_icon_host(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let found = FindWindowExW(
        hwnd,
        HWND::default(),
        w!("SHELLDLL_DefView"),
        PCWSTR::null(),
    );
    if found.is_ok() {
        *(lparam.0 as *mut HWND) = hwnd;
        return BOOL(0);
    }
    BOOL(1)
}

// ---------------------------------------------------------------------------
// Player windows
// ---------------------------------------------------------------------------
//
// Frames are presented through a DXGI swap chain (see `render`).

thread_local! {
    /// Set when Windows asks a player window to repaint (e.g. after the
    /// desktop was uncovered); the player loop redraws the current frames.
    static REPAINT_REQUESTED: Cell<bool> = const { Cell::new(false) };
}

/// True (once) if any player window received WM_PAINT since the last call.
pub fn take_repaint_request() -> bool {
    REPAINT_REQUESTED.with(|requested| requested.replace(false))
}

/// A desktop-layer window showing one animation on one display. Must be
/// created, used, and dropped on the thread that pumps its messages.
pub struct PlayerWindow {
    hwnd: HWND,
    width: u32,
    height: u32,
    surface: Option<Surface>,
}

impl PlayerWindow {
    pub fn create(host: &DesktopHost, bounds: Rect) -> Result<Self> {
        register_class()?;
        let width = i32::try_from(bounds.width).context("display width")?;
        let height = i32::try_from(bounds.height).context("display height")?;
        if width == 0 || height == 0 {
            bail!("display has no area");
        }
        unsafe {
            let parent = host.parent();
            let mut origin = [POINT {
                x: bounds.x,
                y: bounds.y,
            }];
            MapWindowPoints(HWND::default(), parent, &mut origin);
            let origin = origin[0];

            let hwnd = match host {
                DesktopHost::Raised { icons, workerw, .. } => {
                    // Layered at creation (it cannot be added once parented),
                    // then adopted by Progman as a child below the icon view.
                    let hwnd = CreateWindowExW(
                        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                        CLASS_NAME,
                        PCWSTR::null(),
                        WS_POPUP,
                        0,
                        0,
                        width,
                        height,
                        HWND::default(),
                        None,
                        HINSTANCE::default(),
                        None,
                    )
                    .context("create animated wallpaper window")?;
                    let window = Self {
                        hwnd,
                        width: bounds.width,
                        height: bounds.height,
                        surface: None,
                    };
                    SetLayeredWindowAttributes(hwnd, None, 255, LWA_ALPHA)
                        .context("make animated wallpaper window opaque")?;
                    SetParent(hwnd, parent).context("attach to Progman")?;
                    // WS_VISIBLE must come from SWP_SHOWWINDOW below, not from
                    // the style bits: a window made visible by SetWindowLong
                    // alone is never shown to DWM.
                    SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_CHILD | WS_CLIPSIBLINGS).0 as isize);
                    SetWindowPos(
                        hwnd,
                        *icons,
                        origin.x,
                        origin.y,
                        width,
                        height,
                        SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_SHOWWINDOW,
                    )
                    .context("position animated wallpaper window")?;
                    if let Some(workerw) = workerw {
                        let _ = SetWindowPos(
                            *workerw,
                            HWND_BOTTOM,
                            0,
                            0,
                            0,
                            0,
                            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                        );
                    }
                    return Ok(window);
                }
                DesktopHost::Classic { workerw } => CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    CLASS_NAME,
                    PCWSTR::null(),
                    WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
                    origin.x,
                    origin.y,
                    width,
                    height,
                    *workerw,
                    None,
                    HINSTANCE::default(),
                    None,
                )
                .context("create animated wallpaper window")?,
            };
            Ok(Self {
                hwnd,
                width: bounds.width,
                height: bounds.height,
                surface: None,
            })
        }
    }

    /// Draw `frame` of `animation` now. Frames whose pixels were released
    /// must already be on this window's GPU surface.
    pub fn show_frame(
        &mut self,
        gpu: &Gpu,
        animation: &Animation,
        frame: usize,
        fit: WallpaperFit,
    ) -> std::result::Result<(), PresentError> {
        let Some(pixels) = animation.frames.get(frame) else {
            return Ok(());
        };
        if self.surface.is_none() {
            let surface = Surface::new(gpu, self.hwnd, self.width, self.height, animation)
                .map_err(PresentError::Device)?;
            self.surface = Some(surface);
        }
        let Some(surface) = &mut self.surface else {
            return Ok(());
        };
        let full = (0, 0, self.width as i32, self.height as i32);
        let (src, dst) = placement(
            fit,
            animation.width as i32,
            animation.height as i32,
            full.2,
            full.3,
        );
        let pixels = (!pixels.bgra.is_empty()).then_some(&*pixels.bgra);
        let result = surface.present(gpu, frame, pixels, src, dst, dst != full);
        if matches!(result, Err(PresentError::Device(_))) {
            self.surface = None;
        }
        result
    }

    /// True once every frame lives on the GPU, so CPU copies can go.
    pub fn frames_resident(&self) -> bool {
        self.surface.as_ref().is_some_and(Surface::all_resident)
    }

    /// Release GPU objects tied to a device that is being replaced.
    pub fn release_surface(&mut self) {
        self.surface = None;
    }

    /// False if Explorer destroyed the window (for example on restart).
    pub fn is_alive(&self) -> bool {
        unsafe { IsWindow(self.hwnd).as_bool() }
    }
}

impl Drop for PlayerWindow {
    fn drop(&mut self) {
        self.surface = None;
        unsafe {
            if IsWindow(self.hwnd).as_bool() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

fn register_class() -> Result<()> {
    unsafe {
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(player_wnd_proc),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassExW(&class) == 0 {
            let error = GetLastError();
            if error != ERROR_CLASS_ALREADY_EXISTS {
                bail!(
                    "RegisterClassExW for animated wallpaper: Win32 error {}",
                    error.0
                );
            }
        }
    }
    Ok(())
}

unsafe extern "system" fn player_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            BeginPaint(hwnd, &mut paint);
            let _ = EndPaint(hwnd, &paint);
            REPAINT_REQUESTED.with(|requested| requested.set(true));
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// `(x, y, width, height)` in pixels.
pub type PixelRect = (i32, i32, i32, i32);

/// Source and destination rectangles `(x, y, w, h)` for drawing a
/// `src_w` x `src_h` frame into a `dst_w` x `dst_h` window.
pub(crate) fn placement(
    fit: WallpaperFit,
    src_w: i32,
    src_h: i32,
    dst_w: i32,
    dst_h: i32,
) -> (PixelRect, PixelRect) {
    let full_src = (0, 0, src_w, src_h);
    let full_dst = (0, 0, dst_w, dst_h);
    if src_w <= 0 || src_h <= 0 || dst_w <= 0 || dst_h <= 0 {
        return (full_src, full_dst);
    }
    let (sw, sh, dw, dh) = (
        i64::from(src_w),
        i64::from(src_h),
        i64::from(dst_w),
        i64::from(dst_h),
    );
    match fit {
        WallpaperFit::Stretch => (full_src, full_dst),
        WallpaperFit::Contain => {
            // Letterbox: scale the whole frame to fit inside the window.
            if sw * dh > sh * dw {
                let h = (sh * dw / sw) as i32;
                (full_src, (0, (dst_h - h) / 2, dst_w, h))
            } else {
                let w = (sw * dh / sh) as i32;
                (full_src, ((dst_w - w) / 2, 0, w, dst_h))
            }
        }
        // Fill, and the modes a single animated frame cannot express
        // (tile, center, span), crop the frame to the window's aspect ratio.
        _ => {
            if sw * dh > sh * dw {
                let w = (dw * sh / dh) as i32;
                (((src_w - w) / 2, 0, w, src_h), full_dst)
            } else {
                let h = (dh * sw / dw) as i32;
                ((0, (src_h - h) / 2, src_w, h), full_dst)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_crops_the_frame_to_the_display_aspect() {
        // 16:9 frame on a 16:10 display: crop the sides.
        let (src, dst) = placement(WallpaperFit::Fill, 480, 270, 1280, 800);
        assert_eq!(dst, (0, 0, 1280, 800));
        assert_eq!(src, (24, 0, 432, 270));
        // Tall frame: crop top and bottom.
        let (src, _) = placement(WallpaperFit::Fill, 400, 400, 1280, 800);
        assert_eq!(src, (0, 75, 400, 250));
        // Span/tile/center behave like fill for animations.
        assert_eq!(
            placement(WallpaperFit::Span, 480, 270, 1280, 800),
            placement(WallpaperFit::Fill, 480, 270, 1280, 800)
        );
    }

    #[test]
    fn contain_letterboxes_and_stretch_uses_everything() {
        let (src, dst) = placement(WallpaperFit::Contain, 480, 270, 1280, 800);
        assert_eq!(src, (0, 0, 480, 270));
        assert_eq!(dst, (0, 40, 1280, 720));
        let (src, dst) = placement(WallpaperFit::Stretch, 480, 270, 1280, 800);
        assert_eq!((src, dst), ((0, 0, 480, 270), (0, 0, 1280, 800)));
        // Degenerate sizes never divide by zero.
        let _ = placement(WallpaperFit::Fill, 0, 0, 1280, 800);
    }

    #[test]
    fn raised_layout_is_detected_from_progman_style() {
        assert!(is_raised_layout(0x0020_0080));
        assert!(!is_raised_layout(0x0000_0080));
    }
}
