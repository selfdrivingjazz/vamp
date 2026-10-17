//! Owner-only outbound transport. Library content is never queued or logged.
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Args as ClapArgs, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, fs, io::{BufRead, BufReader, Read, Write}, path::{Path, PathBuf}, process::{Child, Command, Stdio}, sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc::{self, SyncSender}}, thread, time::{Duration, Instant}};
use tungstenite::{Message, client::IntoClientRequest, stream::MaybeTlsStream};

const DEFAULT_ENDPOINT: &str = "https://mcp.vamp.sh";
const MAX_REQUEST: usize = 1 << 20;
const MAX_RESPONSE: usize = 16 << 20;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    command: RelayCommand,
}

#[derive(Subcommand)]
enum RelayCommand {
    /// Open account sign-in, or install a tunnel token downloaded from the account page.
    Login {
        /// Owner-only file containing the scoped tunnel token (never pass a token as an argument).
        #[arg(long)]
        token_file: Option<PathBuf>,
        #[arg(long, default_value = DEFAULT_ENDPOINT)]
        endpoint: String,
    },
    /// Keep this computer connected; no inbound network port is exposed.
    Connect {
        /// Explicitly allow tools that change the library; OAuth consent must also allow writes.
        #[arg(long)]
        read_write: bool,
    },
    /// Show configuration without revealing credentials.
    Status,
    /// Remove this computer's saved credential (revoke it on the account page too).
    Logout,
}

#[derive(Serialize, Deserialize)]
struct Config {
    endpoint: String,
    token: String,
    account_id: String,
    mcp_url: String,
    websocket_url: String,
}

#[derive(Deserialize)]
struct RemoteConfig {
    account_id: String,
    mcp_url: String,
    websocket_url: String,
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .max_redirects(0)
        .build().into()
}

fn endpoint_url(endpoint: &str) -> Result<tungstenite::http::Uri> {
    let uri: tungstenite::http::Uri = endpoint.parse().context("invalid relay endpoint")?;
    let host = uri.host().context("relay endpoint needs a host")?;
    ensure!(uri.scheme_str() == Some("https") || (uri.scheme_str() == Some("http") && matches!(host, "localhost" | "127.0.0.1" | "[::1]")), "relay endpoint must use HTTPS (HTTP is allowed only on loopback)");
    ensure!(uri.path() == "/" && uri.query().is_none() && !endpoint.contains('@'), "relay endpoint must be an origin without credentials, path or query");
    Ok(uri)
}

fn private_read(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path).context("reading relay credential metadata")?;
    ensure!(metadata.file_type().is_file(), "relay credential must be a regular file, not a symlink");
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        ensure!(metadata.permissions().mode() & 0o077 == 0, "relay credential must be owner-only (chmod 600)");
    }
    ensure!(metadata.len() <= 16_384, "relay credential file is too large");
    fs::read_to_string(path).context("reading relay credential")
}

fn load(home: &Path) -> Result<Config> {
    serde_json::from_str(&private_read(&home.join("relay.json"))?).context("reading relay configuration; run vamp relay login")
}

pub fn run(args: Args, home: &Path, context: Option<&str>) -> Result<Value> {
    match args.command {
        RelayCommand::Login { token_file, endpoint } => {
            let endpoint = endpoint.trim_end_matches('/').to_owned();
            endpoint_url(&endpoint)?;
            let Some(file) = token_file else {
                let url = format!("{endpoint}/account");
                // Opening a browser is best-effort; the returned URL also works on headless machines.
                #[cfg(target_os = "macos")]
                let _ = Command::new("open").arg(&url).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
                #[cfg(target_os = "linux")]
                let _ = Command::new("xdg-open").arg(&url).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
                return Ok(json!({"login_url":url,"next":"Sign in, download a tunnel token, then run vamp relay login --token-file FILE."}));
            };
            let raw = private_read(&file)?;
            let token = if raw.trim_start().starts_with('{') {
                serde_json::from_str::<Value>(&raw)?.get("token").and_then(Value::as_str).context("token file needs a token field")?.to_owned()
            } else { raw.trim().to_owned() };
            ensure!(token.starts_with("vat_") && token.len() == 47 && token.bytes().skip(4).all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'), "invalid tunnel token file");
            let mut response = http_agent().get(format!("{endpoint}/api/relay/config")).header("authorization", format!("Bearer {token}")).call().context("connecting to relay")?;
            ensure!(response.status().is_success(), "relay rejected tunnel credential or relay subscription is inactive (HTTP {})", response.status().as_u16());
            let remote: RemoteConfig = serde_json::from_slice(&response.body_mut().with_config().limit(16_384).read_to_vec()?)?;
            ensure!(remote.mcp_url == format!("{endpoint}/mcp/{}",remote.account_id), "relay returned an unexpected resource URL");
            let expected_ws = format!("{}/api/relay/tunnel", endpoint.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1));
            ensure!(remote.websocket_url == expected_ws, "relay returned an unexpected tunnel URL");
            fs::create_dir_all(home)?;
            let config = Config { endpoint, token, account_id: remote.account_id, mcp_url: remote.mcp_url, websocket_url: remote.websocket_url };
            // create_new avoids following an attacker-controlled staging symlink.
            let temp = home.join(format!(".relay-{}.json", std::process::id()));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
            let result = (|| -> Result<()> { let mut f = options.open(&temp)?; f.write_all(&serde_json::to_vec(&config)?)?; f.sync_all()?; fs::rename(&temp, home.join("relay.json"))?; Ok(()) })();
            if result.is_err() { let _ = fs::remove_file(&temp); }
            result?;
            Ok(json!({"configured":true,"account_id":config.account_id,"mcp_url":config.mcp_url,"read_only_default":true}))
        }
        RelayCommand::Status => {
            if !home.join("relay.json").exists() { return Ok(json!({"configured":false})); }
            let config = load(home)?;
            Ok(json!({"configured":true,"account_id":config.account_id,"mcp_url":config.mcp_url,"read_only_default":true}))
        }
        RelayCommand::Logout => {
            match fs::remove_file(home.join("relay.json")) { Ok(()) => (), Err(e) if e.kind() == std::io::ErrorKind::NotFound => (), Err(e) => return Err(e.into()) }
            Ok(json!({"configured":false,"note":"Revoke the tunnel token on your account page to invalidate copies."}))
        }
        RelayCommand::Connect { read_write } => connect(home, context, read_write),
    }
}

struct LocalServer(Child);
impl Drop for LocalServer {
    fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
}

fn local_server(home: &Path, context: Option<&str>, read_write: bool) -> Result<(LocalServer, String)> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("--home").arg(home);
    if let Some(context) = context { command.arg("--context").arg(context); }
    command.args(["mcp", "--http", "127.0.0.1:0", if read_write { "--read-write" } else { "--read-only" }]);
    let mut child = LocalServer(command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().context("starting local MCP handler")?);
    let stderr = child.0.stderr.take().context("local MCP startup channel missing")?;
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        // Drain stderr for the child's lifetime, without logging or retaining arbitrary text.
        let mut reader = BufReader::new(stderr);
        loop {
            let mut line = String::new();
            match reader.by_ref().take(8192).read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Some(url) = line.trim().strip_prefix("VAMP_MCP_HTTP=") { let _ = tx.try_send(url.to_owned()); }
                }
            }
        }
    });
    let url = rx.recv_timeout(Duration::from_secs(15)).context("local MCP handler did not announce its endpoint")?;
    let uri: tungstenite::http::Uri = url.parse()?;
    ensure!(uri.scheme_str() == Some("http") && uri.host() == Some("127.0.0.1") && uri.port_u16().is_some_and(|port| port != 0) && uri.path() == "/mcp" && uri.query().is_none(), "local MCP announced an invalid endpoint");
    Ok((child, url))
}

#[derive(Deserialize)]
struct RequestFrame {
    id: String,
    method: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    body: String,
    scope: String,
}

fn emit(tx: &SyncSender<Value>, cancelled: &AtomicBool, value: Value) -> Result<()> {
    ensure!(!cancelled.load(Ordering::Relaxed), "request cancelled");
    tx.send(value).context("relay disconnected")
}

fn forward(frame: RequestFrame, url: &str, read_write: bool, cancelled: &AtomicBool, tx: &SyncSender<Value>) -> Result<()> {
    ensure!(frame.method == "POST" && frame.body.len() <= MAX_REQUEST, "unsupported or oversized request");
    ensure!(matches!(frame.scope.as_str(), "read" | "write"), "invalid request scope");
    ensure!(frame.scope != "write" || read_write, "writes are disabled locally");
    let agent = http_agent();
    let mut request = agent.post(url).header("content-type", "application/json")
        .header("X-Vamp-Mcp-Scope", if frame.scope == "write" && read_write { "write" } else { "read" });
    for (key, value) in &frame.headers {
        if matches!(key.to_ascii_lowercase().as_str(), "accept" | "mcp-protocol-version" | "mcp-session-id" | "last-event-id") { request = request.header(key, value); }
    }
    let mut response = request.send(frame.body.as_bytes())?;
    let headers: HashMap<String, String> = response.headers().iter().filter_map(|(key,value)| {
        matches!(key.as_str(), "content-type" | "mcp-session-id" | "mcp-protocol-version" | "retry-after").then(|| value.to_str().ok().map(|v|(key.to_string(),v.to_owned()))).flatten()
    }).collect();
    emit(tx, cancelled, json!({"type":"start","id":frame.id,"status":response.status().as_u16(),"headers":headers}))?;
    let mut reader = response.body_mut().as_reader();
    let mut buffer = [0u8; 32_768];
    let mut total = 0;
    loop {
        ensure!(!cancelled.load(Ordering::Relaxed), "request cancelled");
        let n = reader.read(&mut buffer)?;
        if n == 0 { break; }
        total += n;
        ensure!(total <= MAX_RESPONSE, "response exceeds relay limit");
        emit(tx, cancelled, json!({"type":"chunk","id":frame.id,"data":STANDARD.encode(&buffer[..n])}))?;
    }
    emit(tx, cancelled, json!({"type":"end","id":frame.id}))
}

fn session(config: &Config, local_url: &str, read_write: bool, stopped: &AtomicBool) -> Result<()> {
    let mut request = config.websocket_url.as_str().into_client_request()?;
    request.headers_mut().insert("authorization", format!("Bearer {}",config.token).parse()?);
    // JSON framing can escape each source byte to six bytes. Bound before tungstenite allocates.
    let websocket_config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_REQUEST * 6 + 65_536))
        .max_frame_size(Some(MAX_REQUEST * 6 + 65_536));
    // Never forward the tunnel credential across a redirect.
    let (mut socket, _) = tungstenite::client::connect_with_config(request, Some(websocket_config), 0).context("opening relay tunnel")?;
    match socket.get_mut() {
        MaybeTlsStream::Plain(tcp) => { tcp.set_read_timeout(Some(Duration::from_millis(200)))?; tcp.set_write_timeout(Some(Duration::from_secs(10)))?; },
        MaybeTlsStream::Rustls(tls) => { tls.sock.set_read_timeout(Some(Duration::from_millis(200)))?; tls.sock.set_write_timeout(Some(Duration::from_secs(10)))?; },
        _ => bail!("unsupported relay TLS transport"),
    }
    eprintln!("Relay connected ({}).", if read_write { "writes require OAuth consent" } else { "read-only" });
    let (tx, rx) = mpsc::sync_channel::<Value>(32);
    let mut jobs: HashMap<String, (Arc<AtomicBool>, thread::JoinHandle<()>)> = HashMap::new();
    let mut heartbeat = Instant::now();
    let mut received = Instant::now();
    let result = (|| -> Result<()> {
        loop {
            if stopped.load(Ordering::Relaxed) { let _ = socket.close(None); return Ok(()); }
            while let Ok(value) = rx.try_recv() { socket.send(Message::Text(serde_json::to_string(&value)?.into()))?; }
            jobs.retain(|_, (_, handle)| !handle.is_finished());
            if heartbeat.elapsed() >= Duration::from_secs(25) { socket.send(Message::Text("ping".into()))?; heartbeat = Instant::now(); }
            ensure!(received.elapsed() < Duration::from_secs(90), "relay heartbeat timed out");
            let message = match socket.read() {
                Ok(message) => { received = Instant::now(); message },
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(),std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                Err(error) => return Err(error.into()),
            };
            match message {
                Message::Text(text) if text == "pong" => (),
                Message::Text(text) => {
                    ensure!(text.len() <= MAX_REQUEST * 6 + 65_536, "relay frame is too large");
                    let value: Value = serde_json::from_str(&text)?;
                    match value.get("type").and_then(Value::as_str) {
                        Some("cancel") => { if let Some((cancelled,_)) = value.get("id").and_then(Value::as_str).and_then(|id|jobs.get(id)) { cancelled.store(true,Ordering::Relaxed); } },
                        Some("request") => {
                            let frame: RequestFrame = serde_json::from_value(value)?;
                            ensure!(!frame.id.is_empty() && frame.id.len() <= 128 && !jobs.contains_key(&frame.id), "invalid relay request id");
                            if jobs.len() >= 8 { socket.send(Message::Text(json!({"type":"error","id":frame.id}).to_string().into()))?; continue; }
                            let id = frame.id.clone();
                            let cancelled = Arc::new(AtomicBool::new(false));
                            let worker_cancelled = Arc::clone(&cancelled);
                            let tx = tx.clone();
                            let url = local_url.to_owned();
                            let worker_id = id.clone();
                            let handle = thread::spawn(move || {
                                if forward(frame,&url,read_write,&worker_cancelled,&tx).is_err() { let _ = emit(&tx,&worker_cancelled,json!({"type":"error","id":worker_id})); }
                            });
                            jobs.insert(id,(cancelled,handle));
                        },
                        _ => bail!("unsupported relay frame"),
                    }
                },
                Message::Ping(data) => socket.send(Message::Pong(data))?,
                Message::Close(_) => bail!("relay connection closed"),
                _ => (),
            }
        }
    })();
    for (cancelled, _) in jobs.values() { cancelled.store(true, Ordering::Relaxed); }
    // Dropping the receiver releases workers blocked by network backpressure.
    drop(rx);
    result
}

fn connect(home: &Path, context: Option<&str>, read_write: bool) -> Result<Value> {
    let config = load(home)?;
    endpoint_url(&config.endpoint)?;
    let stopped = Arc::new(AtomicBool::new(false));
    let signal_stopped = Arc::clone(&stopped);
    ctrlc::set_handler(move || signal_stopped.store(true, Ordering::Relaxed)).context("installing relay shutdown handler")?;
    let (mut local, url) = local_server(home, context, read_write)?;
    let mut failures = 0u32;
    loop {
        if stopped.load(Ordering::Relaxed) { return Ok(json!({"connected":false})); }
        ensure!(home.join("relay.json").exists(), "relay logged out");
        ensure!(local.0.try_wait()?.is_none(), "local MCP handler exited");
        let started = Instant::now();
        // Never print remote errors: they can contain credentials or library payloads.
        let _ = session(&config,&url,read_write,&stopped);
        if started.elapsed() > Duration::from_secs(60) { failures = 0; }
        failures = failures.saturating_add(1);
        let base = 1u64 << failures.min(6);
        let jitter = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_millis() as u64 % 1000;
        eprintln!("Relay disconnected; reconnecting.");
        let until = Instant::now() + Duration::from_millis(base * 1000 + jitter);
        while Instant::now() < until && !stopped.load(Ordering::Relaxed) { thread::sleep(Duration::from_millis(100)); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_requires_secure_origin() {
        assert!(endpoint_url("https://mcp.vamp.sh").is_ok());
        assert!(endpoint_url("http://127.0.0.1:7791").is_ok());
        for bad in ["http://example.com", "https://example.com/path", "https://user@example.com", "https://example.com/?token=secret"] { assert!(endpoint_url(bad).is_err()); }
    }
    #[test]
    fn local_write_gate_precedes_network() {
        let (tx,_rx) = mpsc::sync_channel(1);
        let frame = RequestFrame { id:"x".into(),method:"POST".into(),headers:HashMap::new(),body:"{}".into(),scope:"write".into() };
        assert!(forward(frame,"http://127.0.0.1:1/mcp",false,&AtomicBool::new(false),&tx).unwrap_err().to_string().contains("disabled"));
    }
}
