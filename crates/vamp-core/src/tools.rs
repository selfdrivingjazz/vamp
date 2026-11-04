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
pub fn spawn_server(command: &[String], socket: &Path, log: &Path, idle_seconds: u64) -> Result<()> {
    use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
    ensure!((1..=86400).contains(&idle_seconds), "idle timeout must be 1–86400 seconds");
    ensure!(!command.is_empty(), "processor command cannot be empty");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(log)?;
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .arg("--serve")
        .arg("--socket")
        .arg(socket)
        .arg("--idle-seconds")
        .arg(idle_seconds.to_string())
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
    idle_seconds: u64,
) -> Result<ToolResponse> {
    use std::os::unix::net::UnixStream;
    // The lock is per socket and cross-process: two CLI batches must not both start it.
    let _guard = server_lock(socket)?;
    match UnixStream::connect(socket) {
        Ok(_) => {}
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
            if let Ok(metadata) = std::fs::symlink_metadata(socket) {
                use std::os::unix::fs::{FileTypeExt, MetadataExt};
                ensure!(metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
                    "refusing to replace a non-owned socket: {}", socket.display());
                std::fs::remove_file(socket)?;
            }
            spawn_server(command, socket, log, idle_seconds)?;
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
        Err(error) => return Err(error.into()),
    }
    drop(_guard);
    query_server(socket, request, timeout_seconds)
}

#[cfg(unix)]
fn server_lock(socket: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .mode(0o600).custom_flags(libc::O_NOFOLLOW).open(socket.with_extension("server-lock"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

/// Stop the process actually listening on this socket, not a possibly reused pidfile PID.
#[cfg(unix)]
pub fn stop_server(socket: &Path) -> Result<bool> {
    use std::os::unix::{fs::{FileTypeExt, MetadataExt}, net::UnixStream};
    let _guard = server_lock(socket)?;
    let metadata = match std::fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    ensure!(metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
        "refusing to stop a non-owned socket: {}", socket.display());
    let stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            std::fs::remove_file(socket)?;
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    let pid = server_pid(&stream)?;
    ensure!(pid > 1 && pid != std::process::id() as i32, "invalid processor server PID");
    signal_server(pid, libc::SIGTERM)?;
    use std::os::fd::AsRawFd;
    let started = Instant::now();
    loop {
        // Observe the existing connection instead of filling a busy server's accept queue.
        let mut byte = 0u8;
        let received = unsafe { libc::recv(stream.as_raw_fd(), &mut byte as *mut _ as *mut libc::c_void,
            1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
        if received == 0 { return Ok(true); }
        if received < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock {
            return Ok(true);
        }
        if started.elapsed() >= Duration::from_secs(2) {
            signal_server(pid, libc::SIGKILL)?;
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
fn signal_server(pid: i32, signal: i32) -> Result<()> {
    if unsafe { libc::kill(pid, signal) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) { return Err(error.into()); }
    }
    Ok(())
}

#[cfg(unix)]
fn server_pid(stream: &std::os::unix::net::UnixStream) -> Result<i32> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "linux")]
    let (level, option, mut credentials) = (libc::SOL_SOCKET, libc::SO_PEERCRED, unsafe { std::mem::zeroed::<libc::ucred>() });
    #[cfg(target_os = "macos")]
    let (level, option, mut credentials) = (libc::SOL_LOCAL, libc::LOCAL_PEERPID, 0 as libc::pid_t);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut size = std::mem::size_of_val(&credentials) as libc::socklen_t;
        ensure!(unsafe { libc::getsockopt(stream.as_raw_fd(), level, option,
            &mut credentials as *mut _ as *mut libc::c_void, &mut size) } == 0,
            "cannot identify processor server: {}", std::io::Error::last_os_error());
        #[cfg(target_os = "linux")]
        return Ok(credentials.pid);
        #[cfg(target_os = "macos")]
        return Ok(credentials);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    bail!("stopping warm processors is supported on macOS and Linux");
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
