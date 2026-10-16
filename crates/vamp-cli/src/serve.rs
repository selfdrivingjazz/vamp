//! `vamp serve`: run the `vamp-server` installed beside this `vamp` (release archives ship both)
//! for the current or `--context` library. The server stays its own program so the CLI carries
//! no HTTP server and supervisors (launchd, systemd) run and restart the same binary.

use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Options {
    pub bind: Option<String>,
    pub auth: Option<String>,
    pub no_jobs: bool,
    pub dist: Option<PathBuf>,
}

/// Replace this process with `vamp-server --home HOME [--context NAME] serve …`; returns only
/// on failure (on Windows it waits and exits with the server's status).
pub fn run(home: &Path, context: Option<&str>, options: Options) -> Result<()> {
    let server = locate()?;
    let mut command = Command::new(&server);
    command.arg("--home").arg(home);
    if let Some(context) = context {
        command.args(["--context", context]);
    }
    command.arg("serve");
    if let Some(bind) = options.bind {
        command.args(["--bind", &bind]);
    }
    if let Some(auth) = options.auth {
        command.args(["--auth", &auth]);
    }
    if options.no_jobs {
        command.arg("--no-jobs");
    }
    if let Some(dist) = options.dist {
        command.arg("--dist").arg(dist);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let error = command.exec();
        Err(error).with_context(|| format!("starting {}", server.display()))
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .with_context(|| format!("starting {}", server.display()))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// `vamp-server` beside the running `vamp` (also beside the symlink's target), else on PATH.
fn locate() -> Result<PathBuf> {
    let binary = format!("vamp-server{}", std::env::consts::EXE_SUFFIX);
    let mut candidates = Vec::new();
    if let Ok(current) = std::env::current_exe() {
        candidates.extend(current.parent().map(|dir| dir.join(&binary)));
        if let Ok(real) = current.canonicalize() {
            candidates.extend(real.parent().map(|dir| dir.join(&binary)));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join(&binary)));
    }
    candidates.into_iter().find(|path| crate::executable(path)).context(
        "vamp-server is not installed beside vamp or on PATH; release archives include it (curl -fsSL https://vamp.sh/install | sh), source builds: cargo build --release -p vamp-server",
    )
}
