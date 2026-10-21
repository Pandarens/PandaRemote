//! Game streaming tunnel: carries Moonlight <-> Sunshine traffic over the authenticated P2P connection.
//!
//! The client side listens on 127.0.0.1 at Sunshine's ports, so Moonlight is pointed at `127.0.0.1`;
//! the host side forwards to the local Sunshine. TCP ports map to QUIC bi-streams, UDP ports to QUIC
//! datagrams (fragmented, since Moonlight's LAN packets exceed the datagram MTU).

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU16, AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use bytes::{BufMut, Bytes, BytesMut};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream, UdpSocket},
    task::JoinSet,
};

use crate::proto::{self, TcpOpen};

const LOCALHOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
const HEADER: usize = 6;

/// Sunshine/GameStream port offsets from the base (HTTP) port, see Sunshine `nvhttp.h`/`stream.h`/`rtsp.h`.
/// The wire carries offsets, so client and host may use different base ports.
const TCP_OFFSETS: [i16; 4] = [-5, 0, 1, 21]; // HTTPS, HTTP, Web UI, RTSP
const UDP_OFFSETS: [i16; 4] = [9, 10, 11, 13]; // video, control, audio, mic

fn port_at(base: u16, offset: i16) -> u16 {
    (base as i32 + offset as i32) as u16
}

#[derive(Default)]
pub struct TunnelStats {
    pub up: AtomicU64,
    pub down: AtomicU64,
}

// ---------- host side ----------

pub async fn serve_host(conn: &Connection, mut control: RecvStream, base: u16) -> Result<()> {
    let mut tasks = JoinSet::new();

    // TCP: each bi-stream opened by the client is one TCP connection to Sunshine.
    {
        let conn = conn.clone();
        tasks.spawn(async move {
            loop {
                let (send, mut recv) = conn.accept_bi().await?;
                tokio::spawn(async move {
                    let res = async {
                        let TcpOpen { offset } = proto::read_msg(&mut recv).await?;
                        if !TCP_OFFSETS.contains(&offset) {
                            bail!("port offset {offset} is not tunnelled");
                        }
                        let port = port_at(base, offset);
                        let tcp = TcpStream::connect((LOCALHOST, port))
                            .await
                            .with_context(|| format!("Sunshine не отвечает на порту {port}"))?;
                        pipe(tcp, send, recv, None).await
                    }
                    .await;
                    if let Err(e) = res {
                        tracing::debug!("tcp tunnel stream: {e:#}");
                    }
                });
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        });
    }

    // UDP: one local socket per Sunshine port, created on first packet.
    {
        let conn = conn.clone();
        tasks.spawn(async move {
            let ids = Arc::new(AtomicU16::new(0));
            let mut sockets: HashMap<i16, Arc<UdpSocket>> = HashMap::new();
            let mut reasm = Reassembler::default();
            loop {
                let dgram = conn.read_datagram().await?;
                let Some((offset, payload)) = reasm.push(dgram) else { continue };
                if !UDP_OFFSETS.contains(&offset) {
                    continue;
                }
                let sock = match sockets.get(&offset) {
                    Some(s) => s.clone(),
                    None => {
                        let s = Arc::new(udp_bind(SocketAddr::from((LOCALHOST, 0)))?);
                        s.connect((LOCALHOST, port_at(base, offset))).await?;
                        sockets.insert(offset, s.clone());
                        let (s2, conn2, ids2) = (s.clone(), conn.clone(), ids.clone());
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 65536];
                            while let Ok(n) = s2.recv(&mut buf).await {
                                if send_fragmented(&conn2, &ids2, offset, &buf[..n]).is_err() {
                                    break;
                                }
                            }
                        });
                        s
                    }
                };
                let _ = sock.send(&payload).await;
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        });
    }

    // The session lasts until the client closes its control stream or the connection drops.
    tokio::select! {
        _ = control.read_to_end(1024) => {}
        _ = conn.closed() => {}
        Some(r) = tasks.join_next() => { r??; }
    }
    Ok(())
}

// ---------- client side ----------

/// Binds Sunshine's ports on 127.0.0.1 and forwards them until the connection ends.
pub async fn run_client(conn: Connection, control: SendStream, base: u16, stats: Arc<TunnelStats>) -> Result<()> {
    let mut tasks = JoinSet::new();

    for offset in TCP_OFFSETS {
        let port = port_at(base, offset);
        let listener = TcpListener::bind((LOCALHOST, port))
            .await
            .with_context(|| format!("порт {port} занят (запущен локальный Sunshine/другой туннель?)"))?;
        let (conn, stats) = (conn.clone(), stats.clone());
        tasks.spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await?;
                let _ = tcp.set_nodelay(true);
                let (conn, stats) = (conn.clone(), stats.clone());
                tokio::spawn(async move {
                    let res = async {
                        let (mut send, recv) = conn.open_bi().await?;
                        proto::write_msg(&mut send, &TcpOpen { offset }).await?;
                        pipe(tcp, send, recv, Some(stats)).await
                    }
                    .await;
                    if let Err(e) = res {
                        tracing::debug!("tcp tunnel stream: {e:#}");
                    }
                });
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        });
    }

    let ids = Arc::new(AtomicU16::new(0));
    let mut sockets: HashMap<i16, (Arc<UdpSocket>, Arc<Mutex<Option<SocketAddr>>>)> = HashMap::new();
    for offset in UDP_OFFSETS {
        let port = port_at(base, offset);
        let sock = Arc::new(udp_bind(SocketAddr::from((LOCALHOST, port))).with_context(|| format!("UDP порт {port} занят"))?);
        let peer = Arc::new(Mutex::new(None));
        sockets.insert(offset, (sock.clone(), peer.clone()));
        let (conn, ids, stats) = (conn.clone(), ids.clone(), stats.clone());
        tasks.spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                let (n, from) = sock.recv_from(&mut buf).await?;
                *peer.lock().unwrap() = Some(from);
                stats.up.fetch_add(n as u64, Ordering::Relaxed);
                send_fragmented(&conn, &ids, offset, &buf[..n])?;
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        });
    }
    {
        let (conn, stats) = (conn.clone(), stats.clone());
        tasks.spawn(async move {
            let mut reasm = Reassembler::default();
            loop {
                let dgram = conn.read_datagram().await?;
                let Some((offset, payload)) = reasm.push(dgram) else { continue };
                let Some((sock, peer)) = sockets.get(&offset) else { continue };
                let target = *peer.lock().unwrap();
                if let Some(target) = target {
                    stats.down.fetch_add(payload.len() as u64, Ordering::Relaxed);
                    let _ = sock.send_to(&payload, target).await;
                }
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        });
    }

    let _control = control; // dropping it ends the session on the host
    tokio::select! {
        e = conn.closed() => bail!("соединение закрыто: {e}"),
        Some(r) = tasks.join_next() => { r??; }
    }
    Ok(())
}

// ---------- shared helpers ----------

/// Video arrives in bursts of ~100 packets per frame; Windows' default 64 KiB socket buffer drops them.
const UDP_BUFFER: usize = 4 * 1024 * 1024;

fn udp_bind(addr: SocketAddr) -> Result<UdpSocket> {
    let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    let _ = sock.set_recv_buffer_size(UDP_BUFFER);
    let _ = sock.set_send_buffer_size(UDP_BUFFER);
    sock.bind(&addr.into())?;
    sock.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(sock.into())?)
}

async fn pipe(tcp: TcpStream, mut send: SendStream, mut recv: RecvStream, stats: Option<Arc<TunnelStats>>) -> Result<()> {
    let (mut tr, mut tw) = tcp.into_split();
    let up = async {
        let n = tokio::io::copy(&mut tr, &mut send).await?;
        send.finish()?;
        anyhow::Ok(n)
    };
    let down = async {
        let n = tokio::io::copy(&mut recv, &mut tw).await?;
        tw.shutdown().await?;
        anyhow::Ok(n)
    };
    let (up, down) = tokio::try_join!(up, down)?;
    if let Some(s) = stats {
        s.up.fetch_add(up, Ordering::Relaxed);
        s.down.fetch_add(down, Ordering::Relaxed);
    }
    Ok(())
}

/// Datagram layout: port offset i16 | message id u16 | fragment index u8 | fragment count u8 | payload.
fn send_fragmented(conn: &Connection, ids: &AtomicU16, offset: i16, payload: &[u8]) -> Result<()> {
    let max = conn.max_datagram_size().context("peer does not support datagrams")?;
    let chunk = max.saturating_sub(HEADER).max(1);
    let count = payload.len().div_ceil(chunk).max(1);
    if count > u8::MAX as usize {
        bail!("UDP packet too large: {}", payload.len());
    }
    let id = ids.fetch_add(1, Ordering::Relaxed);
    for (i, part) in payload.chunks(chunk).enumerate().chain(
        // keep zero-length packets
        (payload.is_empty()).then_some((0, &[][..])),
    ) {
        let mut b = BytesMut::with_capacity(HEADER + part.len());
        b.put_i16(offset);
        b.put_u16(id);
        b.put_u8(i as u8);
        b.put_u8(count as u8);
        b.put_slice(part);
        // Dropping on a full send buffer is the right behaviour for real-time UDP.
        if let Err(e) = conn.send_datagram(b.freeze()) {
            let s = e.to_string();
            if s.contains("closed") || s.contains("lost") {
                bail!("{s}");
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct Reassembler {
    partial: HashMap<i16, Partial>,
}

struct Partial {
    id: u16,
    parts: Vec<Option<Bytes>>,
    received: usize,
}

impl Reassembler {
    /// Returns `(port offset, payload)` once all fragments of a packet have arrived.
    /// Only the newest packet per port is kept: a lost fragment simply drops that packet.
    fn push(&mut self, dgram: Bytes) -> Option<(i16, Bytes)> {
        if dgram.len() < HEADER {
            return None;
        }
        let port = i16::from_be_bytes([dgram[0], dgram[1]]);
        let id = u16::from_be_bytes([dgram[2], dgram[3]]);
        let (idx, count) = (dgram[4] as usize, dgram[5] as usize);
        let body = dgram.slice(HEADER..);
        if count <= 1 {
            return Some((port, body));
        }
        if idx >= count {
            return None;
        }
        let p = self.partial.entry(port).or_insert_with(|| Partial { id, parts: Vec::new(), received: 0 });
        if p.id != id || p.parts.len() != count {
            *p = Partial { id, parts: vec![None; count], received: 0 };
        }
        if p.parts[idx].is_none() {
            p.parts[idx] = Some(body);
            p.received += 1;
        }
        if p.received < count {
            return None;
        }
        let mut out = BytesMut::new();
        for part in p.parts.drain(..).flatten() {
            out.put(part);
        }
        self.partial.remove(&port);
        Some((port, out.freeze()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frag(port: i16, id: u16, idx: u8, cnt: u8, body: &[u8]) -> Bytes {
        let mut b = BytesMut::new();
        b.put_i16(port);
        b.put_u16(id);
        b.put_u8(idx);
        b.put_u8(cnt);
        b.put_slice(body);
        b.freeze()
    }

    #[test]
    fn reassembles_out_of_order() {
        let mut r = Reassembler::default();
        assert!(r.push(frag(1, 5, 1, 2, b"world")).is_none());
        let (port, data) = r.push(frag(1, 5, 0, 2, b"hello ")).unwrap();
        assert_eq!(port, 1);
        assert_eq!(&data[..], b"hello world");
    }

    #[test]
    fn newer_packet_drops_incomplete_one() {
        let mut r = Reassembler::default();
        assert!(r.push(frag(1, 5, 0, 2, b"a")).is_none());
        assert!(r.push(frag(1, 6, 0, 2, b"b")).is_none());
        assert_eq!(&r.push(frag(1, 6, 1, 2, b"c")).unwrap().1[..], b"bc");
    }

    #[test]
    fn single_fragment_passes_through() {
        let mut r = Reassembler::default();
        assert_eq!(&r.push(frag(9, 0, 0, 1, b"x")).unwrap().1[..], b"x");
    }
}
