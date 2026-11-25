//! Pairing is opt-in and only served on the actual Tailscale listener. Creating an
//! invitation requires the owner bearer and a connection from the authority itself.
use crate::{App, HttpError, Reply, error_reply, header, json_reply, read_body, request_header};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::net::{IpAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tiny_http::{Method, Request};
use uuid::Uuid;

const LIFETIME: Duration = Duration::from_secs(180);
const ATTEMPTS: u8 = 12;
const MAX_DEVICES: usize = 128;

pub(crate) fn equals(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

struct Invite {
    secret: [u8; 6],
    expires: Instant,
    remaining: u8,
}

#[derive(Clone, Deserialize, Serialize)]
struct Device {
    id: String,
    name: String,
    last_seen: i64,
    token_hash: String,
}

#[derive(Default)]
struct State {
    invite: Option<Invite>,
    devices: Vec<Device>,
}

pub(crate) struct Pairing {
    addr: SocketAddrV4,
    path: PathBuf,
    state: Mutex<State>,
}

impl Pairing {
    pub(crate) fn open(dir: &Path, addr: SocketAddrV4) -> Result<Self> {
        vamp_core::pairing::encode(addr, [0; 6])?;
        let path = dir.join("paired-devices.json");
        let devices = match fs::File::open(&path) {
            Ok(file) => serde_json::from_reader(file)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self { addr, path, state: Mutex::new(State { invite: None, devices }) })
    }

    fn issue(&self, now: Instant) -> Result<String> {
        let secret = Uuid::new_v4().as_bytes()[..6].try_into().expect("6 bytes");
        let code = vamp_core::pairing::encode(self.addr, secret)?;
        self.state.lock().unwrap().invite = Some(Invite { secret, expires: now + LIFETIME, remaining: ATTEMPTS });
        Ok(code)
    }

    fn redeem(&self, code: &str, name: &str, now: Instant) -> Result<Option<String>> {
        let mut state = self.state.lock().unwrap();
        let Some(invite) = state.invite.as_mut() else { return Ok(None) };
        if now >= invite.expires || invite.remaining == 0 { return Ok(None) }
        invite.remaining -= 1;
        if !vamp_core::pairing::decode(code)
            .is_some_and(|(addr, secret)| addr == self.addr && equals(&secret, &invite.secret)) {
            return Ok(None);
        }
        anyhow::ensure!(state.devices.len() < MAX_DEVICES, "too many paired devices");
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let device = Device {
            id: Uuid::new_v4().to_string(), name: name.to_owned(), last_seen: chrono::Utc::now().timestamp(),
            token_hash: blake3::hash(token.as_bytes()).to_hex().to_string(),
        };
        state.devices.push(device);
        if let Err(error) = self.save(&state.devices) {
            state.devices.pop();
            return Err(error);
        }
        state.invite = None;
        Ok(Some(token))
    }

    pub(crate) fn accepts(&self, token: &str, peer: Option<IpAddr>) -> bool {
        if !peer.is_some_and(crate::auth::tailnet_ip) || token.len() != 64 { return false; }
        let hash = blake3::hash(token.as_bytes()).to_hex();
        let mut state = self.state.lock().unwrap();
        let Some(index) = state.devices.iter().position(|d| equals(d.token_hash.as_bytes(), hash.as_bytes())) else { return false };
        let now = chrono::Utc::now().timestamp();
        if now - state.devices[index].last_seen >= 60 {
            state.devices[index].last_seen = now;
            if let Err(error) = self.save(&state.devices) { eprintln!("vamp-server: saving device status: {error:#}"); }
        }
        true
    }

    fn save(&self, devices: &[Device]) -> Result<()> {
        let temp = self.path.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let result = (|| -> Result<()> {
            let mut file = options.open(&temp)?;
            serde_json::to_writer(&mut file, devices)?;
            file.sync_all()?;
            fs::rename(&temp, &self.path)?;
            Ok(())
        })();
        if result.is_err() { let _ = fs::remove_file(temp); }
        result
    }
}

pub(crate) fn route(app: &App, request: &mut Request) -> Option<Reply> {
    let path = request.url();
    if !matches!(path, "/v1/pairing/code" | "/v1/pairing/redeem" | "/v1/pairing/devices") { return None; }
    let Some(pairing) = &app.pairing else { return Some(error_reply(404, "Pairing is off.")); };
    let peer = request.remote_addr().map(|a| a.ip());
    let owner = peer == Some(IpAddr::V4(*pairing.addr.ip()))
        && request_header(request, "Authorization").and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|token| equals(token.as_bytes(), app.token.as_bytes()));
    let mut reply = match (request.method(), path) {
        (&Method::Post, "/v1/pairing/code") if owner => match pairing.issue(Instant::now()) {
            Ok(code) => json_reply(200, &serde_json::json!({
                "code":code, "qr_payload":format!("vampire://join?code={code}"),
                "expires_at":chrono::Utc::now().timestamp() + LIFETIME.as_secs() as i64,
            })),
            Err(_) => error_reply(500, "Couldn’t create a code."),
        },
        (&Method::Get, "/v1/pairing/devices") if owner => {
            let state = pairing.state.lock().unwrap();
            let devices: Vec<_> = state.devices.iter().map(|d| serde_json::json!({"id":d.id,"name":d.name,"last_seen":d.last_seen})).collect();
            json_reply(200, &serde_json::json!({"devices":devices}))
        }
        (&Method::Post, "/v1/pairing/redeem") if peer.is_some_and(crate::auth::tailnet_ip) => {
            redeem(pairing, request).unwrap_or_else(HttpError::reply)
        }
        _ => error_reply(401, "Pairing requires a direct Tailscale connection."),
    };
    reply.add_header(header("Cache-Control", "no-store"));
    Some(reply)
}

fn redeem(pairing: &Pairing, request: &mut Request) -> Result<Reply, HttpError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Redeem { code: String, device: String }
    let request: Redeem = serde_json::from_slice(&read_body(request, 1024)?)
        .map_err(|_| HttpError::new(400, "Invalid pairing request.".into()))?;
    let device = request.device.trim();
    if device.is_empty() || device.len() > 128 || device.chars().any(char::is_control) {
        return Err(HttpError::new(400, "Invalid device name.".into()));
    }
    match pairing.redeem(&request.code, device, Instant::now()) {
        Ok(Some(token)) => Ok(json_reply(200, &serde_json::json!({"token":token}))),
        Ok(None) => Ok(error_reply(401, "That code is no longer valid. Get a new one on the Mac.")),
        Err(error) => {
            eprintln!("vamp-server: redeeming pairing code: {error:#}");
            Ok(error_reply(500, "Couldn’t save this device."))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_attempt_limit_replacement_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let addr = "100.96.0.1:7791".parse().unwrap();
        let pairing = Pairing::open(dir.path(), addr).unwrap();
        let now = Instant::now();
        let expired = pairing.issue(now).unwrap();
        assert!(pairing.redeem(&expired, "Other Mac", now + LIFETIME).unwrap().is_none());
        let replaced = pairing.issue(now).unwrap();
        let exhausted = pairing.issue(now).unwrap();
        for _ in 0..ATTEMPTS {
            assert!(pairing.redeem(&replaced, "Other Mac", now).unwrap().is_none());
        }
        assert!(pairing.redeem(&exhausted, "Other Mac", now).unwrap().is_none());
        let live = pairing.issue(now).unwrap();
        let token = pairing.redeem(&live, "Other Mac", now).unwrap().unwrap();
        assert!(pairing.redeem(&live, "Other Mac", now).unwrap().is_none());
        assert!(!fs::read_to_string(&pairing.path).unwrap().contains(&token));
        let again = Pairing::open(dir.path(), addr).unwrap();
        assert!(again.accepts(&token, Some("100.64.0.2".parse().unwrap())));
        assert!(!again.accepts(&token, Some("192.168.0.2".parse().unwrap())));
        assert_eq!(again.state.lock().unwrap().devices[0].name, "Other Mac");
    }
}
