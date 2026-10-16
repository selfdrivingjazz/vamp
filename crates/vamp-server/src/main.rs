use anyhow::{Context as _, Result, anyhow, ensure};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Duration;
use vamp_core::{Library, default_home, load_config};
use vamp_server::auth::{Allowlist, AuthMode, Tailscale};

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
        /// for another port). Use 0.0.0.0:7777 or a Tailscale address to accept other machines.
        #[arg(long, default_value = "127.0.0.1:7777")]
        bind: String,
        /// Default: `tailscale` when bound to a tailnet address, else `token`.
        #[arg(long, value_enum)]
        auth: Option<AuthMode>,
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
    InstallAgent {
        #[arg(long, default_value = "127.0.0.1:7777")]
        bind: String,
        #[arg(long)]
        no_jobs: bool,
        /// Pass `--dist DIR` to serve (installer + release files).
        #[arg(long)]
        dist: Option<PathBuf>,
        /// Replace an existing plist.
        #[arg(long)]
        force: bool,
    },
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

fn context_name(cli: &Cli, home: &Path) -> Result<String> {
    match &cli.context {
        Some(name) => Ok(name.clone()),
        None => load_config(home)?
            .current
            .ok_or_else(|| anyhow!("no active context; pass --context")),
    }
}

fn install_agent(
    home: &Path,
    name: &str,
    bind: &str,
    no_jobs: bool,
    dist: Option<&Path>,
    force: bool,
) -> Result<()> {
    let user_home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let label = format!("com.selfdrivingjazz.vamp.server.{name}");
    let agents = user_home.join("Library/LaunchAgents");
    let path = agents.join(format!("{label}.plist"));
    ensure!(
        force || !path.exists(),
        "{} exists; pass --force to replace it",
        path.display()
    );
    let program = std::env::current_exe()?.canonicalize()?;
    let mut args = vec![
        "serve".to_owned(),
        "--context".to_owned(),
        name.to_owned(),
        "--bind".to_owned(),
        bind.to_owned(),
    ];
    if no_jobs {
        args.push("--no-jobs".into());
    }
    if let Some(dist) = dist {
        args.push("--dist".into());
        args.push(dist.canonicalize()?.to_string_lossy().into_owned());
    }
    let mut env = vec![(
        "VAMP_HOME",
        home.canonicalize()?.to_string_lossy().into_owned(),
    )];
    for key in ["VAMP_MEDIA_HELPER", "VAMP_STORE_HELPER"] {
        if let Some(helper) = std::env::var_os(key) {
            env.push((key, helper.to_string_lossy().into_owned()));
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
    let name = context_name(&cli, &home)?;
    let library = Library::open(&home, Some(&name))?;
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
        Command::InstallAgent {
            bind,
            no_jobs,
            dist,
            force,
        } => install_agent(&home, &name, &bind, no_jobs, dist.as_deref(), force)?,
        Command::Serve {
            bind,
            auth,
            threads,
            no_jobs,
            jobs_interval,
            dist,
        } => {
            let tailscale = Tailscale::discover();
            let bind = resolve_bind(&bind, &tailscale)?;
            let server = tiny_http::Server::http(&bind).map_err(|e| anyhow!("bind {bind}: {e}"))?;
            let on_tailnet = server
                .server_addr()
                .to_ip()
                .is_some_and(|a| vamp_server::auth::tailnet_ip(a.ip()));
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
            let addr = server.server_addr().to_ip().context("server has no IP address")?;
            let url = if addr.ip().is_unspecified() {
                format!("http://<this-machine>:{}", addr.port())
            } else {
                format!("http://{addr}")
            };
            let reach = if addr.ip().is_loopback() {
                " (this machine only; --bind 0.0.0.0:PORT accepts other machines)"
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
                "vamp-server: other machines: {join} (later changes: vamp sync); Vampire app: Connect to Server… with this URL and token"
            );
            if !no_jobs {
                vamp_server::spawn_jobs(
                    home.clone(),
                    name.clone(),
                    Duration::from_secs(jobs_interval.max(1)),
                    app.health(),
                );
            }
            vamp_server::serve(std::sync::Arc::new(server), app, threads);
        }
    }
    Ok(())
}
