use anyhow::{Context as _, Result, anyhow, ensure};
use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::sync::Arc;
use vamp_core::{Library, default_home, load_config};
use vamp_diagnostics::{Event, Feature, Product, Reporter};
use vamp_server::auth::{Allowlist, AuthMode, Tailscale};
use vamp_server::telemetry::Telemetry;

/// Serve one local VAMP library over HTTP for replicas, clippers and browser extensions.
#[derive(Parser)]
#[command(name = "vamp-server", version)]
struct Cli {
    #[arg(long, global = true, env = "VAMP_HOME")]
    home: Option<PathBuf>,
    /// Context to serve (default: current context).
    #[arg(long, global = true, env = "VAMP_CONTEXT")]
    context: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the bearer token, creating it (0600, in the library state dir) if absent.
    Token,
    /// Serve the /v1 API and (unless --no-jobs) drain processor jobs in the background.
    Serve {
        /// `host:port`; `tailnet` = this machine's Tailscale IPv4 on :7777 (`tailnet:PORT`
        /// for another port). Use `tailnet` to accept other machines on your Tailscale network.
        #[arg(long, default_value = "127.0.0.1:7777")]
        bind: String,
        /// Default: `tailscale` when bound to a tailnet address, else `token`.
        #[arg(long, value_enum)]
        auth: Option<AuthMode>,
        /// Announce this library on the local network (name and ID, never credentials).
        #[arg(long)]
        advertise: bool,
        /// HTTP workers. Requests never touch cloud folders (originals go through a separate
        /// bounded I/O pool), so workers are cheap.
        #[arg(long, default_value_t = 16)]
        threads: usize,
        /// Do not run processor jobs (captions, tags, embeddings) in this process.
        #[arg(long)]
        no_jobs: bool,
        /// Seconds between job queue polls when idle.
        #[arg(long, default_value_t = 5)]
        jobs_interval: u64,
        /// Serve CLI installer + release files without a token: GET /install.sh, /dist/<file>.
        /// Default: <VAMP_HOME>/dist when that directory exists.
        #[arg(long, env = "VAMP_DIST")]
        dist: Option<PathBuf>,
    },
    /// Write ~/Library/LaunchAgents/com.selfdrivingjazz.vamp.server.<context>.plist (does not load it).
    InstallAgent(ServiceArgs),
    /// Write a user service (launchd on macOS, systemd on Linux) without loading it.
    InstallService(ServiceArgs),
    /// Admit Tailscale identities without a token (`serve --auth tailscale`). With no
    /// flags, prints the allowlist. The default allowlist is this machine's Tailscale user.
    Allow {
        /// Tailscale login name, e.g. alice@example.com. Repeatable.
        #[arg(long = "user")]
        users: Vec<String>,
        /// Node tag, e.g. tag:server (needed for tag-owned machines). Repeatable.
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Remove the given users/tags instead of adding them.
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Args)]
struct ServiceArgs {
    #[arg(long, default_value = "127.0.0.1:7777")]
    bind: String,
    /// Announce this library on the local network.
    #[arg(long)]
    advertise: bool,
    #[arg(long)]
    no_jobs: bool,
    /// Pass `--dist DIR` to serve (installer + release files).
    #[arg(long)]
    dist: Option<PathBuf>,
    /// Replace an existing service definition.
    #[arg(long)]
    force: bool,
}

fn context_name(cli: &Cli, home: &Path) -> Result<String> {
    match &cli.context {
        Some(name) => Ok(name.clone()),
        None => load_config(home)?
            .current
            .ok_or_else(|| anyhow!("no active context; pass --context")),
    }
}

fn service_args(name: &str, options: &ServiceArgs) -> Result<Vec<String>> {
    let mut args = vec![
        "serve".to_owned(),
        "--context".to_owned(),
        name.to_owned(),
        "--bind".to_owned(),
        options.bind.clone(),
    ];
    if options.advertise {
        args.push("--advertise".into());
    }
    if options.no_jobs {
        args.push("--no-jobs".into());
    }
    if let Some(dist) = &options.dist {
        args.push("--dist".into());
        args.push(dist.canonicalize()?.to_string_lossy().into_owned());
    }
    Ok(args)
}

fn install_agent(home: &Path, name: &str, options: &ServiceArgs) -> Result<()> {
    let user_home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let label = format!("com.selfdrivingjazz.vamp.server.{name}");
    let agents = user_home.join("Library/LaunchAgents");
    let path = agents.join(format!("{label}.plist"));
    ensure!(
        options.force || !path.exists(),
        "{} exists; pass --force to replace it",
        path.display()
    );
    let program = std::env::current_exe()?.canonicalize()?;
    let args = service_args(name, options)?;
    let mut env = vec![(
        "VAMP_HOME",
        home.canonicalize()?.to_string_lossy().into_owned(),
    )];
    for key in ["VAMP_MEDIA_HELPER", "VAMP_STORE_HELPER", "VAMP_FFMPEG", "XDG_STATE_HOME", "DO_NOT_TRACK"] {
        // A launched service does not inherit these settings from the installer.
        if let Some(value) = std::env::var_os(key) {
            env.push((key, value.to_string_lossy().into_owned()));
        }
    }
    // launchd starts with a minimal PATH; keep the caller's so processors resolve.
    if let Some(search) = std::env::var_os("PATH") {
        env.push(("PATH", search.to_string_lossy().into_owned()));
    }
    let log = user_home.join(format!("Library/Logs/vamp-server-{name}.log"));
    std::fs::create_dir_all(&agents)?;
    std::fs::write(
        &path,
        vamp_core::launch_agent_plist(&label, &program, &args, &env, &log),
    )?;
    println!("wrote {}", path.display());
    println!(
        "load:   launchctl bootstrap gui/$(id -u) {}",
        path.display()
    );
    println!("unload: launchctl bootout gui/$(id -u)/{label}");
    println!("log:    {}", log.display());
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_systemd(home: &Path, name: &str, options: &ServiceArgs) -> Result<()> {
    // The library context is already validated, but an attached legacy context may contain dots.
    ensure!(
        !name.is_empty()
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.')),
        "context name {name:?} cannot be used in a service name"
    );
    let user_home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| user_home.join(".config"));
    let unit_name = format!("vamp-server-{name}.service");
    let path = config.join("systemd/user").join(&unit_name);
    ensure!(
        options.force || !path.exists(),
        "{} exists; pass --force to replace it",
        path.display()
    );
    let program = std::env::current_exe()?.canonicalize()?;
    let home = home.canonicalize()?;
    let args = service_args(name, options)?;
    fn quoted(text: &str) -> String {
        // systemd.syntax quoting plus protection from specifier and ExecStart expansions.
        format!(
            "\"{}\"",
            text.replace('\\', "\\\\").replace('"', "\\\"")
                .replace('\n', "\\n").replace('\r', "\\r").replace('%', "%%").replace('$', "$$")
        )
    }
    let paths = [program.as_path(), Path::new("--home"), home.as_path()];
    let exec = paths.iter().copied().chain(args.iter().map(Path::new))
        .map(|value| value.to_str().map(quoted).context("service arguments must be UTF-8"))
        .collect::<Result<Vec<_>>>()?.join(" ");
    let mut env = String::new();
    for key in ["PATH", "VAMP_MEDIA_HELPER", "VAMP_STORE_HELPER", "VAMP_FFMPEG", "XDG_STATE_HOME", "DO_NOT_TRACK"] {
        if let Ok(value) = std::env::var(key) {
            // Environment= does not expand variables, unlike ExecStart.
            let assignment = format!("{key}={value}");
            env.push_str(&format!("Environment={}\n", quoted(&assignment).replace("$$", "$")));
        }
    }
    let unit = format!(
        "[Unit]\nDescription=VAMP library server ({name})\n\
        \n[Service]\nExecStart={exec}\n{env}Restart=on-failure\nRestartSec=5\n\
        \n[Install]\nWantedBy=default.target\n"
    );
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, unit)?;
    println!("wrote {}", path.display());
    println!("start:  systemctl --user daemon-reload && systemctl --user enable --now {unit_name}");
    println!("stop:   systemctl --user disable --now {unit_name}");
    println!("log:    journalctl --user -u {unit_name} -f");
    Ok(())
}

fn install_service(home: &Path, name: &str, options: &ServiceArgs) -> Result<()> {
    #[cfg(target_os = "linux")]
    return install_systemd(home, name, options);
    #[cfg(target_os = "macos")]
    return install_agent(home, name, options);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    anyhow::bail!("install-service is supported on Linux and macOS; run serve under a supervisor")
}

/// `tailnet[:port]` -> this machine's Tailscale IPv4 (default port 7777).
fn resolve_bind(bind: &str, tailscale: &Tailscale) -> Result<String> {
    let Some(rest) = bind.strip_prefix("tailnet") else {
        return Ok(bind.to_owned());
    };
    let port = match rest.strip_prefix(':') {
        Some(port) => port.parse::<u16>().context("invalid tailnet port")?,
        None if rest.is_empty() => 7777,
        None => anyhow::bail!("--bind tailnet[:PORT]"),
    };
    Ok(format!("{}:{port}", tailscale.ipv4()?))
}

fn main() -> Result<()> {
    vamp_core::allow_cloud_placeholder_reads();
    let cli = Cli::parse();
    let home = cli.home.clone().unwrap_or_else(default_home);
    let reporter = matches!(cli.command, Command::Serve { .. })
        .then(|| vamp_diagnostics::reporter(&home, Product::server()))
        .flatten();
    if let Some(reporter) = &reporter {
        reporter.clone().install_panic_hook(Feature::Launch);
    }
    let result = run(cli, &home, reporter.as_ref());
    if result.is_err()
        && let Some(reporter) = reporter
    {
        reporter.record(Event::new(Feature::Launch, false));
    }
    result
}

fn run(cli: Cli, home: &Path, reporter: Option<&Arc<Reporter>>) -> Result<()> {
    let name = context_name(&cli, home)?;
    let library = Library::open(home, Some(&name))?;
    ensure!(
        library.context.remote.is_none(),
        "context {name} is a replica; serve its authority library instead"
    );
    match cli.command {
        Command::Token => println!("{}", vamp_server::token(&library.state_dir)?),
        Command::Allow {
            users,
            tags,
            remove,
        } => {
            let tailscale = Tailscale::discover();
            let mut allow = Allowlist::load(&library.state_dir, &tailscale)?;
            for tag in &tags {
                ensure!(tag.starts_with("tag:"), "tags look like tag:name: {tag}");
            }
            if remove {
                allow.users.retain(|u| !users.contains(u));
                allow.tags.retain(|t| !tags.contains(t));
            } else {
                allow.users.extend(users.iter().cloned());
                allow.tags.extend(tags.iter().cloned());
            }
            if !users.is_empty() || !tags.is_empty() {
                allow.save(&library.state_dir)?;
            }
            println!("{}", serde_json::to_string(&allow)?);
        }
        Command::InstallAgent(options) => install_agent(home, &name, &options)?,
        Command::InstallService(options) => install_service(home, &name, &options)?,
        Command::Serve {
            bind,
            auth,
            advertise,
            threads,
            no_jobs,
            jobs_interval,
            dist,
        } => {
            let tailscale = Tailscale::discover();
            let bind = resolve_bind(&bind, &tailscale)?;
            let server = tiny_http::Server::http(&bind).map_err(|e| anyhow!("bind {bind}: {e}"))?;
            let addr = server.server_addr().to_ip().context("server has no IP address")?;
            ensure!(
                !advertise || !addr.ip().is_loopback(),
                "--advertise requires a LAN or tailnet bind"
            );
            let on_tailnet = vamp_server::auth::tailnet_ip(addr.ip());
            let auth = auth.unwrap_or(if on_tailnet {
                AuthMode::Tailscale
            } else {
                AuthMode::Token
            });
            let state_dir = library.state_dir.clone();
            // Exits (status 70) when workers stop making progress; launchd KeepAlive restarts.
            let mut app = vamp_server::App::new(library, &name)?.with_watchdog();
            if auth == AuthMode::Tailscale {
                let allow = Allowlist::load(&state_dir, &tailscale)?;
                eprintln!(
                    "vamp-server: tailscale auth; allowed users {:?}, tags {:?} (vamp-server allow --user/--tag)",
                    allow.users, allow.tags
                );
                app = app.with_tailnet(tailscale);
            }
            let dist = dist.or_else(|| Some(home.join("dist")).filter(|d| d.is_dir()));
            if let Some(dir) = dist {
                eprintln!(
                    "vamp-server: distributing {} at /install.sh and /dist/",
                    dir.display()
                );
                app = app.with_dist(dir);
            }
            let _discovery = app.register_discovery(addr, advertise);
            let url = if addr.ip().is_unspecified() {
                format!("http://<this-machine>:{}", addr.port())
            } else {
                format!("http://{addr}")
            };
            let reach = if addr.ip().is_loopback() {
                " (this machine only; --bind tailnet:PORT accepts machines on your Tailscale network)"
            } else {
                ""
            };
            eprintln!("vamp-server: serving context {name} at {url}{reach}");
            eprintln!(
                "vamp-server: token in {} (print it: vamp-server token --context {name})",
                state_dir.join("server-token").display()
            );
            let join = match auth {
                AuthMode::Token => format!("vamp join {url} --token TOKEN"),
                AuthMode::Tailscale => format!("vamp join {url} (allowlisted) or add --token TOKEN"),
            };
            eprintln!(
                "vamp-server: other machines: {join} (later changes: vamp sync); Vampire: Connect to Server…"
            );
            let telemetry = reporter.map(|reporter| Telemetry::new(reporter.clone()));
            if let Some(telemetry) = &telemetry {
                telemetry.observe(Feature::Launch, true, None);
                app = app.with_telemetry(telemetry.clone());
            }
            if !no_jobs {
                vamp_server::spawn_jobs(
                    home.to_path_buf(),
                    name.clone(),
                    Duration::from_secs(jobs_interval.max(1)),
                    app.health(),
                    telemetry,
                );
            }
            vamp_server::serve(std::sync::Arc::new(server), app, threads);
        }
    }
    Ok(())
}
