//! This PC's network card addresses (so the viewer can remember them) and Wake-on-LAN for other PCs.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

use anyhow::{Context, Result};

/// MAC addresses of this PC's network adapters.
pub fn macs() -> Vec<[u8; 6]> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let Ok(out) =
            std::process::Command::new("getmac").args(["/fo", "csv", "/nh"]).creation_flags(CREATE_NO_WINDOW).output()
        else {
            return Vec::new();
        };
        let mut found: Vec<[u8; 6]> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| parse_mac(line.split(',').next()?.trim_matches('"')))
            .collect();
        // Virtual adapters (VMware, Hyper-V, VirtualBox) cannot wake the PC.
        found.retain(|m| {
            !matches!(m[..3], [0x00, 0x50, 0x56] | [0x00, 0x0C, 0x29] | [0x00, 0x15, 0x5D] | [0x08, 0x00, 0x27])
        });
        found.dedup();
        found
    }
    #[cfg(not(windows))]
    Vec::new()
}

/// "AA-BB-CC-DD-EE-FF" (or with colons) -> bytes.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split(['-', ':']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (b, p) in mac.iter_mut().zip(parts) {
        *b = u8::from_str_radix(p, 16).ok()?;
    }
    (mac != [0; 6]).then_some(mac)
}

pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join("-")
}

/// Broadcasts a Wake-on-LAN "magic packet" for `mac` (6 × 0xFF, then the address 16 times) on the
/// local network, to the usual ports 9 and 7.
pub fn wake(mac: [u8; 6]) -> Result<()> {
    let mut packet = vec![0xFF; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).context("UDP socket")?;
    socket.set_broadcast(true)?;
    for port in [9, 7] {
        socket.send_to(&packet, SocketAddrV4::new(Ipv4Addr::BROADCAST, port)).context("send magic packet")?;
    }
    tracing::info!(mac = %format_mac(&mac), "Wake-on-LAN packet sent");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_parsing() {
        assert_eq!(parse_mac("70-85-C2-0A-1B-FF"), Some([0x70, 0x85, 0xC2, 0x0A, 0x1B, 0xFF]));
        assert_eq!(parse_mac("70:85:c2:0a:1b:ff"), Some([0x70, 0x85, 0xC2, 0x0A, 0x1B, 0xFF]));
        assert_eq!(parse_mac("N/A"), None);
        assert_eq!(format_mac(&[0x70, 0x85, 0xC2, 0x0A, 0x1B, 0xFF]), "70-85-C2-0A-1B-FF");
    }
}
