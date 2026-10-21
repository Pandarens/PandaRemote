//! Connection ticket: `panda1` + base32(host_endpoint_id[32] || access_secret[32]).
//! The ticket is the only thing a client needs; it stays valid while the host keeps its key and secret.

use anyhow::{Result, bail};
use data_encoding::BASE32_NOPAD;
use iroh::EndpointId;

const PREFIX: &str = "panda1";

#[derive(Clone)]
pub struct Ticket {
    pub host_id: EndpointId,
    pub secret: [u8; 32],
}

impl Ticket {
    pub fn encode(&self) -> String {
        let mut raw = [0u8; 64];
        raw[..32].copy_from_slice(self.host_id.as_bytes());
        raw[32..].copy_from_slice(&self.secret);
        format!("{PREFIX}{}", BASE32_NOPAD.encode(&raw).to_lowercase())
    }

    pub fn decode(s: &str) -> Result<Self> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        let Some(body) = s.strip_prefix(PREFIX) else {
            bail!("ticket must start with `{PREFIX}`");
        };
        let raw = BASE32_NOPAD
            .decode(body.to_uppercase().as_bytes())
            .map_err(|_| anyhow::anyhow!("ticket is not valid base32"))?;
        if raw.len() != 64 {
            bail!("ticket has wrong length");
        }
        let host_id = EndpointId::from_bytes(raw[..32].try_into()?)?;
        Ok(Ticket {
            host_id,
            secret: raw[32..].try_into()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let t = Ticket {
            host_id: iroh::SecretKey::generate().public(),
            secret: [7; 32],
        };
        let back = Ticket::decode(&t.encode()).unwrap();
        assert_eq!(back.host_id, t.host_id);
        assert_eq!(back.secret, t.secret);
        assert!(Ticket::decode("panda1abc").is_err());
    }
}
