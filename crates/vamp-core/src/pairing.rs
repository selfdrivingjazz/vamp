//! Single-use invitation codes for direct Tailscale IPv4 connections. The code carries
//! the endpoint and an ephemeral secret; no network discovery or permanent token is needed.
use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::Path;
use std::time::Duration;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const PREFIX: u32 = 0x6440_0000;
const HOST_MASK: u32 = 0x003f_ffff;
const CHARS: usize = 18;

pub fn encode(addr: SocketAddrV4, secret: [u8; 6]) -> Result<String> {
    let ip = u32::from(*addr.ip());
    ensure!(ip & !HOST_MASK == PREFIX && addr.port() != 0, "pairing requires a Tailscale IPv4 listener");
    let mut value = (u128::from(ip & HOST_MASK) << 64) | (u128::from(addr.port()) << 48);
    for (i, byte) in secret.into_iter().enumerate() {
        value |= u128::from(byte) << ((5 - i) * 8);
    }
    let mut code = String::with_capacity(CHARS + 2);
    for i in 0..CHARS {
        if i == 6 || i == 12 { code.push('-'); }
        code.push(ALPHABET[((value >> ((CHARS - 1 - i) * 5)) & 31) as usize] as char);
    }
    Ok(code)
}

pub fn decode(code: &str) -> Option<(SocketAddrV4, [u8; 6])> {
    if code.len() > 64 { return None; }
    let mut value = 0u128;
    let mut count = 0;
    for ch in code.bytes().filter(|b| *b != b'-' && !b.is_ascii_whitespace()) {
        if count == CHARS { return None; }
        let ch = match ch.to_ascii_uppercase() { b'O' => b'0', b'I' | b'L' => b'1', ch => ch };
        value = (value << 5) | ALPHABET.iter().position(|b| *b == ch)? as u128;
        count += 1;
    }
    let ip = value >> 64;
    let port = (value >> 48) as u16;
    if count != CHARS || ip > u128::from(HOST_MASK) || port == 0 { return None; }
    let addr = SocketAddrV4::new(Ipv4Addr::from(PREFIX | ip as u32), port);
    Some((addr, (value as u64).to_be_bytes()[2..].try_into().ok()?))
}

pub(crate) fn is_direct_tailnet_url(url: &str) -> bool {
    url.trim_end_matches('/').strip_prefix("http://")
        .and_then(|addr| addr.parse::<SocketAddrV4>().ok())
        .is_some_and(|addr| u32::from(*addr.ip()) & !HOST_MASK == PREFIX)
}

#[derive(Deserialize)]
struct Credential { token: String }

/// Redeem the invitation without redirects or proxies, then reuse the normal replica attach.
/// The server returns a device token in the body, never the authority's own token.
pub fn join(
    home: &Path, code: &str, device: &str, name: Option<&str>,
    drive_roots: std::collections::BTreeMap<String, String>,
) -> Result<(String, crate::Context)> {
    let (addr, secret) = decode(code).context("Invalid pairing code.")?;
    let base = format!("http://{addr}");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(8)))
        .build().into();
    let response = agent.post(format!("{base}/v1/pairing/redeem"))
        .send_json(serde_json::json!({"code":encode(addr, secret)?, "device":device}));
    let mut response = match response {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(401 | 429)) => anyhow::bail!("That code is no longer valid. Get a new one on the Mac."),
        Err(_) => anyhow::bail!("Can’t reach the library. Check Tailscale on both devices."),
    };
    let credential: Credential = response.body_mut().with_config().limit(4096).read_json()
        .context("Invalid pairing response.")?;
    ensure!(credential.token.len() == 64 && credential.token.bytes().all(|b| b.is_ascii_hexdigit()), "Invalid pairing response.");
    crate::remote::join(home, &base, &credential.token, name, drive_roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_contains_port_and_secret_and_accepts_typing_variants() {
        for (addr, secret) in [("100.64.0.1:7777", [0; 6]), ("100.127.255.254:65535", [255; 6])] {
            let addr = addr.parse().unwrap();
            let code = encode(addr, secret).unwrap();
            assert_eq!(decode(&code), Some((addr, secret)));
            assert_eq!(decode(&code.to_lowercase().replace('0', "o").replace('-', " ")), Some((addr, secret)));
        }
        assert!(encode("192.168.1.4:7777".parse().unwrap(), [0; 6]).is_err());
        assert!(decode("ZZZZZZ-ZZZZZZ-ZZZZZZ").is_none());
        assert!(decode("000000-000000-000000").is_none());
    }
}
