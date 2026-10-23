//! Session recording on the viewer: the video exactly as it arrives, without re-encoding.
//! H.264 goes into an MP4 file; HEVC is written as an elementary stream (`.hevc`, plays in VLC/ffplay),
//! because the MP4 writer used here cannot store HEVC parameter sets.

use std::{
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result};
use bytes::Bytes;
use mp4::{AvcConfig, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};

use crate::proto::Codec;

/// Milliseconds: sample times come from the arrival clock.
const TIMESCALE: u32 = 1000;

pub struct Recorder {
    path: PathBuf,
    kind: Kind,
    started: Option<Instant>,
}

enum Kind {
    /// MP4 needs SPS/PPS from the first keyframe before it can be opened.
    Mp4 {
        file: Option<Mp4Writer<BufWriter<File>>>,
        width: u16,
        height: u16,
        pending: Option<(u64, bool, Bytes)>,
    },
    Raw(BufWriter<File>),
}

impl Recorder {
    /// Starts a recording into `dir`; frames are written from the next keyframe on.
    pub fn start(dir: &Path, name: &str, codec: Codec, width: u32, height: u32) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let stamp =
            humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string().replace([':', 'T'], "-");
        let stamp = stamp.trim_end_matches('Z');
        let safe: String = name.chars().map(|c| if r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
        let (ext, kind) = match codec {
            Codec::H264 => ("mp4", None),
            _ => ("hevc", Some(())),
        };
        let path = dir.join(format!("Panda {safe} {stamp}.{ext}"));
        let kind = match kind {
            None => Kind::Mp4 { file: None, width: width as u16, height: height as u16, pending: None },
            Some(()) => {
                Kind::Raw(BufWriter::new(File::create(&path).with_context(|| format!("create {}", path.display()))?))
            }
        };
        Ok(Recorder { path, kind, started: None })
    }

    /// Adds one access unit (Annex B). Frames before the first keyframe are skipped.
    pub fn push(&mut self, au: &[u8], keyframe: bool) -> Result<()> {
        if self.started.is_none() {
            if !keyframe {
                return Ok(());
            }
            self.started = Some(Instant::now());
        }
        let at = self.started.unwrap().elapsed().as_millis() as u64;
        match &mut self.kind {
            Kind::Raw(w) => w.write_all(au)?,
            Kind::Mp4 { file, width, height, pending } => {
                let nals = nal_units(au);
                if file.is_none() {
                    let sps = nals.iter().find(|n| n[0] & 0x1F == 7);
                    let pps = nals.iter().find(|n| n[0] & 0x1F == 8);
                    let (Some(sps), Some(pps)) = (sps, pps) else {
                        // A keyframe without parameter sets: wait for the next one.
                        self.started = None;
                        return Ok(());
                    };
                    let out = BufWriter::new(
                        File::create(&self.path).with_context(|| format!("create {}", self.path.display()))?,
                    );
                    let config = Mp4Config {
                        major_brand: "isom".parse().unwrap(),
                        minor_version: 512,
                        compatible_brands: ["isom", "iso2", "avc1", "mp41"]
                            .iter()
                            .map(|b| b.parse().unwrap())
                            .collect(),
                        timescale: TIMESCALE,
                    };
                    let mut writer = Mp4Writer::write_start(out, &config)?;
                    writer.add_track(&TrackConfig {
                        track_type: TrackType::Video,
                        timescale: TIMESCALE,
                        language: "und".into(),
                        media_conf: MediaConfig::AvcConfig(AvcConfig {
                            width: *width,
                            height: *height,
                            seq_param_set: sps.to_vec(),
                            pic_param_set: pps.to_vec(),
                        }),
                    })?;
                    *file = Some(writer);
                }
                // MP4 stores NAL units with 4-byte lengths; parameter sets and delimiters live in the header.
                let mut body = Vec::with_capacity(au.len() + 16);
                for n in nals.iter().filter(|n| !matches!(n[0] & 0x1F, 7 | 8 | 9)) {
                    body.extend_from_slice(&(n.len() as u32).to_be_bytes());
                    body.extend_from_slice(n);
                }
                // A sample's duration is only known when the next one arrives.
                if let Some((start, sync, bytes)) = pending.replace((at, keyframe, Bytes::from(body))) {
                    let duration = (at.saturating_sub(start)).max(1) as u32;
                    let sample = Mp4Sample { start_time: start, duration, rendering_offset: 0, is_sync: sync, bytes };
                    file.as_mut().unwrap().write_sample(1, &sample)?;
                }
            }
        }
        Ok(())
    }

    /// Finishes the file (MP4 needs its index written at the end).
    pub fn finish(mut self) -> Result<PathBuf> {
        match &mut self.kind {
            Kind::Raw(w) => w.flush()?,
            Kind::Mp4 { file, pending, .. } => {
                if let Some(mut writer) = file.take() {
                    if let Some((start, sync, bytes)) = pending.take() {
                        let sample =
                            Mp4Sample { start_time: start, duration: 33, rendering_offset: 0, is_sync: sync, bytes };
                        writer.write_sample(1, &sample)?;
                    }
                    writer.write_end()?;
                    writer.into_writer().flush()?;
                }
            }
        }
        Ok(self.path)
    }
}

/// NAL units of an Annex B access unit (without start codes).
fn nal_units(au: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(au.len(), |&n| n - 3);
        // A 4-byte start code leaves a trailing zero on the previous unit.
        while end > s && au[end - 1] == 0 && starts.get(k + 1).is_some() {
            end -= 1;
        }
        if end > s {
            nals.push(&au[s..end]);
        }
    }
    nals
}

/// The user's Videos\Panda folder.
pub fn videos_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|d| d.video_dir().map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
        .join("Panda")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b() {
        let au = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
        let nals = nal_units(&au);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]);
    }

    #[test]
    fn mp4_from_real_encoder_output() {
        // Encode a few frames with openh264 and record them; the file must have the MP4 boxes.
        use crate::host::encoder::{self, Picture};
        let (w, h) = (64u32, 64u32);
        let (_, mut enc) = encoder::create(&[Codec::H264], w, h, 30, 1_000_000, false).unwrap();
        let dir = std::env::temp_dir().join(format!("panda-rec-test-{}", std::process::id()));
        let mut rec = Recorder::start(&dir, "тест", Codec::H264, w, h).unwrap();
        let pixels = vec![128u8; (w * h * 4) as usize];
        for i in 0..10 {
            for f in enc.encode(&Picture { pixels: &pixels, width: w, height: h, bgra: true }, i == 0).unwrap() {
                rec.push(&f.data, f.keyframe).unwrap();
            }
        }
        let path = rec.finish().unwrap();
        let data = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(data.windows(4).any(|b| b == b"ftyp") && data.windows(4).any(|b| b == b"moov"));
        assert!(data.windows(4).any(|b| b == b"avcC"));
    }
}
