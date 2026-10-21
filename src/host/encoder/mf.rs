//! Hardware H.264 / HEVC / AV1 through a Media Foundation transform (Intel QuickSync, NVIDIA NVENC, AMD AMF).
//! Hardware MFTs are asynchronous: they announce `METransformNeedInput` / `METransformHaveOutput`
//! events, and we drive them from the capture thread without extra threads.

#![allow(non_upper_case_globals)]

use std::mem::ManuallyDrop;

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::VARIANT_BOOL,
        Media::MediaFoundation::*,
        System::{
            Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree},
            Variant::{VARIANT, VT_BOOL, VT_UI4},
        },
    },
    core::{GUID, Interface, PWSTR},
};

use super::{EncodedFrame, Picture, VideoEncoder, to_nv12};
use crate::proto::Codec;

fn subtype(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
        Codec::Av1 => MFVideoFormat_AV1,
    }
}

pub struct MfEncoder {
    transform: IMFTransform,
    /// Present for asynchronous MFTs.
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    input_id: u32,
    output_id: u32,
    fps: u32,
    frame_index: i64,
    need_input: u32,
    nv12: Vec<u8>,
    ready: Vec<EncodedFrame>,
    name: String,
}

fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
    }
    var
}

fn variant_bool(v: bool) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = VARIANT_BOOL(if v { -1 } else { 0 });
    }
    var
}

impl MfEncoder {
    pub fn new(codec: Codec, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        unsafe {
            // S_FALSE / RPC_E_CHANGED_MODE just mean COM is already initialised on this thread.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;
        }
        let (transform, name) = activate_hardware_encoder(codec)?;
        unsafe {
            let attrs = transform.GetAttributes()?;
            // Required before an async MFT accepts any call.
            let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        let events = transform.cast::<IMFMediaEventGenerator>().ok();
        let (input_id, output_id) = stream_ids(&transform);

        unsafe {
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &subtype(codec))?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            out.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            match codec {
                // Baseline: no B-frames (no reordering delay) and decodable by every client.
                Codec::H264 => out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?,
                Codec::Hevc => out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?,
                Codec::Av1 => {}
            }
            transform.SetOutputType(output_id, &out, 0).context("SetOutputType")?;

            let input = find_nv12_input(&transform, input_id)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            input.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(input_id, &input, 0).context("SetInputType")?;
        }

        let codec_api = transform.cast::<ICodecAPI>().ok();
        if let Some(c) = &codec_api {
            unsafe {
                let _ = c.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true));
                let _ = c.SetValue(
                    &CODECAPI_AVEncCommonRateControlMode,
                    &variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
                );
                let _ = c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bitrate));
                // Long GOP: keyframes are requested explicitly after losses.
                let _ = c.SetValue(&CODECAPI_AVEncMPVGOPSize, &variant_u32(fps * 10));
            }
        }

        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        let name = format!("{name} · {}", codec.name());
        tracing::info!(encoder = %name, async_mft = events.is_some(), "hardware encoder ready");
        Ok(MfEncoder {
            transform,
            events,
            codec: codec_api,
            input_id,
            output_id,
            fps,
            frame_index: 0,
            need_input: 0,
            nv12: Vec::new(),
            ready: Vec::new(),
            name,
        })
    }

    /// Blocks for the next MFT event and handles it.
    fn pump_event(&mut self) -> Result<()> {
        let events = self.events.as_ref().expect("async MFT");
        let event = unsafe { events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) }?;
        match MF_EVENT_TYPE(unsafe { event.GetType()? } as i32) {
            METransformNeedInput => self.need_input += 1,
            METransformHaveOutput => {
                self.drain_output()?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Pulls one encoded sample. Returns false when the MFT has nothing more (sync MFTs).
    fn drain_output(&mut self) -> Result<bool> {
        unsafe {
            let info = self.transform.GetOutputStreamInfo(self.output_id)?;
            let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            let sample = if provides {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(1 << 20))?)?;
                Some(s)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_id,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            let sample = ManuallyDrop::take(&mut buffers[0].pSample);
            drop(ManuallyDrop::take(&mut buffers[0].pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let t = self.transform.GetOutputAvailableType(self.output_id, 0)?;
                    self.transform.SetOutputType(self.output_id, &t, 0)?;
                    return Ok(true);
                }
                Err(e) => return Err(e).context("ProcessOutput"),
            }
            let sample = sample.context("encoder returned no sample")?;
            let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            if !data.is_empty() {
                self.ready.push(EncodedFrame { data, keyframe });
            }
            Ok(true)
        }
    }

    fn make_sample(&self) -> Result<IMFSample> {
        unsafe {
            let len = self.nv12.len() as u32;
            let buffer = MFCreateMemoryBuffer(len)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, self.nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            let duration = 10_000_000 / self.fps as i64;
            sample.SetSampleTime(self.frame_index * duration)?;
            sample.SetSampleDuration(duration)?;
            Ok(sample)
        }
    }
}

impl VideoEncoder for MfEncoder {
    fn name(&self) -> String {
        format!("{} (аппаратный)", self.name)
    }

    fn encode(&mut self, pic: &Picture, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        to_nv12(pic, &mut self.nv12);
        if force_keyframe && let Some(c) = &self.codec {
            unsafe {
                let _ = c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
            }
        }
        let sample = self.make_sample()?;
        self.frame_index += 1;

        if self.events.is_some() {
            while self.need_input == 0 {
                self.pump_event()?;
            }
            unsafe { self.transform.ProcessInput(self.input_id, &sample, 0) }.context("ProcessInput")?;
            self.need_input -= 1;
            // In low-latency mode the output for this frame arrives before the next input request;
            // if the encoder asks for more input first, the frame is picked up on the next call.
            while self.ready.is_empty() && self.need_input == 0 {
                self.pump_event()?;
            }
        } else {
            unsafe { self.transform.ProcessInput(self.input_id, &sample, 0) }.context("ProcessInput")?;
            while self.drain_output()? {}
        }
        Ok(std::mem::take(&mut self.ready))
    }

    fn set_bitrate(&mut self, bps: u32) {
        if let Some(c) = &self.codec {
            unsafe {
                let _ = c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(bps));
            }
        }
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Some(generator) = self.events.take()
                && let Ok(shutdown) = generator.cast::<IMFShutdown>()
            {
                let _ = shutdown.Shutdown();
            }
        }
    }
}

/// Hardware encoders present for each codec (diagnostics).
pub fn available() -> Vec<(Codec, String)> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    }
    Codec::PREFERENCE
        .into_iter()
        .flat_map(|c| {
            enum_names(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE, Some(MFVideoFormat_NV12), Some(subtype(c)))
                .into_iter()
                .map(move |n| (c, n))
        })
        .collect()
}

/// Friendly names of the MFTs matching a category and types.
fn enum_names(category: GUID, flags: MFT_ENUM_FLAG, input: Option<GUID>, output: Option<GUID>) -> Vec<String> {
    let input = input.map(|g| MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: g });
    let output = output.map(|g| MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: g });
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let mut names = Vec::new();
    unsafe {
        if MFTEnumEx(
            category,
            flags | MFT_ENUM_FLAG_SORTANDFILTER,
            input.as_ref().map(|i| i as *const _),
            output.as_ref().map(|o| o as *const _),
            &mut list,
            &mut count,
        )
        .is_err()
            || list.is_null()
        {
            return names;
        }
        for slot in std::slice::from_raw_parts_mut(list, count as usize) {
            if let Some(a) = slot.take() {
                names.push(friendly_name(&a).unwrap_or_else(|| "MFT".into()));
            }
        }
        CoTaskMemFree(Some(list as *const _));
    }
    names
}

fn activate_hardware_encoder(codec: Codec) -> Result<(IMFTransform, String)> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec) };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )
        .context("MFTEnumEx")?;
        if count == 0 || list.is_null() {
            bail!("no hardware {} encoder", codec.name());
        }
        let activates = std::slice::from_raw_parts_mut(list, count as usize);
        let mut result = Err(anyhow::anyhow!("no hardware {} encoder could be activated", codec.name()));
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

fn stream_ids(t: &IMFTransform) -> (u32, u32) {
    let mut input = [0u32];
    let mut output = [0u32];
    match unsafe { t.GetStreamIDs(&mut input, &mut output) } {
        Ok(()) => (input[0], output[0]),
        // E_NOTIMPL: fixed streams numbered from 0.
        Err(_) => (0, 0),
    }
}

fn find_nv12_input(t: &IMFTransform, input_id: u32) -> Result<IMFMediaType> {
    for i in 0.. {
        let Ok(ty) = (unsafe { t.GetInputAvailableType(input_id, i) }) else {
            break;
        };
        let subtype: GUID = unsafe { ty.GetGUID(&MF_MT_SUBTYPE) }?;
        if subtype == MFVideoFormat_NV12 {
            return Ok(ty);
        }
    }
    // Some MFTs list nothing until the output type is set; build the type ourselves.
    let ty = unsafe { MFCreateMediaType()? };
    unsafe {
        ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
    }
    Ok(ty)
}
