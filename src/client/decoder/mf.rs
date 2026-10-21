//! Hardware H.264 decoding: the Microsoft H.264 decoder MFT bound to a D3D11 device, so decoding
//! runs on the GPU (DXVA). Decoded NV12 surfaces are read back and converted to RGBA for egui.

#![allow(non_upper_case_globals)]

use std::mem::ManuallyDrop;

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
            },
        },
        Media::MediaFoundation::*,
        System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree},
    },
    core::{GUID, Interface, PWSTR},
};

use super::VideoDecoder;
use crate::client::viewer::VideoFrame;

struct Gpu {
    /// Kept alive for the decoder's lifetime.
    manager: IMFDXGIDeviceManager,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Option<ID3D11Texture2D>,
}

pub struct MfDecoder {
    transform: IMFTransform,
    /// D3D11 device bound to the decoder (DXVA mode), its context and our CPU-readable copy target.
    gpu: Option<Gpu>,
    width: u32,
    height: u32,
    /// Allocated surface height (H.264 pads to 16 lines, e.g. 1088 for 1080p).
    alloc_height: u32,
    timestamp: i64,
    name: String,
}

impl MfDecoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;
        }
        let (transform, mut name) = activate_decoder()?;
        let attrs = unsafe { transform.GetAttributes()? };
        unsafe {
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }

        // Bind a D3D11 device so the MFT decodes with DXVA; without it decoding is software.
        let d3d_aware = unsafe { attrs.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) != 0;
        let gpu = if d3d_aware {
            match create_gpu().and_then(|g| {
                unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, g.manager.as_raw() as usize) }?;
                Ok(g)
            }) {
                Ok(g) => {
                    name.push_str(" (аппаратный, DXVA)");
                    Some(g)
                }
                Err(e) => {
                    tracing::warn!("DXVA unavailable, decoder runs in software: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        if gpu.is_none() {
            name.push_str(" (программный)");
        }

        unsafe {
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &input, 0).context("SetInputType")?;
        }
        let mut decoder = MfDecoder { transform, gpu, width, height, alloc_height: height, timestamp: 0, name };
        decoder.set_nv12_output()?;
        unsafe {
            decoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            decoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        tracing::info!(decoder = %decoder.name, "H.264 decoder ready");
        Ok(decoder)
    }

    /// Picks NV12 among the output types (again after a stream change) and records the surface size.
    fn set_nv12_output(&mut self) -> Result<()> {
        for i in 0.. {
            let Ok(ty) = (unsafe { self.transform.GetOutputAvailableType(0, i) }) else {
                break;
            };
            let subtype: GUID = unsafe { ty.GetGUID(&MF_MT_SUBTYPE) }?;
            if subtype != MFVideoFormat_NV12 {
                continue;
            }
            unsafe { self.transform.SetOutputType(0, &ty, 0) }.context("SetOutputType")?;
            if let Ok(size) = unsafe { ty.GetUINT64(&MF_MT_FRAME_SIZE) } {
                self.alloc_height = (size & 0xffff_ffff) as u32;
            }
            return Ok(());
        }
        bail!("decoder offers no NV12 output")
    }

    /// Pulls one decoded picture. `Ok(None)` when the decoder needs more input.
    fn pull(&mut self) -> Result<Option<VideoFrame>> {
        loop {
            let info = unsafe { self.transform.GetOutputStreamInfo(0)? };
            let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            let sample = if provides {
                None
            } else {
                unsafe {
                    let s = MFCreateSample()?;
                    s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize)?)?;
                    Some(s)
                }
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
            let sample = unsafe { ManuallyDrop::take(&mut buffers[0].pSample) };
            drop(unsafe { ManuallyDrop::take(&mut buffers[0].pEvents) });
            match result {
                Ok(()) => {
                    return self.read_picture(&sample.context("decoder returned no sample")?).map(Some);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                // First SPS parsed (or resolution changed): renegotiate and try again.
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.set_nv12_output()?,
                Err(e) => return Err(e).context("ProcessOutput"),
            }
        }
    }

    fn read_picture(&mut self, sample: &IMFSample) -> Result<VideoFrame> {
        let (w, h) = (self.width as usize, self.height as usize);
        let buffer = unsafe { sample.GetBufferByIndex(0)? };
        let (pitch, rows, data): (usize, usize, Vec<u8>);
        if let (Some(gpu), Ok(dxgi)) = (self.gpu.as_mut(), buffer.cast::<IMFDXGIBuffer>()) {
            // DXVA surface: one GPU copy into our staging texture, then a single mapped read.
            unsafe {
                let mut raw = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
                let texture = ID3D11Texture2D::from_raw(raw);
                let index = dxgi.GetSubresourceIndex()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                let staging = match &gpu.staging {
                    Some(s) => s.clone(),
                    None => {
                        let staging_desc = D3D11_TEXTURE2D_DESC {
                            ArraySize: 1,
                            MipLevels: 1,
                            Usage: D3D11_USAGE_STAGING,
                            BindFlags: 0,
                            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                            MiscFlags: 0,
                            ..desc
                        };
                        let mut s = None;
                        gpu.device.CreateTexture2D(&staging_desc, None, Some(&mut s))?;
                        gpu.staging = s.clone();
                        s.context("no staging texture")?
                    }
                };
                gpu.context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &texture, index, None);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                pitch = mapped.RowPitch as usize;
                rows = desc.Height as usize;
                data = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * rows * 3 / 2).to_vec();
                gpu.context.Unmap(&staging, 0);
            }
        } else if let Ok(buf2d) = buffer.cast::<IMF2DBuffer>() {
            unsafe {
                let mut scan0 = std::ptr::null_mut();
                let mut p = 0i32;
                buf2d.Lock2D(&mut scan0, &mut p)?;
                pitch = p.unsigned_abs() as usize;
                rows = self.alloc_height as usize;
                data = std::slice::from_raw_parts(scan0, pitch * rows * 3 / 2).to_vec();
                buf2d.Unlock2D()?;
            }
        } else {
            bail!("unsupported decoder output buffer");
        }
        let (y_plane, uv_plane) = data.split_at(pitch * rows);
        let mut y = Vec::with_capacity(w * h);
        let mut uv = Vec::with_capacity(w * h / 2);
        for row in 0..h {
            y.extend_from_slice(&y_plane[row * pitch..row * pitch + w]);
        }
        for row in 0..h / 2 {
            uv.extend_from_slice(&uv_plane[row * pitch..row * pitch + w]);
        }
        Ok(VideoFrame { width: w, height: h, y, uv })
    }
}

impl VideoDecoder for MfDecoder {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn decode(&mut self, au: &[u8]) -> Result<Option<VideoFrame>> {
        let sample = unsafe {
            let buffer = MFCreateMemoryBuffer(au.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(au.as_ptr(), ptr, au.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(au.len() as u32)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&buffer)?;
            s.SetSampleTime(self.timestamp)?;
            self.timestamp += 166_667;
            s
        };
        let mut newest = None;
        loop {
            match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                Ok(()) => break,
                // Output queue full: drain it, then the input is accepted.
                Err(e) if e.code() == MF_E_NOTACCEPTING => match self.pull()? {
                    Some(f) => newest = Some(f),
                    None => bail!("decoder refuses input but has no output"),
                },
                Err(e) => return Err(e).context("ProcessInput"),
            }
        }
        while let Some(f) = self.pull()? {
            newest = Some(f);
        }
        Ok(newest)
    }
}

impl Drop for MfDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

fn create_gpu() -> Result<Gpu> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device = device.context("no D3D11 device")?;
        // The MFT uses the device from its own threads.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.context("no DXGI device manager")?;
        manager.ResetDevice(&device, token)?;
        Ok(Gpu { manager, device, context: context.context("no D3D11 context")?, staging: None })
    }
}

fn activate_decoder() -> Result<(IMFTransform, String)> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
            &mut list,
            &mut count,
        )
        .context("MFTEnumEx")?;
        if count == 0 || list.is_null() {
            bail!("no H.264 decoder MFT");
        }
        let activates = std::slice::from_raw_parts_mut(list, count as usize);
        let mut result = Err(anyhow::anyhow!("no H.264 decoder could be activated"));
        for slot in activates.iter_mut() {
            let Some(activate) = slot.take() else {
                continue;
            };
            if result.is_ok() {
                continue;
            }
            let name = friendly_name(&activate).unwrap_or_else(|| "H.264 MFT".into());
            match activate.ActivateObject::<IMFTransform>() {
                Ok(t) => result = Ok((t, name)),
                Err(e) => tracing::debug!(%name, "activate failed: {e}"),
            }
        }
        CoTaskMemFree(Some(list as *const _));
        result
    }
}

fn friendly_name(activate: &IMFActivate) -> Option<String> {
    unsafe {
        let mut s = PWSTR::null();
        let mut len = 0;
        activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut s, &mut len).ok()?;
        let name = s.to_string().ok();
        CoTaskMemFree(Some(s.0 as *const _));
        name
    }
}
