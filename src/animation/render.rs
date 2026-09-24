//! GPU presentation of animation frames.
//!
//! Each player window gets a flip-model DXGI swap chain drawn through a
//! Direct2D device context. This is the one path that the raised desktop
//! (Windows 11 24H2+) composes: GDI, `UpdateLayeredWindow`, and even a
//! Direct2D HWND render target all stay invisible in a child of the
//! no-redirection `Progman` (verified on build 26200). The same path works on
//! the classic `WorkerW` layout.
//!
//! GPU objects exist only while something animates; the player drops them
//! when the last animated display goes away.

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1DeviceContext, ID2D1Factory1,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_INTERPOLATION_MODE_LINEAR, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1, DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

use super::desktop::PixelRect;
use super::frames::Animation;

const PIXEL_FORMAT: D2D1_PIXEL_FORMAT = D2D1_PIXEL_FORMAT {
    format: DXGI_FORMAT_B8G8R8A8_UNORM,
    alphaMode: D2D1_ALPHA_MODE_IGNORE,
};

/// Shared device objects for every player window on the player thread.
pub struct Gpu {
    d3d: ID3D11Device,
    dxgi_factory: IDXGIFactory2,
    context: ID2D1DeviceContext,
}

impl Gpu {
    pub fn new() -> Result<Self> {
        let d3d = created3d_device(D3D_DRIVER_TYPE_HARDWARE)
            .or_else(|_| created3d_device(D3D_DRIVER_TYPE_WARP))
            .context("create Direct3D 11 device")?;
        unsafe {
            let dxgi: IDXGIDevice = d3d.cast().context("query DXGI device")?;
            let dxgi_factory: IDXGIFactory2 = dxgi
                .GetAdapter()
                .context("query DXGI adapter")?
                .GetParent()
                .context("query DXGI factory")?;
            let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
                .context("create Direct2D factory")?;
            let context = d2d
                .CreateDevice(&dxgi)
                .context("create Direct2D device")?
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
                .context("create Direct2D device context")?;
            Ok(Self {
                d3d,
                dxgi_factory,
                context,
            })
        }
    }
}

fn created3d_device(driver: D3D_DRIVER_TYPE) -> Result<ID3D11Device> {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            driver,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    device.context("Direct3D returned no device")
}

/// Per-window swap chain plus one GPU bitmap per animation frame.
///
/// Each frame is uploaded the first time it is shown and then stays on the
/// GPU, so steady-state playback is a draw and a present with no CPU copy.
pub struct Surface {
    swapchain: IDXGISwapChain1,
    target: ID2D1Bitmap1,
    frames: Vec<Option<ID2D1Bitmap1>>,
    frame_size: (u32, u32),
}

/// Where and how a frame is drawn into the window.
pub struct Layout {
    /// Source rectangle in frame pixels.
    pub src: PixelRect,
    /// Destination rectangle in window pixels.
    pub dst: PixelRect,
    /// Fill (BGRA) for the area outside `dst`, when it does not cover all.
    pub letterbox: Option<[u8; 4]>,
    /// Nearest-neighbour instead of linear filtering.
    pub pixel_art: bool,
}

/// Why a frame could not be presented.
#[derive(Debug)]
pub enum PresentError {
    /// The frame is not on the GPU and its pixels were already released.
    NotResident,
    /// Device loss or another GPU failure; recreate every GPU object.
    Device(anyhow::Error),
}

impl Surface {
    pub fn new(
        gpu: &Gpu,
        hwnd: HWND,
        width: u32,
        height: u32,
        animation: &Animation,
    ) -> Result<Self> {
        unsafe {
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                ..Default::default()
            };
            let swapchain = gpu
                .dxgi_factory
                .CreateSwapChainForHwnd(&gpu.d3d, hwnd, &desc, None, None)
                .context("create swap chain")?;
            let buffer: IDXGISurface = swapchain.GetBuffer(0).context("get swap chain buffer")?;
            let target = gpu
                .context
                .CreateBitmapFromDxgiSurface(
                    &buffer,
                    Some(&D2D1_BITMAP_PROPERTIES1 {
                        pixelFormat: PIXEL_FORMAT,
                        dpiX: 96.0,
                        dpiY: 96.0,
                        bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                        ..Default::default()
                    }),
                )
                .context("wrap swap chain buffer")?;
            Ok(Self {
                swapchain,
                target,
                frames: vec![None; animation.frames.len()],
                frame_size: (animation.width, animation.height),
            })
        }
    }

    /// True once every frame has been uploaded.
    pub fn all_resident(&self) -> bool {
        self.frames.iter().all(Option::is_some)
    }

    fn upload(&mut self, gpu: &Gpu, index: usize, pixels: &[u8]) -> Result<()> {
        let bitmap = unsafe {
            gpu.context.CreateBitmap(
                D2D_SIZE_U {
                    width: self.frame_size.0,
                    height: self.frame_size.1,
                },
                Some(pixels.as_ptr().cast()),
                self.frame_size.0 * 4,
                &D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: PIXEL_FORMAT,
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
                    ..Default::default()
                },
            )
        }
        .context("upload animation frame")?;
        self.frames[index] = Some(bitmap);
        Ok(())
    }

    /// Present frame `index`, cropping `src` into `dst` (both `(x, y, w, h)`
    /// in pixels). `pixels` is needed only the first time a frame is shown.
    pub fn present(
        &mut self,
        gpu: &Gpu,
        index: usize,
        pixels: Option<&[u8]>,
        layout: &Layout,
    ) -> std::result::Result<(), PresentError> {
        if self.frames.get(index).is_none_or(Option::is_none) {
            let expected = (self.frame_size.0 as usize) * (self.frame_size.1 as usize) * 4;
            match pixels {
                Some(pixels) if index < self.frames.len() && pixels.len() == expected => self
                    .upload(gpu, index, pixels)
                    .map_err(PresentError::Device)?,
                _ => return Err(PresentError::NotResident),
            }
        }
        let Some(Some(frame)) = self.frames.get(index) else {
            return Err(PresentError::NotResident);
        };
        let rect = |(x, y, w, h): PixelRect| D2D_RECT_F {
            left: x as f32,
            top: y as f32,
            right: (x + w) as f32,
            bottom: (y + h) as f32,
        };
        unsafe {
            let context = &gpu.context;
            context.SetTarget(&self.target);
            context.BeginDraw();
            if let Some([b, g, r, _]) = layout.letterbox {
                context.Clear(Some(&D2D1_COLOR_F {
                    r: f32::from(r) / 255.0,
                    g: f32::from(g) / 255.0,
                    b: f32::from(b) / 255.0,
                    a: 1.0,
                }));
            }
            let interpolation = if layout.pixel_art {
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR
            } else {
                D2D1_INTERPOLATION_MODE_LINEAR
            };
            context.DrawBitmap(
                frame,
                Some(&rect(layout.dst)),
                1.0,
                interpolation,
                Some(&rect(layout.src)),
                None,
            );
            let drawn = context.EndDraw(None, None);
            context.SetTarget(None);
            drawn
                .context("draw animation frame")
                .map_err(PresentError::Device)?;
            // Sync interval 0: flip-model presents are composed by DWM on its
            // own schedule; waiting for a vblank only adds wakeups.
            self.swapchain
                .Present(0, DXGI_PRESENT(0))
                .ok()
                .context("present animation frame")
                .map_err(PresentError::Device)
        }
    }
}
