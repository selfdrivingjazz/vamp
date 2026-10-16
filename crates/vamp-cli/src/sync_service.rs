//! `vamp sync install-service`: keep `vamp sync --watch` running for one replica context
//! (systemd --user on Linux, a launchd agent on macOS).
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use vamp_core::{Library, load_config};

pub fn install(home: &Path, context: Option<&str>) -> Result<Value> {
    let name = match context {
        Some(name) => name.to_owned(),
        None => load_config(home)?
            .current
            .context("no active context; pass --context")?,
    };
    ensure!(
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "context name {name:?} cannot be used in a service name"
    );
    let home = if home.is_absolute() {
        home.to_path_buf()
    } else {
        std::env::current_dir()?.join(home)
    };
    let library = Library::open(&home, Some(&name))?;
    ensure!(
        library.context.remote.is_some(),
        "context {name} is not a replica; sync --watch only runs on replicas (vamp join)"
    );
    let program = std::env::current_exe()?.canonicalize()?;
    let user_home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let args = [
        "--home".to_owned(),
        path_text(&home)?.to_owned(),
        "--context".to_owned(),
        name.clone(),
        "sync".to_owned(),
        "--watch".to_owned(),
    ];
    if cfg!(target_os = "macos") {
        let label = format!("com.selfdrivingjazz.vamp.sync.{name}");
        let path = user_home.join(format!("Library/LaunchAgents/{label}.plist"));
        let log = user_home.join(format!("Library/Logs/vamp-sync-{name}.log"));
        let env = [("PATH", std::env::var("PATH").unwrap_or_default())];
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            vamp_core::launch_agent_plist(&label, &program, &args, &env, &log),
        )?;
        let load = format!(
            "launchctl bootout gui/$(id -u)/{label} 2>/dev/null; launchctl bootstrap gui/$(id -u) {}",
            path.display()
        );
        Ok(json!({"agent":path,"log":log,"load":load}))
    } else {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| user_home.join(".config"));
        let unit_name = format!("vamp-sync-{name}.service");
        let path = config.join("systemd/user").join(&unit_name);
        let mut exec = format!("\"{}\"", quoted(&program)?);
        for arg in &args {
            exec.push_str(&format!(" \"{}\"", quoted(Path::new(arg))?));
        }
        // Restarts cover authority restarts and network outages.
        let unit = format!(
            "[Unit]\nDescription=VAMP replica sync ({name})\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={exec}\nRestart=always\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n"
        );
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, unit)?;
        Ok(json!({"unit":path,
            "enable":format!("systemctl --user daemon-reload && systemctl --user enable --now {unit_name}"),
            "notes":["Run `loginctl enable-linger $USER` once so sync runs without a login session.",
                format!("Logs: journalctl --user -u {unit_name} -f")]}))
    }
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("service paths must be UTF-8")
}

fn quoted(path: &Path) -> Result<&str> {
    let text = path_text(path)?;
    ensure!(
        !text.contains(['"', '\\', '%']),
        "service arguments cannot contain quotes, backslashes or %"
    );
    Ok(text)
}
