//! Video frames over unreliable datagrams: each frame is cut into equal shards and protected with
//! Reed-Solomon parity shards, so the receiver rebuilds it from any `data` of the `data + parity` shards.
//!
//! Shard layout: `type u8 | frame_id u32 | idx u16 | data u16 | parity u16 | frame_len u32 | flags u8 | payload`.

use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use bytes::{BufMut, Bytes, BytesMut};

pub const DGRAM_VIDEO: u8 = 0x01;
const HEADER: usize = 16;
const FLAG_KEYFRAME: u8 = 1;
/// Incomplete frames older than this are given up on.
const STALE: Duration = Duration::from_secs(1);

/// Splits an encoded frame into datagrams no larger than `max_datagram`, adding `parity_percent` parity.
pub fn packetize(
    frame_id: u32,
    data: &[u8],
    keyframe: bool,
    max_datagram: usize,
    parity_percent: u32,
) -> Result<Vec<Bytes>> {
    // Reed-Solomon shards must have an even, equal size.
    let shard = (max_datagram.saturating_sub(HEADER)) & !1;
    if shard < 64 {
        bail!("datagram size too small: {max_datagram}");
    }
    let data_count = data.len().div_ceil(shard).max(1);
    let parity_count = (data_count * parity_percent as usize).div_ceil(100).max(1);
    if data_count + parity_count > u16::MAX as usize {
        bail!("frame too large: {} bytes", data.len());
    }

    let mut shards: Vec<Vec<u8>> = data.chunks(shard).map(|c| c.to_vec()).collect();
    if shards.is_empty() {
        shards.push(Vec::new());
    }
    for s in &mut shards {
        s.resize(shard, 0);
    }
    let parity = reed_solomon_simd::encode(data_count, parity_count, &shards)?;

    let flags = if keyframe { FLAG_KEYFRAME } else { 0 };
    let out = shards
        .iter()
        .chain(parity.iter())
        .enumerate()
        .map(|(idx, payload)| {
            let mut b = BytesMut::with_capacity(HEADER + payload.len());
            b.put_u8(DGRAM_VIDEO);
            b.put_u32(frame_id);
            b.put_u16(idx as u16);
            b.put_u16(data_count as u16);
            b.put_u16(parity_count as u16);
            b.put_u32(data.len() as u32);
            b.put_u8(flags);
            b.put_slice(payload);
            b.freeze()
        })
        .collect();
    Ok(out)
}

pub struct Frame {
    pub keyframe: bool,
    pub data: Vec<u8>,
    /// Frames between the previous delivered one and this one were lost.
    pub after_gap: bool,
}

struct Partial {
    data_count: usize,
    parity_count: usize,
    len: usize,
    keyframe: bool,
    shards: HashMap<usize, Bytes>,
    created: Instant,
}

#[derive(Default)]
pub struct Reassembler {
    partial: BTreeMap<u32, Partial>,
    last_delivered: Option<u32>,
    /// Frames that never completed, for statistics.
    pub lost: u32,
}

impl Reassembler {
    pub fn push(&mut self, dgram: Bytes) -> Option<Frame> {
        if dgram.len() < HEADER || dgram[0] != DGRAM_VIDEO {
            return None;
        }
        let id = u32::from_be_bytes(dgram[1..5].try_into().ok()?);
        let idx = u16::from_be_bytes([dgram[5], dgram[6]]) as usize;
        let data_count = u16::from_be_bytes([dgram[7], dgram[8]]) as usize;
        let parity_count = u16::from_be_bytes([dgram[9], dgram[10]]) as usize;
        let len = u32::from_be_bytes(dgram[11..15].try_into().ok()?) as usize;
        let keyframe = dgram[15] & FLAG_KEYFRAME != 0;
        if self.last_delivered.is_some_and(|last| id <= last) || data_count == 0 || idx >= data_count + parity_count {
            return None; // late shard of a frame already delivered or given up
        }

        let now = Instant::now();
        let p = self.partial.entry(id).or_insert_with(|| Partial {
            data_count,
            parity_count,
            len,
            keyframe,
            shards: HashMap::new(),
            created: now,
        });
        p.shards.entry(idx).or_insert_with(|| dgram.slice(HEADER..));
        if p.shards.len() < p.data_count {
            self.drop_stale(now);
            return None;
        }

        let p = self.partial.remove(&id)?;
        let data = rebuild(&p)?;
        let after_gap = match self.last_delivered {
            Some(last) => id != last.wrapping_add(1),
            None => false,
        };
        if let Some(last) = self.last_delivered {
            self.lost += id.wrapping_sub(last).saturating_sub(1);
        }
        self.last_delivered = Some(id);
        // Anything older is now unusable.
        self.partial.retain(|&k, _| k > id);
        Some(Frame { keyframe: p.keyframe, data, after_gap })
    }

    fn drop_stale(&mut self, now: Instant) {
        self.partial.retain(|_, p| now.duration_since(p.created) < STALE);
    }
}

fn rebuild(p: &Partial) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(p.data_count * p.shards.values().next()?.len());
    if (0..p.data_count).all(|i| p.shards.contains_key(&i)) {
        for i in 0..p.data_count {
            out.extend_from_slice(&p.shards[&i]);
        }
    } else {
        let originals = p.shards.iter().filter(|(i, _)| **i < p.data_count).map(|(i, s)| (*i, s));
        let recovery = p.shards.iter().filter(|(i, _)| **i >= p.data_count).map(|(i, s)| (*i - p.data_count, s));
        let restored = reed_solomon_simd::decode(p.data_count, p.parity_count, originals, recovery).ok()?;
        for i in 0..p.data_count {
            match p.shards.get(&i) {
                Some(s) => out.extend_from_slice(s),
                None => out.extend_from_slice(restored.get(&i)?),
            }
        }
    }
    out.truncate(p.len);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn roundtrip_without_loss() {
        let data = sample(10_000);
        let shards = packetize(1, &data, true, 1200, 20).unwrap();
        let mut r = Reassembler::default();
        let frame = shards.into_iter().find_map(|s| r.push(s)).unwrap();
        assert_eq!(frame.data, data);
        assert!(frame.keyframe);
    }

    #[test]
    fn recovers_lost_shards() {
        let data = sample(50_000);
        let shards = packetize(7, &data, false, 1200, 20).unwrap();
        let mut r = Reassembler::default();
        // Lose every 6th shard (~16 %), below the 20 % parity.
        let frame = shards.into_iter().enumerate().filter(|(i, _)| i % 6 != 0).find_map(|(_, s)| r.push(s)).unwrap();
        assert_eq!(frame.data, data);
    }

    #[test]
    fn counts_unrecoverable_frames_as_lost() {
        let mut r = Reassembler::default();
        let a = packetize(1, &sample(3000), true, 1200, 20).unwrap();
        let b = packetize(2, &sample(3000), false, 1200, 20).unwrap();
        let c = packetize(3, &sample(3000), false, 1200, 20).unwrap();
        assert!(a.into_iter().find_map(|s| r.push(s)).is_some());
        r.push(b[0].clone()); // frame 2 never completes
        let f = c.into_iter().find_map(|s| r.push(s)).unwrap();
        assert!(f.after_gap);
        assert_eq!(r.lost, 1);
        assert!(r.push(b[1].clone()).is_none(), "late shards are ignored");
    }

    #[test]
    fn tiny_frame() {
        let shards = packetize(1, b"x", false, 1200, 20).unwrap();
        assert_eq!(shards.len(), 2);
        let mut r = Reassembler::default();
        assert_eq!(r.push(shards[1].clone()).unwrap().data, b"x");
    }
}
