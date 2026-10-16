//! Tailscale identity auth: a connection from a tailnet address is admitted when the
//! peer's Tailscale user login or one of its node tags is in the allowlist stored in
//! `<state_dir>/tailnet-allow.json`. Peers are resolved with `tailscale whois --json`
//! (the CLI talks to the local tailscaled / Tailscale.app LocalAPI) and cached per IP.
//! Bearer tokens remain valid in every mode.
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ALLOW_FILE: &str = "tailnet-allow.json";
/// Whois answers (including failures) are reused this long per peer IP.
const WHOIS_TTL: Duration = Duration::from_secs(60);
/// macOS GUI install, which does not put `tailscale` on PATH.
const MAC_APP_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AuthMode {
    /// Bearer token only.
    Token,
    /// Tailscale identity (allowlisted user or tag) or bearer token.
    Tailscale,
}

/// Users (Tailscale login names, e.g. `alice@example.com`) and node tags (`tag:server`)
/// admitted without a token.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allowlist {
    #[serde(default)]
    pub users: BTreeSet<String>,
    #[serde(default)]
    pub tags: BTreeSet<String>,
}

/// Identity of a tailnet peer. Tag-owned nodes report the pseudo-user `tagged-devices`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Peer {
    pub login: String,
    pub tags: Vec<String>,
    pub node: String,
}

impl Allowlist {
    /// Tag-owned nodes never match by user: `tagged-devices` is shared by every tagged node.
    pub fn permits(&self, peer: &Peer) -> bool {
        (peer.login != "tagged-devices" && self.users.contains(&peer.login))
            || peer.tags.iter().any(|t| self.tags.contains(t))
    }
    /// The `vamp-server allow` invocation that would admit `peer`.
    pub fn hint(peer: &Peer) -> String {
        match peer.tags.first() {
            Some(tag) => format!("vamp-server allow --tag {tag}"),
            None => format!("vamp-server allow --user {}", peer.login),
        }
    }
    /// Stored allowlist, or (none stored yet) the server machine's own Tailscale user.
    pub fn load(state_dir: &Path, tailscale: &Tailscale) -> Result<Self> {
        match std::fs::read(state_dir.join(ALLOW_FILE)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid {}", state_dir.join(ALLOW_FILE).display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                users: tailscale.self_login()?.into_iter().collect(),
                tags: BTreeSet::new(),
            }),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        let path = state_dir.join(ALLOW_FILE);
        let temp = state_dir.join(format!(".{ALLOW_FILE}.tmp"));
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    }
}

/// Tailscale's CGNAT IPv4 range (100.64.0.0/10) or ULA IPv6 range (fd7a:115c:a1e0::/48).
pub fn tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// Per-IP whois answers (failures included) with the time they were fetched.
type WhoisCache = HashMap<IpAddr, (Instant, Result<Peer, String>)>;

pub struct Tailscale {
    cli: PathBuf,
    cache: Mutex<WhoisCache>,
}

impl Tailscale {
    /// `$VAMP_TAILSCALE`, else `tailscale` on PATH, else the macOS app's bundled CLI.
    pub fn discover() -> Self {
        let cli = std::env::var_os("VAMP_TAILSCALE")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("PATH").and_then(|paths| {
                    std::env::split_paths(&paths)
                        .map(|p| p.join("tailscale"))
                        .find(|p| p.is_file())
                })
            })
            .unwrap_or_else(|| PathBuf::from(MAC_APP_CLI));
        Self {
            cli,
            cache: Mutex::new(HashMap::new()),
        }
    }
    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut cmd = Command::new(&self.cli);
        cmd.args(args);
        // The macOS app binary runs as a CLI only when it looks like a terminal launch
        // (TERM or SHLVL set); under launchd it otherwise tries to start the GUI and fails.
        if std::env::var_os("TERM").is_none() {
            cmd.env("TERM", "dumb");
        }
        let output = cmd
            .output()
            .with_context(|| format!("run {} (set VAMP_TAILSCALE)", self.cli.display()))?;
        ensure!(
            output.status.success(),
            "tailscale {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(output.stdout)
    }
    /// This machine's tailnet IPv4.
    pub fn ipv4(&self) -> Result<Ipv4Addr> {
        let out = self.run(&["ip", "-4"])?;
        let text = String::from_utf8_lossy(&out);
        text.lines()
            .find_map(|l| l.trim().parse().ok())
            .context("tailscale ip -4 returned no address")
    }
    /// Login of the user owning this machine; `None` for tag-owned machines.
    pub fn self_login(&self) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Status {
            #[serde(rename = "Self")]
            me: SelfNode,
            #[serde(rename = "User", default)]
            users: HashMap<String, Profile>,
        }
        #[derive(Deserialize)]
        struct SelfNode {
            #[serde(rename = "UserID")]
            user: u64,
            #[serde(rename = "Tags", default)]
            tags: Vec<String>,
        }
        let status: Status = serde_json::from_slice(&self.run(&["status", "--json"])?)
            .context("invalid tailscale status --json")?;
        if !status.me.tags.is_empty() {
            return Ok(None);
        }
        Ok(status
            .users
            .get(&status.me.user.to_string())
            .map(|p| p.login.clone()))
    }
    /// Resolve the peer at `ip` (cached for [`WHOIS_TTL`]).
    pub fn whois(&self, ip: IpAddr) -> Result<Peer> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, answer)) = cache.get(&ip)
            && at.elapsed() < WHOIS_TTL
        {
            return answer.clone().map_err(anyhow::Error::msg);
        }
        drop(cache);
        let answer = self
            .run(&["whois", "--json", &ip.to_string()])
            .and_then(|bytes| parse_whois(&bytes))
            .map_err(|e| format!("{e:#}"));
        cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(ip, (Instant::now(), answer.clone()));
        answer.map_err(anyhow::Error::msg)
    }
}

#[derive(Deserialize)]
struct Profile {
    #[serde(rename = "LoginName")]
    login: String,
}

/// `tailscale whois --json` / LocalAPI `whois` body.
pub fn parse_whois(bytes: &[u8]) -> Result<Peer> {
    #[derive(Deserialize)]
    struct Whois {
        #[serde(rename = "Node")]
        node: Node,
        #[serde(rename = "UserProfile")]
        user: Profile,
    }
    #[derive(Deserialize)]
    struct Node {
        #[serde(rename = "Name", default)]
        name: String,
        #[serde(rename = "Tags", default)]
        tags: Option<Vec<String>>,
    }
    let whois: Whois = serde_json::from_slice(bytes).context("invalid whois answer")?;
    Ok(Peer {
        login: whois.user.login,
        tags: whois.node.tags.unwrap_or_default(),
        node: whois.node.name.trim_end_matches('.').to_owned(),
    })
}

/// Tailscale identity check for one connection; `Err` carries the client-facing reason.
pub struct TailnetAuth {
    pub tailscale: Tailscale,
    pub state_dir: PathBuf,
}

impl TailnetAuth {
    pub fn check(&self, ip: Option<IpAddr>) -> Result<Peer> {
        let Some(ip) = ip.filter(|ip| tailnet_ip(*ip)) else {
            bail!("not a tailnet peer; use a bearer token");
        };
        let peer = self
            .tailscale
            .whois(ip)
            .with_context(|| format!("cannot identify tailnet peer {ip}"))?;
        // Re-read per request so `vamp-server allow` applies without a restart.
        let allow = Allowlist::load(&self.state_dir, &self.tailscale)?;
        ensure!(
            allow.permits(&peer),
            "tailnet peer {} (user {}, tags [{}]) is not allowed; on the server run: {}",
            peer.node,
            peer.login,
            peer.tags.join(", "),
            Allowlist::hint(&peer)
        );
        Ok(peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(login: &str, tags: &[&str]) -> Peer {
        Peer {
            login: login.into(),
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            node: "n".into(),
        }
    }

    #[test]
    fn users_and_tags_admit_independently() {
        let allow = Allowlist {
            users: ["alice@example.com".to_owned()].into(),
            tags: ["tag:server".to_owned()].into(),
        };
        assert!(allow.permits(&peer("alice@example.com", &[])));
        assert!(allow.permits(&peer("tagged-devices", &["tag:server"])));
        assert!(!allow.permits(&peer("eve@example.com", &[])));
        assert!(!allow.permits(&peer("tagged-devices", &["tag:other"])));
        assert!(!Allowlist::default().permits(&peer("alice@example.com", &[])));
    }

    #[test]
    fn tagged_devices_pseudo_user_never_admits_every_tagged_node() {
        let allow = Allowlist {
            users: ["tagged-devices".to_owned()].into(),
            tags: BTreeSet::new(),
        };
        assert!(!allow.permits(&peer("tagged-devices", &["tag:anything"])));
    }

    #[test]
    fn denial_hint_names_the_tag_for_tag_owned_nodes() {
        assert_eq!(
            Allowlist::hint(&peer("tagged-devices", &["tag:server"])),
            "vamp-server allow --tag tag:server"
        );
        assert_eq!(
            Allowlist::hint(&peer("alice@example.com", &[])),
            "vamp-server allow --user alice@example.com"
        );
    }

    #[test]
    fn whois_parses_tagged_and_user_nodes() {
        let tagged = br#"{"Node":{"Name":"server.example-tailnet.ts.net.","Tags":["tag:server"]},"UserProfile":{"LoginName":"tagged-devices"}}"#;
        assert_eq!(
            parse_whois(tagged).unwrap(),
            Peer {
                login: "tagged-devices".into(),
                tags: vec!["tag:server".into()],
                node: "server.example-tailnet.ts.net".into(),
            }
        );
        let user = br#"{"Node":{"Name":"laptop.ts.net.","Tags":null},"UserProfile":{"LoginName":"alice@example.com"}}"#;
        assert_eq!(parse_whois(user).unwrap().tags, Vec::<String>::new());
    }

    #[test]
    fn tailnet_ranges() {
        assert!(tailnet_ip("100.64.0.7".parse().unwrap()));
        assert!(tailnet_ip("fd7a:115c:a1e0::7".parse().unwrap()));
        assert!(!tailnet_ip("100.128.0.1".parse().unwrap()));
        assert!(!tailnet_ip("127.0.0.1".parse().unwrap()));
    }
}
