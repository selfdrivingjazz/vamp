//! Per-user running-server registry and optional link-local DNS-SD announcements.
use std::env;
use std::fs::{self, OpenOptions};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use mdns_sd::{IfKind, IfPredicate, ServiceDaemon, ServiceInfo};
use serde::Serialize;
use vamp_core::Library;

const SERVICE_TYPE: &str = "_vamp._tcp.local.";

#[derive(Serialize)]
pub(crate) struct Identity {
    version: u8,
    library_id: String,
    library_name: String,
    host: String,
    pub(crate) auth: &'static str,
}

impl Identity {
    pub(crate) fn new(library: &Library, context: &str) -> Self {
        let manifest = fs::read(library.state_dir.join("library.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        let library_name = manifest
            .as_ref()
            .and_then(|value| value["display_name"].as_str())
            .unwrap_or(context)
            .to_owned();
        Self {
            version: 1,
            library_id: library.context.library_id.clone(),
            library_name,
            host: hostname(),
            auth: "token",
        }
    }
}

fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // gethostname may omit the NUL on truncation; inspect only the bytes it returned.
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let end = buf.iter().position(|&byte| byte == 0).unwrap_or(buf.len());
            if end > 0 {
                return String::from_utf8_lossy(&buf[..end]).into_owned();
            }
        }
    }
    env::var("COMPUTERNAME").unwrap_or_else(|_| "VAMP server".into())
}

fn registry_path(state: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(state) = state.filter(|p| p.is_absolute()) {
        return Some(state.join("vamp/servers"));
    }
    let home = home.filter(|p| p.is_absolute())?;
    Some(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/VAMP/servers")
    } else {
        home.join(".local/state/vamp/servers")
    })
}

fn usable_local_addr(mut addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() {
        addr.set_ip(match addr {
            SocketAddr::V4(_) => Ipv4Addr::LOCALHOST.into(),
            SocketAddr::V6(_) => Ipv6Addr::LOCALHOST.into(),
        });
    }
    addr
}

fn registry_file(dir: &Path, identity: &Identity, addr: SocketAddr) -> Result<PathBuf> {
    let mut dirs = fs::DirBuilder::new();
    dirs.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        dirs.mode(0o700);
    }
    dirs.create(dir)?;
    for entry in fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json")
            && path.file_stem().and_then(|name| name.to_str()).and_then(|name| name.parse::<i32>().ok())
                .is_some_and(pid_gone)
        {
            let _ = fs::remove_file(path);
        }
    }
    #[derive(Serialize)]
    struct Entry<'a> {
        #[serde(flatten)]
        identity: &'a Identity,
        pid: u32,
        url: String,
    }
    let pid = std::process::id();
    let path = dir.join(format!("{pid}.json"));
    let tmp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let entry = Entry {
        identity,
        pid,
        url: format!("http://{}", usable_local_addr(addr)),
    };
    let result = (|| -> Result<()> {
        serde_json::to_writer(options.open(&tmp)?, &entry)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map(|()| path)
}

fn pid_gone(pid: i32) -> bool {
    #[cfg(unix)]
    if pid > 0 && unsafe { libc::kill(pid, 0) } == -1 {
        return io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    }
    false
}

fn txt_value(text: &str, key: &str) -> String {
    // RFC 6763: a length-prefixed TXT string has at most 255 bytes, including key and '='.
    text[..text.floor_char_boundary(text.len().min(254 - key.len()))].to_owned()
}

struct Announcement {
    daemon: ServiceDaemon,
    name: String,
}

impl Announcement {
    fn start(identity: &Identity, addr: SocketAddr) -> Result<Self> {
        let ip = addr.ip();
        let tailnet = crate::auth::tailnet_ip(ip);
        let mut txt = vec![
            ("id", identity.library_id.clone()),
            ("name", txt_value(&identity.library_name, "name")),
            ("host", txt_value(&identity.host, "host")),
            ("auth", identity.auth.to_owned()),
        ];
        if tailnet {
            // The SRV target is on this link; connect to the listener on Tailscale instead.
            txt.push(("addr", ip.to_string()));
        }
        let daemon = ServiceDaemon::new()?;
        let result = (|| -> Result<String> {
            let label = format!("vamp-{}", uuid::Uuid::new_v4().simple());
            let addresses: &[IpAddr] = if tailnet || ip.is_unspecified() {
                &[]
            } else {
                std::slice::from_ref(&ip)
            };
            let selected = IfKind::Predicate(IfPredicate::new(move |interface| {
                let link_ip = interface.ip();
                if link_ip.is_loopback() || crate::auth::tailnet_ip(link_ip) {
                    return false;
                }
                if tailnet || ip.is_unspecified() {
                    return tailnet || link_ip.is_ipv4() == ip.is_ipv4();
                }
                link_ip == ip
            }));
            let mut service = ServiceInfo::new(
                SERVICE_TYPE,
                &label,
                &format!("{label}.local."),
                addresses,
                addr.port(),
                txt.as_slice(),
            )?;
            service.set_interfaces(vec![selected]);
            if addresses.is_empty() {
                service = service.enable_addr_auto();
            }
            let name = service.get_fullname().to_owned();
            daemon.register(service)?;
            Ok(name)
        })();
        match result {
            Ok(name) => Ok(Self { daemon, name }),
            Err(error) => {
                let _ = daemon.shutdown();
                Err(error)
            }
        }
    }
}

impl Drop for Announcement {
    fn drop(&mut self) {
        if let Ok(status) = self.daemon.unregister(&self.name) {
            let _ = status.recv_timeout(Duration::from_millis(250));
        }
        let _ = self.daemon.shutdown();
    }
}

pub(crate) struct Registration {
    file: Option<PathBuf>,
    _announcement: Option<Announcement>,
}

impl Registration {
    pub(crate) fn start(identity: &Identity, addr: SocketAddr, advertise: bool) -> Self {
        let file = registry_path(
            env::var_os("XDG_STATE_HOME").map(PathBuf::from),
            env::var_os("HOME").map(PathBuf::from),
        ).context("no absolute XDG_STATE_HOME or HOME")
            .and_then(|dir| registry_file(&dir, identity, addr))
            .map_err(|error| eprintln!("vamp-server: local discovery unavailable: {error:#}"))
            .ok();
        let announcement = if advertise && !addr.ip().is_loopback() {
            Announcement::start(identity, addr)
                .map_err(|error| eprintln!("vamp-server: network discovery unavailable: {error:#}"))
                .ok()
        } else {
            None
        };
        Self { file, _announcement: announcement }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(file) = &self.file {
            let _ = fs::remove_file(file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_only_connection_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let identity = Identity {
            version: 1,
            library_id: "library-id".into(),
            library_name: "My photos".into(),
            host: "computer.local".into(),
            auth: "token",
        };
        let addr = "0.0.0.0:7794".parse().unwrap();
        let path = registry_file(dir.path(), &identity, addr).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(value, serde_json::json!({
            "version": 1, "pid": std::process::id(), "url": "http://127.0.0.1:7794",
            "library_id": "library-id", "library_name": "My photos", "host": "computer.local", "auth": "token",
        }));
        assert_eq!(usable_local_addr("[::]:7794".parse().unwrap()).to_string(), "[::1]:7794");
        assert!(!pid_gone(std::process::id() as i32));
    }

    #[test]
    fn registry_root_honors_absolute_state_home_only() {
        assert_eq!(registry_path(Some("/state".into()), None), Some("/state/vamp/servers".into()));
        assert_eq!(registry_path(Some("relative".into()), None), None);
        assert_eq!(txt_value(&"🌄".repeat(80), "name").len(), 248);
    }
}
