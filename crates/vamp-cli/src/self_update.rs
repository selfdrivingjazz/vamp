//! `vamp self-update`: replace this release install (`vamp` and the `vamp-media` beside it)
//! with the latest GitHub release of `selfdrivingjazz/vamp`, verified against that release's
//! SHA256SUMS. Only release builds (built with `VAMP_RELEASE_TARGET` set to their target
//! triple) know their asset; source builds refuse.

use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

/// Release download base; `VAMP_INSTALL_BASE` overrides it like it does for the installer.
/// Assets are fetched from `<base>/latest/download/<name>`.
const DEFAULT_BASE: &str = "https://github.com/selfdrivingjazz/vamp/releases";
const BINARIES: [&str; 2] = ["vamp-media", "vamp"];
const MAX_ARCHIVE: u64 = 256 << 20;

/// The release asset's target triple, compiled in by release builds.
const RELEASE_TARGET: Option<&str> = option_env!("VAMP_RELEASE_TARGET");

pub fn run(check: bool) -> Result<Value> {
    let Some(target) = RELEASE_TARGET.filter(|target| !target.is_empty()) else {
        bail!(
            "this vamp was built from source, not installed from a release; update it from its checkout, or install a release with: curl -fsSL https://vamp.sh/install | sh"
        );
    };
    let base = std::env::var("VAMP_INSTALL_BASE").ok().filter(|b| !b.is_empty());
    let base = base.as_deref().unwrap_or(DEFAULT_BASE).trim_end_matches('/');
    let current = env!("CARGO_PKG_VERSION");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(600)))
        .user_agent(concat!("vamp/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let sums = String::from_utf8(get(&agent, &format!("{base}/latest/download/SHA256SUMS"), 1 << 20)?)
        .context("SHA256SUMS is not text")?;
    let (asset, sha256, latest) = select_asset(&sums, target)?;
    let available = newer(&latest, current)?;
    let mut out = json!({"current": current, "latest": latest, "target": target, "update_available": available, "updated": false});
    if check || !available {
        return Ok(out);
    }

    let archive = get(&agent, &format!("{base}/latest/download/{asset}"), MAX_ARCHIVE)?;
    ensure!(hex(&Sha256::digest(&archive)) == sha256, "checksum mismatch for {asset}; nothing was replaced");
    let mut files = untar(flate2::read::GzDecoder::new(archive.as_slice()))
        .with_context(|| format!("reading {asset}"))?;
    let exe = std::env::current_exe()
        .and_then(fs::canonicalize)
        .context("locating the running vamp")?;
    let dir = exe.parent().context("the running vamp has no directory")?;
    let mut installed = Vec::new();
    for name in BINARIES {
        let bytes = files.remove(name).with_context(|| format!("{asset} has no {name}"))?;
        let path = if name == "vamp" { exe.clone() } else { dir.join(name) };
        replace(&path, &bytes).with_context(|| {
            format!("replacing {} (no write access? re-run the installer with --dir pointing at a directory you own)", path.display())
        })?;
        installed.push(path);
    }
    out["updated"] = json!(true);
    out["installed"] = json!(installed);
    Ok(out)
}

fn get(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>> {
    agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .with_context(|| format!("reading {url}"))
}

/// The release asset for `target` in a SHA256SUMS file (`<sha256>  vamp-<version>-<target>.tar.gz`
/// lines): its file name, checksum and version.
fn select_asset(sums: &str, target: &str) -> Result<(String, String, String)> {
    let suffix = format!("-{target}.tar.gz");
    let mut found = sums.lines().filter_map(|line| {
        let (sha, name) = line.split_once(char::is_whitespace)?;
        let name = name.trim().trim_start_matches('*');
        let version = name.strip_prefix("vamp-")?.strip_suffix(&suffix)?;
        Some((name.to_owned(), sha.to_ascii_lowercase(), version.to_owned()))
    });
    let asset = found.next().with_context(|| format!("the latest release has no build for {target}"))?;
    ensure!(found.next().is_none(), "SHA256SUMS lists several builds for {target}");
    ensure!(
        asset.1.len() == 64 && asset.1.bytes().all(|b| b.is_ascii_hexdigit()),
        "malformed checksum for {}",
        asset.0
    );
    Ok(asset)
}

/// `latest` is a later release than `current`: MAJOR.MINOR.PATCH, a pre-release (`-rc.1`)
/// sorting before its release.
fn newer(latest: &str, current: &str) -> Result<bool> {
    fn parse(version: &str) -> Result<([u64; 3], Option<&str>)> {
        let (core, pre) = match version.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (version, None),
        };
        let mut parts = core.split('.').map(str::parse::<u64>);
        let mut numbers = [0; 3];
        for number in &mut numbers {
            *number = parts.next().with_context(|| format!("bad version {version}"))?
                .with_context(|| format!("bad version {version}"))?;
        }
        ensure!(parts.next().is_none(), "bad version {version}");
        Ok((numbers, pre))
    }
    let (latest, current) = (parse(latest)?, parse(current)?);
    Ok(match latest.0.cmp(&current.0) {
        std::cmp::Ordering::Equal => match (latest.1, current.1) {
            (None, Some(_)) => true,
            (Some(a), Some(b)) => a > b,
            _ => false,
        },
        order => order.is_gt(),
    })
}

/// Regular files of a ustar archive named like one of BINARIES (any directory).
fn untar(mut reader: impl std::io::Read) -> Result<std::collections::HashMap<&'static str, Vec<u8>>> {
    let mut out = std::collections::HashMap::new();
    let mut header = [0u8; 512];
    loop {
        reader.read_exact(&mut header).context("truncated archive")?;
        if header.iter().all(|&b| b == 0) {
            return Ok(out);
        }
        let field = |range: std::ops::Range<usize>| {
            let bytes = &header[range];
            std::str::from_utf8(&bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())])
                .unwrap_or("")
                .trim()
                .to_owned()
        };
        let size = u64::from_str_radix(&field(124..136), 8).context("bad tar entry size")?;
        ensure!(size <= MAX_ARCHIVE, "tar entry too large");
        let mut data = Vec::new();
        (&mut reader).take(size).read_to_end(&mut data)?;
        ensure!(data.len() as u64 == size, "truncated archive");
        let padding = (512 - size % 512) % 512;
        std::io::copy(&mut (&mut reader).take(padding), &mut std::io::sink())?;
        let name = field(0..100);
        let base = name.rsplit('/').next().unwrap_or("");
        if matches!(header[156], b'0' | 0) {
            if let Some(&known) = BINARIES.iter().find(|&&n| n == base) {
                out.insert(known, data);
            }
        }
    }
}

/// Write `bytes` beside `path` and rename it over `path`, so a running copy keeps its inode
/// and a failure leaves the old file in place.
fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("no parent directory")?;
    let name = path.file_name().context("no file name")?.to_string_lossy();
    let temp: PathBuf = dir.join(format!(".{name}.update-{}", std::process::id()));
    let result = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))?;
        }
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    Ok(result?)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(newer("0.2.0", "0.1.9").unwrap());
        assert!(newer("0.10.0", "0.9.0").unwrap());
        assert!(newer("1.0.0", "1.0.0-rc.2").unwrap());
        assert!(newer("1.0.0-rc.2", "1.0.0-rc.1").unwrap());
        assert!(!newer("0.1.0", "0.1.0").unwrap());
        assert!(!newer("0.1.0", "0.2.0").unwrap());
        assert!(!newer("1.0.0-rc.1", "1.0.0").unwrap());
        assert!(newer("0.1", "0.1.0").is_err());
    }

    #[test]
    fn asset_for_target() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!(
            "{a}  vamp-0.2.0-aarch64-unknown-linux-musl.tar.gz\n{b} *vamp-0.2.0-x86_64-unknown-linux-musl.tar.gz\n{a}  SHA256SUMS.sig\n"
        );
        assert_eq!(
            select_asset(&sums, "x86_64-unknown-linux-musl").unwrap(),
            ("vamp-0.2.0-x86_64-unknown-linux-musl.tar.gz".into(), "b".repeat(64), "0.2.0".into())
        );
        assert!(select_asset(&sums, "x86_64-apple-darwin").is_err());
    }
}
