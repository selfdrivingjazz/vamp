use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use vamp_protocol::{ToolRequest, ToolResponse};

const MAX_OUTPUT: u64 = 4 * 1024 * 1024;
const MAX_ERROR: u64 = 64 * 1024;
/// A persistent processor server exits after this long without requests.
pub const SERVE_IDLE_SECONDS: u64 = 900;

/// One ToolRequest line to a `--serve` processor socket, one ToolResponse line back.
#[cfg(unix)]
pub fn query_server(socket: &Path, request: &ToolRequest, timeout: u64) -> Result<ToolResponse> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket)?;
    let timeout = Some(Duration::from_secs(timeout));
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut response = String::new();
    BufReader::new(stream.take(MAX_OUTPUT)).read_line(&mut response)?;
    ensure!(
        response.ends_with('\n'),
        "processor server closed the connection"
    );
    let value: Value = serde_json::from_str(&response)?;
    if let Some(error) = value.get("error") {
        bail!("processor server error: {error}");
    }
    Ok(serde_json::from_value(value)?)
}

/// Start `command --serve --socket S --idle-seconds N` in the background (own process
/// group, stderr appended to `log`).
#[cfg(unix)]
pub fn spawn_server(command: &[String], socket: &Path, log: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    ensure!(!command.is_empty(), "processor command cannot be empty");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .arg("--serve")
        .arg("--socket")
        .arg(socket)
        .arg("--idle-seconds")
        .arg(SERVE_IDLE_SECONDS.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .with_context(|| format!("cannot start processor server: {}", command[0]))?;
    // Reap on exit so long-lived hosts do not accumulate zombies.
    thread::spawn(move || child.wait());
    Ok(())
}

/// Run `request` on a warm `--serve` instance of `command` at `socket`, starting it first
/// when nothing listens. Servers bind before loading their model, so the query then waits
/// (up to `timeout_seconds`) for the load instead of falling back to a cold one-shot run.
#[cfg(unix)]
pub fn run_served(
    command: &[String],
    request: &ToolRequest,
    timeout_seconds: u64,
    socket: &Path,
    log: &Path,
) -> Result<ToolResponse> {
    use std::os::unix::net::UnixStream;
    static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    {
        let _guard = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        if UnixStream::connect(socket).is_err() {
            spawn_server(command, socket, log)?;
            let started = Instant::now();
            while UnixStream::connect(socket).is_err() {
                ensure!(
                    started.elapsed() < Duration::from_secs(60),
                    "processor server did not open {}; see {}",
                    socket.display(),
                    log.display()
                );
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    query_server(socket, request, timeout_seconds)
}

pub fn run_tool(
    command: &[String],
    request: &ToolRequest,
    timeout_seconds: u64,
) -> Result<ToolResponse> {
    ensure!(
        !command.is_empty() && !command[0].is_empty(),
        "processor command is empty"
    );
    ensure!(
        (1..=3600).contains(&timeout_seconds),
        "processor timeout must be 1–3600 seconds"
    );
    let input = serde_json::to_vec(request)?;
    ensure!(
        input.len() <= 1024 * 1024,
        "processor request exceeds 1 MiB"
    );
    let mut builder = Command::new(&command[0]);
    builder
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        builder.process_group(0);
    }
    let mut child = builder
        .spawn()
        .with_context(|| format!("cannot start processor: {}", command[0]))?;
    let pid = child.id();
    let mut stdin = child.stdin.take().context("processor stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("processor stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("processor stderr unavailable")?;
    let writer = thread::spawn(move || -> std::io::Result<()> {
        stdin.write_all(&input)?;
        stdin.write_all(b"\n")
    });
    let out_reader = thread::spawn(move || read_bounded(stdout, MAX_OUTPUT));
    let err_reader = thread::spawn(move || read_bounded(stderr, MAX_ERROR));
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= Duration::from_secs(timeout_seconds) {
            timed_out = true;
            terminate_group(pid);
            let _ = child.kill();
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(20));
    };
    // Processors are finite jobs. Descendants must not retain pipes after their parent exits.
    terminate_group(pid);
    let written = writer
        .join()
        .map_err(|_| anyhow::anyhow!("processor writer panicked"))?;
    let output = out_reader
        .join()
        .map_err(|_| anyhow::anyhow!("processor stdout reader panicked"))??;
    let error = err_reader
        .join()
        .map_err(|_| anyhow::anyhow!("processor stderr reader panicked"))??;
    ensure!(
        !timed_out,
        "processor timed out after {timeout_seconds} seconds"
    );
    if !status.success() {
        bail!(
            "processor exited with {status}: {}",
            String::from_utf8_lossy(&error)
        );
    }
    written.context("processor closed stdin before consuming request")?;
    ensure!(
        output.len() <= MAX_OUTPUT as usize,
        "processor output exceeds 4 MiB"
    );
    serde_json::from_slice(&output)
        .context("processor stdout must contain exactly one ToolResponse JSON object")
}
fn read_bounded(reader: impl Read, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn terminate_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}
