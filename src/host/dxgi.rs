//! Primary-monitor capture via DXGI Desktop Duplication: GPU-side, ~1 ms per frame,
//! and it only yields frames when the screen actually changed.

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
                D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
                ID3D11DeviceContext, ID3D11Texture2D,
            },
            Dxgi::{
                CreateDXGIFactory1, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1,
                IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
            },
        },
    },
    core::Interface,
};

pub struct Dxgi {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    pub width: u32,
    pub height: u32,
    /// Last captured frame, tightly packed BGRA.
    pub bgra: Vec<u8>,
}

/// A monitor's place on the virtual desktop.
#[derive(Clone, Copy, Debug)]
pub struct Output {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

impl Output {
    /// The primary monitor is the one at the desktop origin.
    pub fn primary(&self) -> bool {
        self.left == 0 && self.top == 0
    }
}

/// Monitors attached to the desktop, in adapter/output order.
fn enumerate() -> Result<Vec<(IDXGIAdapter1, IDXGIOutput1, Output)>> {
    let mut found = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut adapter_idx = 0;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_idx) {
            adapter_idx += 1;
            let mut output_idx = 0;
            while let Ok(output) = adapter.EnumOutputs(output_idx) {
                output_idx += 1;
                let desc = output.GetDesc()?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let r = desc.DesktopCoordinates;
                let place = Output {
                    left: r.left,
                    top: r.top,
                    width: (r.right - r.left) as u32,
                    height: (r.bottom - r.top) as u32,
                };
                found.push((adapter.clone(), output.cast::<IDXGIOutput1>()?, place));
            }
        }
    }
    Ok(found)
}

/// The monitors a viewer can choose from.
pub fn outputs() -> Vec<Output> {
    enumerate().map(|v| v.into_iter().map(|(_, _, o)| o).collect()).unwrap_or_default()
}

impl Dxgi {
    /// Duplicates monitor `index` of [`outputs`], or the primary one.
    pub fn new(index: Option<usize>) -> Result<(Self, Output)> {
        let mut all = enumerate()?;
        let pick = index.filter(|&i| i < all.len()).or_else(|| all.iter().position(|(_, _, o)| o.primary()));
        let Some(i) = pick else { bail!("primary monitor not found among DXGI outputs") };
        let (adapter, output, place) = all.swap_remove(i);
        let dxgi = unsafe { Self::open(&adapter, output, place.width, place.height)? };
        Ok((dxgi, place))
    }

    unsafe fn open(adapter: &IDXGIAdapter1, output: IDXGIOutput1, width: u32, height: u32) -> Result<Self> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device: ID3D11Device = device.context("no D3D11 device")?;
        let duplication = unsafe { output.DuplicateOutput(&device) }.context("DuplicateOutput")?;
        Ok(Dxgi {
            device,
            context: context.context("no D3D11 context")?,
            duplication,
            staging: None,
            width,
            height,
            bgra: vec![0; (width * height * 4) as usize],
        })
    }

    /// Waits up to `timeout_ms` for a screen update; returns `Ok(true)` when `bgra` was refreshed.
    /// An `Err` means the duplication is lost (mode change, UAC/lock screen) and must be recreated.
    pub fn next_frame(&mut self, timeout_ms: u32) -> Result<bool> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        unsafe {
            if let Err(e) = self.duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(false);
                }
                return Err(e.into());
            }
            let result = self.copy_frame(&info, resource);
            let _ = self.duplication.ReleaseFrame();
            result
        }
    }

    unsafe fn copy_frame(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO, resource: Option<IDXGIResource>) -> Result<bool> {
        // LastPresentTime == 0: only the mouse pointer moved.
        if info.LastPresentTime == 0 {
            return Ok(false);
        }
        let texture: ID3D11Texture2D = resource.context("no desktop resource")?.cast()?;
        unsafe {
            if self.staging.is_none() {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                if (desc.Width, desc.Height) != (self.width, self.height) {
                    bail!("display mode changed");
                }
                desc.Usage = D3D11_USAGE_STAGING;
                desc.BindFlags = 0;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                desc.MiscFlags = 0;
                desc.MipLevels = 1;
                desc.ArraySize = 1;
                let mut staging = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
                self.staging = staging;
            }
            let staging = self.staging.as_ref().unwrap();
            self.context.CopyResource(staging, &texture);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let row = (self.width * 4) as usize;
            let pitch = mapped.RowPitch as usize;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * self.height as usize);
            for y in 0..self.height as usize {
                self.bgra[y * row..(y + 1) * row].copy_from_slice(&src[y * pitch..y * pitch + row]);
            }
            self.context.Unmap(staging, 0);
        }
        Ok(true)
    }
}
