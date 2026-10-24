//! `vamp setup`: optional extras for this machine.
//!
//! `plan` (read-only) reports machine facts, each extra's state, size and speed, and what this
//! machine should install. `install` sets up smart search (SigLIP image and text embeddings)
//! and captions (Florence-2 descriptions and tags) in one uv environment at
//! `<home>/tools/models`, times one cold run of each processor, registers the processors in the
//! active (or --context) library and queues its existing items. `decline` records that an
//! extra is not wanted. Choices live in `<home>/setup.json`.

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    env,
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};
use vamp_core::{EmbedderSpec, Library};
use vamp_protocol::{ProcessorSpec, ToolRequest};

const FLORENCE_SCRIPT: &str = include_str!("../../../processors/florence/processor.py");
const SIGLIP_SCRIPT: &str = include_str!("../../../processors/siglip/processor.py");
const PYPROJECT: &str = include_str!("../../../processors/pyproject.toml");
const SIGLIP_MODEL: &str = "google/siglip-base-patch16-224";
const SIGLIP_REVISION: &str = "7fd15f0689c79d79e38b1c2e2e2370a7bf2761ed";
const FLORENCE_MODEL: &str = "florence-community/Florence-2-base";
const FLORENCE_REVISION: &str = "00921df66db728a9ceb750f5eca43e5c203a2051";
const DEVICES: [&str; 4] = ["auto", "cpu", "cuda", "mps"];
const CHOICES_FILE: &str = "setup.json";

/// Recommendation floors; unknown values count as enough. Machines sold with 8 GB report a
/// little less memory (firmware and kernel reservations), hence 7.5 GiB.
const MIN_MEMORY: u64 = 15 << 29;
const MIN_FREE: u64 = 5_000_000_000;

/// (download, disk) bytes of the shared runtime by PyTorch build: uv-managed Python 3.11, the
/// pinned packages of processors/pyproject.toml with uv's cache of them, and the uv download
/// when none is installed. Measured 2026-10-24 with `vamp setup install search` minus the
/// SigLIP weights: Linux `--torch-backend cpu` in a fresh debian:stable-slim container;
/// `cu128` in the same container with an RTX 4090; macOS (PyPI's arm64 build, APFS clones
/// share the cache's blocks) in a throwaway home on a Mac mini M4.
const RUNTIME_CPU: (u64, u64) = (265_000_000, 1_160_000_000);
const RUNTIME_CUDA: (u64, u64) = (4_020_000_000, 7_150_000_000);
const RUNTIME_MACOS: (u64, u64) = (170_000_000, 700_000_000);
/// Model weights in `tools/cache/hf` (safetensors, so download = disk), measured 2026-10-24.
const SIGLIP_WEIGHTS: u64 = 816_000_000;
const FLORENCE_WEIGHTS: u64 = 469_000_000;

/// Seconds per item until `install` measures this machine: one cold run per job, as the local
/// drain (`vamp serve`, `vamp jobs run`) runs them; captions run two processors (description
/// and tags). (search, captions), measured 2026-10-24 by `vamp setup install` on its sample
/// image: RTX 4090 with --device cuda (5.5 s; 7.8 + 7.4 s), Mac mini M4 CPU (2.5 s; 5.0 +
/// 3.7 s), 16-thread Ryzen 7 2700X CPU (4.6 s; 14.2 + 12.8 s).
const SECONDS_CUDA: (f64, f64) = (5.5, 15.2);
const SECONDS_APPLE: (f64, f64) = (2.5, 8.7);
const SECONDS_CPU: (f64, f64) = (4.6, 27.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extra {
    Search,
    Captions,
}
impl Extra {
    const ALL: [Self; 2] = [Self::Search, Self::Captions];
    pub fn name(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Captions => "captions",
        }
    }
    fn title(self) -> &'static str {
        match self {
            Self::Search => "Smart search",
            Self::Captions => "Captions",
        }
    }
    fn enables(self) -> &'static str {
        match self {
            Self::Search => "search by meaning, Find Similar, Riff and Galaxy",
            Self::Captions => "descriptions and tags for every image",
        }
    }
    fn script(self) -> (&'static str, &'static str) {
        match self {
            Self::Search => ("siglip.py", SIGLIP_SCRIPT),
            Self::Captions => ("florence.py", FLORENCE_SCRIPT),
        }
    }
    fn manifest(self) -> &'static str {
        match self {
            Self::Search => "siglip.json",
            Self::Captions => "florence.json",
        }
    }
    fn model(self) -> (&'static str, &'static str) {
        match self {
            Self::Search => (SIGLIP_MODEL, SIGLIP_REVISION),
            Self::Captions => (FLORENCE_MODEL, FLORENCE_REVISION),
        }
    }
    fn weights(self) -> u64 {
        match self {
            Self::Search => SIGLIP_WEIGHTS,
            Self::Captions => FLORENCE_WEIGHTS,
        }
    }
    /// (processor name, extra arguments, timeout seconds)
    fn processors(self) -> &'static [(&'static str, &'static [&'static str], u64)] {
        match self {
            Self::Search => &[("siglip-embedding", &[], 300)],
            Self::Captions => &[
                ("florence-description", &[], 600),
                ("florence-tags", &["--task", "tags"], 600),
            ],
        }
    }
}

/// `search` and `captions` as named; `recommended` expands to `recommended()`.
fn select(names: &[String], recommended: impl FnOnce() -> Vec<Extra>) -> Result<Vec<Extra>> {
    let mut recommended = Some(recommended);
    let mut extras = Vec::new();
    for name in names {
        let picked = match name.as_str() {
            "search" => vec![Extra::Search],
            "captions" => vec![Extra::Captions],
            "recommended" => recommended.take().map(|f| f()).unwrap_or_default(),
            other => bail!("unknown extra {other:?}; expected search, captions or recommended"),
        };
        for extra in picked {
            if !extras.contains(&extra) {
                extras.push(extra);
            }
        }
    }
    extras.sort_by_key(|extra| Extra::ALL.iter().position(|e| e == extra));
    Ok(extras)
}

/// `extras`, plus the extras already in the shared environment when `device` differs from the
/// `recorded` one: they share its PyTorch build, so they move too and are timed and registered
/// again with the new device.
fn with_moved(
    mut extras: Vec<Extra>,
    models: &Models,
    recorded: Option<&str>,
    device: &str,
) -> Vec<Extra> {
    if recorded != Some(device) {
        for extra in Extra::ALL {
            if models.prepared(extra) && !extras.contains(&extra) {
                extras.push(extra);
            }
        }
        extras.sort_by_key(|extra| Extra::ALL.iter().position(|e| e == extra));
    }
    extras
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .context("setup paths must be valid UTF-8")
}

fn absolute(home: &Path) -> Result<PathBuf> {
    Ok(if home.is_absolute() {
        home.to_path_buf()
    } else {
        env::current_dir()?.join(home)
    })
}

/// The shared model environment `<home>/tools/models`: one uv venv, a script and a weights
/// manifest per extra, and `device`, the device its PyTorch build was installed for.
struct Models {
    root: PathBuf,
    python: PathBuf,
}
impl Models {
    fn new(home: &Path) -> Self {
        let root = home.join("tools/models");
        let python = if cfg!(windows) {
            root.join("Scripts/python.exe")
        } else {
            root.join("bin/python")
        };
        Self { root, python }
    }
    fn script(&self, extra: Extra) -> PathBuf {
        self.root.join(extra.script().0)
    }
    fn manifest(&self, extra: Extra) -> PathBuf {
        self.root.join(extra.manifest())
    }
    fn recorded_device(&self) -> Option<String> {
        fs::read_to_string(self.root.join("device"))
            .ok()
            .map(|device| device.trim().to_owned())
            .filter(|device| !device.is_empty())
    }
    /// The recorded device, else this machine's default.
    fn device(&self) -> String {
        self.recorded_device()
            .unwrap_or_else(|| default_device(nvidia_gpu().is_some()).into())
    }
    /// Runtime, a script (of any vamp version) and a verified weights manifest are present.
    fn prepared(&self, extra: Extra) -> bool {
        let (model, revision) = extra.model();
        self.python.is_file()
            && self.script(extra).is_file()
            && manifest_present(&self.manifest(extra), model, revision)
    }
    /// Prepared, with this vamp's script.
    fn installed(&self, extra: Extra) -> bool {
        self.prepared(extra)
            && fs::read_to_string(self.script(extra)).is_ok_and(|text| text == extra.script().1)
    }
    /// Replace the scripts of prepared extras written by an earlier vamp with this vamp's.
    fn refresh_scripts(&self) -> Result<()> {
        for extra in Extra::ALL {
            if self.prepared(extra) && !self.installed(extra) {
                let script = self.script(extra);
                fs::write(&script, extra.script().1)
                    .with_context(|| format!("cannot update {}", script.display()))?;
            }
        }
        Ok(())
    }
    fn command(&self, extra: Extra, device: &str, arguments: &[&str]) -> Result<Vec<String>> {
        let mut argv = vec![
            path_string(&self.python)?,
            path_string(&self.script(extra))?,
            "--manifest".into(),
            path_string(&self.manifest(extra))?,
            "--device".into(),
            device.into(),
        ];
        argv.extend(arguments.iter().map(|argument| (*argument).to_owned()));
        Ok(argv)
    }
    /// The extra's processors. Their commands also accept --serve --socket PATH --idle-seconds
    /// N, which `vamp worker` uses to keep them warm.
    fn specs(&self, extra: Extra, device: &str) -> Result<Vec<ProcessorSpec>> {
        extra
            .processors()
            .iter()
            .map(|&(name, arguments, timeout_seconds)| {
                Ok(ProcessorSpec {
                    name: name.into(),
                    events: vec!["representation_ready".into()],
                    command: self.command(extra, device, arguments)?,
                    input: "preview".into(),
                    output: "observation".into(),
                    timeout_seconds,
                    placement: Default::default(),
                })
            })
            .collect()
    }
    /// The SigLIP text embedder for semantic queries. Its warm server benefits from an
    /// accelerator (~10 ms on MPS vs ~23 ms CPU per query), so it always uses `auto` (CUDA,
    /// then MPS, then CPU); the model is the image embeddings' either way.
    fn embedder(&self) -> Result<EmbedderSpec> {
        Ok(EmbedderSpec {
            command: self.command(Extra::Search, "auto", &[])?,
            model: SIGLIP_MODEL.into(),
            timeout_seconds: 120,
            serve: true,
        })
    }
}

fn manifest_present(manifest: &Path, model: &str, revision: &str) -> bool {
    fs::read(manifest)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|state| {
            state["model"].as_str() == Some(model)
                && state["revision"].as_str() == Some(revision)
                && state["path"].as_str().is_some_and(|path| {
                    let path = Path::new(path);
                    path.join("config.json").is_file()
                        && (path.join("model.safetensors").is_file()
                            || path.join("model.safetensors.index.json").is_file())
                })
        })
}

/// `cuda` with an NVIDIA GPU, else `cpu` (one-shot Florence was slower on MPS than on the CPU
/// of Apple silicon, so macOS stays on CPU unless asked).
fn default_device(nvidia: bool) -> &'static str {
    if nvidia { "cuda" } else { "cpu" }
}

/// `uv pip install --torch-backend` for a device: CUDA 12.8 wheels for cuda (and auto on an
/// NVIDIA machine); elsewhere CPU wheels on Linux and Windows, where PyPI's default torch
/// bundles several GB of CUDA libraries; PyPI's default build on macOS.
fn torch_backend(device: &str, os: &str, nvidia: bool) -> Option<&'static str> {
    if device == "cuda" || (device == "auto" && nvidia) {
        Some("cu128")
    } else if os == "macos" {
        None
    } else {
        Some("cpu")
    }
}

#[derive(Clone, Debug)]
struct Gpu {
    vendor: &'static str,
    name: String,
}

#[derive(Clone, Debug)]
struct Machine {
    os: &'static str,
    arch: &'static str,
    cpu: Option<String>,
    cores: Option<usize>,
    memory_bytes: Option<u64>,
    free_bytes: Option<u64>,
    gpu: Option<Gpu>,
}
impl Machine {
    fn detect(home: &Path) -> Self {
        let (os, arch) = (env::consts::OS, env::consts::ARCH);
        let cpu = cpu_name();
        let gpu = nvidia_gpu().or_else(|| {
            (os == "macos" && arch == "aarch64").then(|| Gpu {
                vendor: "apple",
                name: cpu.clone().unwrap_or_else(|| "Apple silicon".into()),
            })
        });
        Self {
            os,
            arch,
            cpu,
            cores: std::thread::available_parallelism().ok().map(usize::from),
            memory_bytes: memory_bytes(),
            free_bytes: free_bytes(home),
            gpu,
        }
    }
    fn nvidia(&self) -> bool {
        self.gpu.as_ref().is_some_and(|gpu| gpu.vendor == "nvidia")
    }
    fn json(&self) -> Value {
        json!({"os":self.os, "arch":self.arch, "cpu":self.cpu, "cores":self.cores,
            "memory_bytes":self.memory_bytes, "free_bytes":self.free_bytes,
            "gpu":self.gpu.as_ref().map(|gpu| json!({"vendor":gpu.vendor, "name":gpu.name}))})
    }
}

/// Trimmed stdout of a successful command, if any.
fn output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !text.is_empty()).then_some(text)
}

fn cpu_name() -> Option<String> {
    if cfg!(target_os = "macos") {
        return output("sysctl", &["-n", "machdep.cpu.brand_string"]);
    }
    fs::read_to_string("/proc/cpuinfo")
        .ok()?
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == "model name")
                .then(|| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
}

fn memory_bytes() -> Option<u64> {
    if cfg!(target_os = "macos") {
        return output("sysctl", &["-n", "hw.memsize"])?.parse().ok();
    }
    fs::read_to_string("/proc/meminfo")
        .ok()?
        .lines()
        .find_map(|line| {
            let kib = line.strip_prefix("MemTotal:")?.trim().strip_suffix("kB")?;
            kib.trim().parse::<u64>().ok().map(|kib| kib * 1024)
        })
}

/// Free bytes for unprivileged users on the filesystem of `path`, or of its nearest existing
/// ancestor when it does not exist yet.
fn free_bytes(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|ancestor| ancestor.exists())?;
    available_kib(&output("df", &["-Pk", existing.to_str()?])?).map(|kib| kib * 1024)
}

/// The Available column of `df -P` output: the number before the Capacity percentage, which
/// also works when the filesystem name or mount point contains spaces.
fn available_kib(df: &str) -> Option<u64> {
    let fields: Vec<&str> = df.lines().last()?.split_whitespace().collect();
    let capacity = fields.iter().rposition(|field| field.ends_with('%'))?;
    fields.get(capacity.checked_sub(1)?)?.parse().ok()
}

/// The first GPU `nvidia-smi -L` lists ("GPU 0: NAME (UUID: …)").
fn nvidia_gpu() -> Option<Gpu> {
    if cfg!(target_os = "macos") {
        return None;
    }
    let listing = output("nvidia-smi", &["-L"])?;
    let line = listing.lines().next()?;
    let name = line.split_once(": ").map_or(line, |(_, name)| name);
    let name = name.split(" (UUID").next().unwrap_or(name).trim();
    Some(Gpu {
        vendor: "nvidia",
        name: name.to_owned(),
    })
}

/// An executable named `program` (a path, or a name looked up on PATH).
fn find_program(program: &str) -> Option<PathBuf> {
    if program.contains(std::path::MAIN_SEPARATOR) || program.contains('/') {
        let path = PathBuf::from(program);
        return crate::executable(&path).then_some(path);
    }
    let file = format!("{program}{}", env::consts::EXE_SUFFIX);
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(&file))
        .find(|path| crate::executable(path))
}

/// ffmpeg as the media helper runs it: `VAMP_FFMPEG`, else `ffmpeg` on PATH.
fn ffmpeg() -> Option<PathBuf> {
    match env::var("VAMP_FFMPEG") {
        Ok(program) if !program.is_empty() => find_program(&program),
        _ => find_program("ffmpeg"),
    }
}

fn private_uv(home: &Path) -> PathBuf {
    home.join("tools/bin")
        .join(format!("uv{}", env::consts::EXE_SUFFIX))
}

/// `uv` on PATH, else the copy `install` bootstrapped into `<home>/tools/bin`.
fn find_uv(home: &Path) -> Option<PathBuf> {
    find_program("uv").or_else(|| Some(private_uv(home)).filter(|uv| crate::executable(uv)))
}

/// uv release bootstrapped into `<home>/tools/bin` when none is installed, and the SHA256 of
/// each target's archive (from the release's own `uv-<target>.tar.gz.sha256` files).
#[cfg(feature = "remote")]
const UV_VERSION: &str = "0.13.5";
#[cfg(feature = "remote")]
const UV_ARCHIVES: [(&str, &str); 4] = [
    (
        "aarch64-apple-darwin",
        "743f85035a1996ecb350659e01cd9faea9709829be09b1b4b03c73eaf3c9cd73",
    ),
    (
        "x86_64-apple-darwin",
        "b0f52b0e3945b4107d89024ea38e625683011fbfc05a0d98f9fa2d6dcee28c66",
    ),
    (
        "x86_64-unknown-linux-musl",
        "57ef9d9ccb01fb1bd998df995b0dd221b52f7d7841fbea93f2d4463bf61fb7a9",
    ),
    (
        "aarch64-unknown-linux-musl",
        "7906c79adb21c81abd559841ba76bb1d9af9d791acac7a041b2972e03ae16538",
    ),
];

/// This machine's pinned uv archive: (target, sha256).
#[cfg(feature = "remote")]
fn uv_archive() -> Option<(&'static str, &'static str)> {
    let target = match (env::consts::OS, env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        _ => return None,
    };
    UV_ARCHIVES
        .into_iter()
        .find(|(pinned, _)| *pinned == target)
}

/// `find_uv`, else the pinned uv release bootstrapped into `<home>/tools/bin`.
fn uv(home: &Path) -> Result<PathBuf> {
    match find_uv(home) {
        Some(uv) => Ok(uv),
        None => bootstrap_uv(home),
    }
}

/// Download the pinned uv release into `<home>/tools/bin`, checksum-verified.
#[cfg(feature = "remote")]
fn bootstrap_uv(home: &Path) -> Result<PathBuf> {
    use crate::self_update;
    use sha2::{Digest as _, Sha256};
    let (target, sha256) = uv_archive().with_context(|| {
        format!(
            "no uv build is pinned for {}/{}; install uv and put it on PATH",
            env::consts::OS,
            env::consts::ARCH
        )
    })?;
    let asset = format!("uv-{target}.tar.gz");
    let url = format!("https://github.com/astral-sh/uv/releases/download/{UV_VERSION}/{asset}");
    eprintln!("vamp setup: downloading uv {UV_VERSION}");
    let archive = self_update::get(&self_update::agent(), &url, 64 << 20)?;
    ensure!(
        self_update::hex(&Sha256::digest(&archive)) == sha256,
        "checksum mismatch for {asset}; nothing was installed"
    );
    let mut files = self_update::untar(flate2::read::GzDecoder::new(archive.as_slice()), &["uv"])
        .with_context(|| format!("reading {asset}"))?;
    let bytes = files
        .remove("uv")
        .with_context(|| format!("{asset} has no uv"))?;
    let path = private_uv(home);
    fs::create_dir_all(path.parent().expect("tools/bin has a parent"))?;
    self_update::replace(&path, &bytes)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

#[cfg(not(feature = "remote"))]
fn bootstrap_uv(_home: &Path) -> Result<PathBuf> {
    bail!("cannot find uv; install uv and put it on PATH")
}

/// uv with its cache and its managed Pythons under `<home>/tools`.
fn uv_command(uv: &Path, home: &Path) -> Command {
    let mut command = Command::new(uv);
    command
        .env("UV_CACHE_DIR", home.join("tools/cache/uv"))
        .env("UV_PYTHON_INSTALL_DIR", home.join("tools/python"));
    command
}

/// Run one install step; its output goes to stderr (stdout carries only the result JSON).
fn run(command: &mut Command) -> Result<()> {
    let argv = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("cannot run {argv}"))?;
    ensure!(status.success(), "setup step failed ({status}): {argv}");
    Ok(())
}

/// What a person chose per extra, `<home>/setup.json`.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Choices {
    version: u32,
    #[serde(default)]
    extras: BTreeMap<String, Choice>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Choice {
    /// `installed` or `declined`.
    state: String,
    /// RFC 3339 time of the choice.
    at: String,
    /// Measured by `install`: one cold run of each of the extra's processors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seconds_per_item: Option<f64>,
}
impl Choices {
    fn load(home: &Path) -> Result<Self> {
        let path = home.join(CHOICES_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    version: 1,
                    extras: BTreeMap::new(),
                });
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        let choices: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid {}", path.display()))?;
        ensure!(
            choices.version == 1,
            "{} has version {}; this vamp reads version 1",
            path.display(),
            choices.version
        );
        Ok(choices)
    }
    /// Written to a temporary file and renamed into place.
    fn save(&self, home: &Path) -> Result<()> {
        let path = home.join(CHOICES_FILE);
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        fs::create_dir_all(home)?;
        let temp = home.join(format!(".{CHOICES_FILE}.{}.tmp", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut file = fs::File::create(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temp, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.with_context(|| format!("cannot write {}", path.display()))
    }
    fn get(&self, extra: Extra) -> Option<&Choice> {
        self.extras.get(extra.name())
    }
    fn record(&mut self, extra: Extra, state: &str, seconds_per_item: Option<f64>) {
        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        self.extras.insert(
            extra.name().into(),
            Choice {
                state: state.into(),
                at,
                seconds_per_item,
            },
        );
    }
}

/// An installable extra on this machine.
struct Status {
    extra: Extra,
    /// `installed`, `available` or `declined`.
    state: &'static str,
    recommended: bool,
    why: String,
}

/// Whether the hardware rules recommend `extra` here, and why.
fn rule(extra: Extra, machine: &Machine) -> (bool, String) {
    if extra == Extra::Captions && !machine.nvidia() {
        return (false, "best with an NVIDIA GPU".into());
    }
    if let Some(memory) = machine.memory_bytes.filter(|&memory| memory < MIN_MEMORY) {
        return (
            false,
            format!("needs 8 GB of memory; this machine has {}", gib(memory)),
        );
    }
    if let Some(free) = machine.free_bytes.filter(|&free| free < MIN_FREE) {
        return (false, format!("needs 5 GB of free disk; {} free", gb(free)));
    }
    let why = match extra {
        Extra::Search => "enough memory and free disk",
        Extra::Captions => "NVIDIA GPU found",
    };
    (true, why.into())
}

fn assess(machine: &Machine, models: &Models, choices: &Choices) -> Vec<Status> {
    Extra::ALL
        .into_iter()
        .map(|extra| {
            let state = if models.installed(extra) {
                "installed"
            } else if choices
                .get(extra)
                .is_some_and(|choice| choice.state == "declined")
            {
                "declined"
            } else {
                "available"
            };
            let (fits, why) = rule(extra, machine);
            Status {
                extra,
                state,
                recommended: fits && state == "available",
                why,
            }
        })
        .collect()
}

fn recommended(statuses: &[Status]) -> Vec<Extra> {
    statuses
        .iter()
        .filter(|status| status.recommended)
        .map(|status| status.extra)
        .collect()
}

/// Seconds per item before a measurement, by device class.
fn estimate(extra: Extra, device: &str, machine: &Machine) -> f64 {
    let (search, captions) = if torch_backend(device, machine.os, machine.nvidia()) == Some("cu128")
    {
        SECONDS_CUDA
    } else if machine.os == "macos" && machine.arch == "aarch64" {
        SECONDS_APPLE
    } else {
        SECONDS_CPU
    };
    match extra {
        Extra::Search => search,
        Extra::Captions => captions,
    }
}

/// (download, disk) bytes `install` would add for `extra`; the runtime counts while it is
/// missing.
fn footprint(extra: Extra, device: &str, machine: &Machine, runtime_present: bool) -> (u64, u64) {
    let runtime = match torch_backend(device, machine.os, machine.nvidia()) {
        _ if runtime_present => (0, 0),
        Some("cu128") => RUNTIME_CUDA,
        Some(_) => RUNTIME_CPU,
        None => RUNTIME_MACOS,
    };
    (runtime.0 + extra.weights(), runtime.1 + extra.weights())
}

/// The active (or --context) library: (context name, library).
fn active_library(home: &Path, context: Option<&str>) -> Result<(String, Library)> {
    let name = match context {
        Some(name) => name.to_owned(),
        None => vamp_core::load_config(home)?
            .current
            .context("no active context; run vamp init")?,
    };
    let library = Library::open(home, Some(&name))?;
    Ok((name, library))
}

pub fn plan(home: &Path, context: Option<&str>) -> Result<Value> {
    let home = absolute(home)?;
    let machine = Machine::detect(&home);
    plan_for(&home, context, &machine)
}

pub fn plan_text(home: &Path, context: Option<&str>) -> Result<String> {
    Ok(render(&plan(home, context)?))
}

fn plan_for(home: &Path, context: Option<&str>, machine: &Machine) -> Result<Value> {
    let models = Models::new(home);
    let choices = Choices::load(home)?;
    let device = models
        .recorded_device()
        .unwrap_or_else(|| default_device(machine.nvidia()).into());
    let library = active_library(home, context)
        .ok()
        .and_then(|(name, library)| {
            let items = library.sidebar().ok()?["all"].as_u64()?;
            Some(json!({"context":name, "library_id":library.context.library_id, "items":items}))
        });
    let items = library
        .as_ref()
        .and_then(|library| library["items"].as_u64());
    let vamp_media = crate::resolve_helper().ok();
    let ffmpeg = ffmpeg();
    let video_hint = if machine.os == "macos" {
        "brew install ffmpeg"
    } else {
        "install ffmpeg with your package manager, or set VAMP_FFMPEG"
    };
    let mut extras = vec![
        json!({"name":"previews", "title":"Previews", "enables":"image thumbnails, previews and colors",
            "installable":false, "state":if vamp_media.is_some() {"ready"} else {"missing"}, "recommended":false,
            "why":if vamp_media.is_some() {"built in"} else {"vamp-media not found"},
            "hint":vamp_media.is_none().then_some("put vamp-media beside vamp or on PATH, or set VAMP_MEDIA_HELPER"),
            "download_bytes":0, "disk_bytes":0, "seconds_per_item":null, "seconds_measured":false,
            "library_seconds":null}),
        json!({"name":"video", "title":"Video", "enables":"video thumbnails and 6-second loops",
            "installable":false, "state":if ffmpeg.is_some() {"ready"} else {"missing"}, "recommended":false,
            "why":if ffmpeg.is_some() {"ffmpeg found"} else {"needs ffmpeg"},
            "hint":ffmpeg.is_none().then_some(video_hint),
            "download_bytes":null, "disk_bytes":null, "seconds_per_item":null, "seconds_measured":false,
            "library_seconds":null}),
    ];
    let statuses = assess(machine, &models, &choices);
    for status in &statuses {
        let extra = status.extra;
        let measured = choices
            .get(extra)
            .and_then(|choice| choice.seconds_per_item);
        let seconds = measured.unwrap_or_else(|| estimate(extra, &device, machine));
        let (download, disk) = if status.state == "installed" {
            (0, 0)
        } else {
            footprint(extra, &device, machine, models.python.is_file())
        };
        extras.push(json!({"name":extra.name(), "title":extra.title(), "enables":extra.enables(),
            "installable":true, "state":status.state, "recommended":status.recommended, "why":status.why,
            "hint":(status.state != "installed").then(|| format!("vamp setup install {}", extra.name())),
            "download_bytes":download, "disk_bytes":disk, "seconds_per_item":(seconds * 10.0).round() / 10.0,
            "seconds_measured":measured.is_some(),
            "library_seconds":items.map(|items| (items as f64 * seconds).round() as u64)}));
    }
    let names = |keep: &dyn Fn(&Status) -> bool| -> Vec<&str> {
        statuses
            .iter()
            .filter(|status| keep(status))
            .map(|status| status.extra.name())
            .collect()
    };
    Ok(json!({
        "home":home, "choices_file":home.join(CHOICES_FILE), "machine":machine.json(),
        "tools":{"uv":find_uv(home), "ffmpeg":ffmpeg, "vamp_media":vamp_media},
        "device":device, "library":library, "extras":extras,
        "recommended":names(&|status| status.recommended),
        "offered":names(&|status| status.state != "installed"),
    }))
}

/// GiB as people say it ("16 GB").
fn gib(bytes: u64) -> String {
    let gib = bytes as f64 / f64::from(1u32 << 30);
    if gib < 10.0 {
        format!("{:.1} GB", gib)
    } else {
        format!("{gib:.0} GB")
    }
}

/// Decimal units, as disks and downloads are counted.
fn gb(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 10.0 {
        format!("{gb:.0} GB")
    } else if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

fn duration(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min", seconds.div_ceil(60)),
        _ => {
            let minutes = seconds.div_ceil(60);
            format!("{} h {} min", minutes / 60, minutes % 60)
        }
    }
}

/// The plan for people: machine facts, then one line per extra (state, size, time, reason)
/// and the command that installs what is recommended.
fn render(plan: &Value) -> String {
    let machine = &plan["machine"];
    let os = match machine["os"].as_str() {
        Some("macos") => "macOS",
        Some("linux") => "Linux",
        Some("windows") => "Windows",
        other => other.unwrap_or("unknown OS"),
    };
    let mut facts = vec![format!("{os} {}", machine["arch"].as_str().unwrap_or(""))];
    facts.extend(machine["cpu"].as_str().map(str::to_owned));
    facts.extend(
        machine["cores"]
            .as_u64()
            .map(|cores| format!("{cores} cores")),
    );
    facts.extend(
        machine["memory_bytes"]
            .as_u64()
            .map(|memory| format!("{} memory", gib(memory))),
    );
    facts.extend(
        machine["free_bytes"]
            .as_u64()
            .map(|free| format!("{} free", gb(free))),
    );
    facts.extend(
        machine["gpu"]["name"]
            .as_str()
            .filter(|gpu| machine["cpu"].as_str() != Some(gpu))
            .map(str::to_owned),
    );
    let mut out = format!("This machine: {}\n", facts.join(", "));
    let items = plan["library"]["items"].as_u64();
    match (plan["library"]["context"].as_str(), items) {
        (Some(context), Some(items)) => {
            let _ = writeln!(out, "Library {context}: {items} items");
        }
        _ => out.push_str("No library yet. Create one with vamp init --name NAME.\n"),
    }
    out.push('\n');
    for extra in plan["extras"].as_array().into_iter().flatten() {
        let state = extra["state"].as_str().unwrap_or("");
        let mut parts = vec![extra["enables"].as_str().unwrap_or("").to_owned()];
        if extra["installable"] == true
            && state != "installed"
            && let (Some(download), Some(disk)) = (
                extra["download_bytes"].as_u64(),
                extra["disk_bytes"].as_u64(),
            )
        {
            parts.push(format!("{} download, {} on disk", gb(download), gb(disk)));
        }
        if let Some(seconds) = extra["seconds_per_item"].as_f64() {
            let estimate = if extra["seconds_measured"] == true {
                ""
            } else {
                " (estimate)"
            };
            let mut time = format!("{seconds} s per item{estimate}");
            if let (Some(total), Some(items)) = (extra["library_seconds"].as_u64(), items) {
                let _ = write!(time, ", about {} for {items} items", duration(total));
            }
            parts.push(time);
        }
        if extra["recommended"] == true {
            parts.push(format!(
                "recommended: {}",
                extra["why"].as_str().unwrap_or("")
            ));
        } else if !matches!(state, "installed" | "ready") {
            parts.extend(extra["why"].as_str().map(str::to_owned));
        }
        if extra["installable"] == false {
            parts.extend(extra["hint"].as_str().map(str::to_owned));
        }
        let _ = writeln!(
            out,
            "  {:<9} {:<10} {}",
            extra["name"].as_str().unwrap_or(""),
            state,
            parts.join(" · ")
        );
    }
    out.push('\n');
    let list = |key: &str| -> Vec<&str> {
        plan[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect()
    };
    let (recommended, offered) = (list("recommended"), list("offered"));
    if !recommended.is_empty() {
        out.push_str("Install the recommended extras: vamp setup install recommended\n");
    } else if !offered.is_empty() {
        let _ = writeln!(
            out,
            "Nothing else is recommended for this machine. To install anyway: vamp setup install {}",
            offered.join("|")
        );
    } else {
        out.push_str("Every extra is installed.\n");
    }
    out
}

/// A synthetic photo-like JPEG (a skyline by the sea at sunset) for timing one cold run.
fn write_sample(path: &Path) -> Result<()> {
    let (width, height) = (960u32, 640u32);
    let mix = |a: f32, b: f32, t: f32| (a + (b - a) * t).clamp(0.0, 255.0) as u8;
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        let (fx, fy) = (x as f32 / width as f32, y as f32 / height as f32);
        let horizon = 0.62;
        if ((fx - 0.68) * 1.5).hypot(fy - 0.4) < 0.09 {
            return image::Rgb([255, 214, 120]);
        }
        let block = (fx * 24.0) as u32;
        let roof = horizon - 0.08 - 0.18 * ((block * 37 % 11) as f32 / 10.0);
        if fx < 0.45 && fy >= roof && fy < horizon {
            let lit = (x / 6) % 3 == 1 && (y / 8) % 2 == 1 && !(block + y / 16).is_multiple_of(3);
            return if lit {
                image::Rgb([250, 220, 140])
            } else {
                image::Rgb([40, 42, 54])
            };
        }
        if fy < horizon {
            let t = fy / horizon;
            image::Rgb([
                mix(30.0, 250.0, t),
                mix(60.0, 150.0, t),
                mix(140.0, 90.0, t),
            ])
        } else {
            let t = (fy - horizon) / (1.0 - horizon);
            let wave = ((fx * 60.0 + fy * 200.0).sin() * 0.5 + 0.5) * 30.0;
            image::Rgb([
                mix(40.0, 10.0, t).saturating_add((wave * 0.3) as u8),
                mix(70.0, 30.0, t).saturating_add((wave * 0.5) as u8),
                mix(120.0, 70.0, t).saturating_add(wave as u8),
            ])
        }
    });
    fs::create_dir_all(path.parent().context("sample path has no parent")?)?;
    image
        .save_with_format(path, image::ImageFormat::Jpeg)
        .with_context(|| format!("cannot write {}", path.display()))
}

/// Seconds of one cold run of each processor on `sample`, exactly as the local drain runs a
/// job: the registered command, a `process` ToolRequest on stdin, the processor's timeout.
fn bench(specs: &[ProcessorSpec], sample: &Path) -> Result<Vec<(String, f64)>> {
    let request = ToolRequest {
        operation: "process".into(),
        input_path: Some(path_string(sample)?),
        ..ToolRequest::default()
    };
    specs
        .iter()
        .map(|spec| {
            eprintln!("vamp setup: timing {} on a sample image", spec.name);
            let start = Instant::now();
            let response = vamp_core::run_tool(&spec.command, &request, spec.timeout_seconds)
                .with_context(|| format!("test run of {} failed", spec.name))?;
            ensure!(
                response.observation.is_some(),
                "test run of {} returned no observation",
                spec.name
            );
            Ok((spec.name.clone(), start.elapsed().as_secs_f64()))
        })
        .collect()
}

/// Register `extras`' processors (and the SigLIP text embedder for search) in `library`,
/// keeping each existing registration's placement. Only newly registered processors queue
/// the library's existing items (those without a completed job). Returns (registered, backfill).
fn register(
    library: &mut Library,
    models: &Models,
    extras: &[Extra],
    device: &str,
) -> Result<(Value, Value)> {
    let existing: BTreeMap<String, ProcessorSpec> = library
        .processors()?
        .into_iter()
        .map(|spec| (spec.name.clone(), spec))
        .collect();
    let mut processors = Vec::new();
    let mut backfill = Map::new();
    let mut embedder = Value::Null;
    for &extra in extras {
        for mut spec in models.specs(extra, device)? {
            let previous = existing.get(&spec.name);
            if let Some(previous) = previous {
                spec.placement = previous.placement;
            }
            library.processor_add(spec.clone())?;
            processors.push(
                json!({"name":spec.name, "placement":spec.placement, "new":previous.is_none()}),
            );
            if previous.is_none() {
                backfill.insert(spec.name.clone(), library.enqueue_all(&spec.name, true)?);
            }
        }
        if extra == Extra::Search {
            library.embedder_set(models.embedder()?)?;
            embedder = json!(SIGLIP_MODEL);
        }
    }
    Ok((
        json!({"library_id":library.context.library_id, "processors":processors, "embedder":embedder}),
        Value::Object(backfill),
    ))
}

pub fn install(
    home: &Path,
    context: Option<&str>,
    names: &[String],
    device: Option<&str>,
) -> Result<Value> {
    let home = absolute(home)?;
    let machine = Machine::detect(&home);
    let models = Models::new(&home);
    let mut choices = Choices::load(&home)?;
    let extras = select(names, || recommended(&assess(&machine, &models, &choices)))?;
    let recorded = models.recorded_device();
    let device = match device {
        Some(device) => device.to_owned(),
        None => recorded
            .clone()
            .unwrap_or_else(|| default_device(machine.nvidia()).into()),
    };
    ensure!(
        DEVICES.contains(&device.as_str()),
        "--device must be auto, cpu, cuda or mps"
    );
    if extras.is_empty() {
        return Ok(
            json!({"installed":[], "device":device, "registered":null, "backfill":{},
            "bench":{}, "choices":choices}),
        );
    }
    let extras = with_moved(extras, &models, recorded.as_deref(), &device);
    let backend = torch_backend(&device, machine.os, machine.nvidia());
    let uv = uv(&home)?;
    let names: Vec<&str> = extras.iter().map(|extra| extra.name()).collect();
    eprintln!(
        "vamp setup: installing {} into {} (device {device})",
        names.join(" and "),
        models.root.display()
    );
    if !models.python.is_file() {
        run(uv_command(&uv, &home)
            .args([
                "venv",
                "--managed-python",
                "--allow-existing",
                "--python",
                "3.11",
            ])
            .arg(&models.root))?;
    }
    let pyproject = models.root.join("pyproject.toml");
    fs::write(&pyproject, PYPROJECT).context("cannot write model requirements")?;
    let mut packages = uv_command(&uv, &home);
    packages
        .args(["pip", "install", "--python"])
        .arg(&models.python);
    if let Some(backend) = backend {
        packages.args(["--torch-backend", backend]);
    }
    // Another PyTorch build must replace the installed one: the `==` pins accept any local
    // version (2.8.0+cpu satisfies torch==2.8.0).
    if recorded
        .is_some_and(|recorded| torch_backend(&recorded, machine.os, machine.nvidia()) != backend)
    {
        packages.args([
            "--reinstall-package",
            "torch",
            "--reinstall-package",
            "torchvision",
        ]);
    }
    run(packages.arg("-r").arg(&pyproject))?;
    fs::write(models.root.join("device"), &device).context("cannot record model device")?;
    // Extras installed earlier run on the same packages: give them this vamp's scripts too.
    models.refresh_scripts()?;
    for &extra in &extras {
        let script = models.script(extra);
        fs::write(&script, extra.script().1)
            .with_context(|| format!("cannot write {}", script.display()))?;
        let (model, revision) = extra.model();
        if !manifest_present(&models.manifest(extra), model, revision) {
            // Weights go to <home>/tools/cache/hf; inference loads them only from the path the
            // manifest records, so processors need no environment.
            let argv = models.command(extra, &device, &["--prepare"])?;
            run(Command::new(&argv[0])
                .args(&argv[1..])
                .env("HF_HOME", home.join("tools/cache/hf"))
                .env_remove("HF_HUB_CACHE")
                .env_remove("HUGGINGFACE_HUB_CACHE")
                .env_remove("TRANSFORMERS_CACHE"))?;
        }
    }
    let sample = home.join("tools/cache/sample.jpg");
    write_sample(&sample)?;
    let mut timings = Map::new();
    let timed = extras.iter().try_for_each(|&extra| -> Result<()> {
        let runs = bench(&models.specs(extra, &device)?, &sample)?;
        let seconds = (runs.iter().map(|(_, seconds)| seconds).sum::<f64>() * 10.0).round() / 10.0;
        choices.record(extra, "installed", Some(seconds));
        let processors: Map<String, Value> = runs
            .into_iter()
            .map(|(name, seconds)| (name, json!((seconds * 10.0).round() / 10.0)))
            .collect();
        timings.insert(
            extra.name().into(),
            json!({"seconds_per_item":seconds, "processors":processors}),
        );
        Ok(())
    });
    let _ = fs::remove_file(&sample);
    timed?;
    choices.save(&home)?;
    let (registered, backfill) = match active_library(&home, context) {
        Ok((name, library)) if library.context.remote.is_some() => (
            json!({"skipped":format!("context {name} is a replica; install extras on its authority")}),
            json!({}),
        ),
        Ok((_, mut library)) => register(&mut library, &models, &extras, &device)?,
        Err(error) => (json!({"skipped":format!("{error:#}")}), json!({})),
    };
    Ok(
        json!({"installed":names, "device":device, "registered":registered, "backfill":backfill,
        "bench":timings, "choices":choices}),
    )
}

pub fn decline(home: &Path, names: &[String]) -> Result<Value> {
    let home = absolute(home)?;
    let mut choices = Choices::load(&home)?;
    let extras = select(names, || {
        let machine = Machine::detect(&home);
        recommended(&assess(&machine, &Models::new(&home), &choices))
    })?;
    for &extra in &extras {
        choices.record(extra, "declined", None);
    }
    if !extras.is_empty() {
        choices.save(&home)?;
    }
    Ok(
        json!({"declined":extras.iter().map(|extra| extra.name()).collect::<Vec<_>>(),
        "choices":choices}),
    )
}

/// Extras `vamp init` registers in a new library: recorded as installed in setup.json and
/// present on disk. Read before the library exists, so a broken setup.json stops init early.
pub fn installed_extras(home: &Path) -> Result<Vec<Extra>> {
    let home = absolute(home)?;
    let models = Models::new(&home);
    let choices = Choices::load(&home)?;
    Ok(Extra::ALL
        .into_iter()
        .filter(|&extra| {
            choices
                .get(extra)
                .is_some_and(|choice| choice.state == "installed")
                && models.installed(extra)
        })
        .collect())
}

/// Register `extras` (from [`installed_extras`]) in a library `vamp init` just created.
pub fn register_installed(
    home: &Path,
    library: &mut Library,
    extras: &[Extra],
) -> Result<Vec<&'static str>> {
    if extras.is_empty() {
        return Ok(Vec::new());
    }
    let models = Models::new(&absolute(home)?);
    register(library, &models, extras, &models.device())?;
    Ok(extras.iter().map(|extra| extra.name()).collect())
}

/// This machine's installed extras' processors as `vamp worker` runs them: (spec, servable).
/// Scripts written by an earlier vamp are replaced with this vamp's first.
#[cfg(feature = "remote")]
pub fn model_processors(home: &Path) -> Result<Vec<(ProcessorSpec, bool)>> {
    let models = Models::new(&absolute(home)?);
    models.refresh_scripts()?;
    let device = models.device();
    let mut processors = Vec::new();
    for extra in Extra::ALL
        .into_iter()
        .filter(|&extra| models.installed(extra))
    {
        processors.extend(
            models
                .specs(extra, &device)?
                .into_iter()
                .map(|spec| (spec, true)),
        );
    }
    ensure!(
        !processors.is_empty(),
        "no model processors are installed here; run vamp setup install search (or captions)"
    );
    Ok(processors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vamp_protocol::{Metadata, Placement};

    const GIB: f64 = (1u64 << 30) as f64;

    fn machine(
        memory_gib: Option<f64>,
        free_gb: Option<f64>,
        gpu: Option<(&'static str, &str)>,
        os: &'static str,
    ) -> Machine {
        Machine {
            os,
            arch: if os == "macos" { "aarch64" } else { "x86_64" },
            cpu: Some("Test CPU".into()),
            cores: Some(8),
            memory_bytes: memory_gib.map(|gib| (gib * GIB) as u64),
            free_bytes: free_gb.map(|gb| (gb * 1e9) as u64),
            gpu: gpu.map(|(vendor, name)| Gpu {
                vendor,
                name: name.into(),
            }),
        }
    }
    fn nvidia() -> Machine {
        machine(
            Some(62.0),
            Some(270.0),
            Some(("nvidia", "NVIDIA GeForce RTX 4090")),
            "linux",
        )
    }
    fn sorted_keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    /// An extra as `install` leaves it on disk: venv python, `script`, verified weights.
    fn fake_install(home: &Path, extra: Extra, script: &str) {
        let models = Models::new(home);
        fs::create_dir_all(models.python.parent().unwrap()).unwrap();
        fs::write(&models.python, "").unwrap();
        let weights = home.join("weights").join(extra.name());
        fs::create_dir_all(&weights).unwrap();
        fs::write(weights.join("config.json"), "{}").unwrap();
        fs::write(weights.join("model.safetensors"), "").unwrap();
        let (model, revision) = extra.model();
        fs::write(
            models.manifest(extra),
            json!({"model":model, "revision":revision, "path":weights}).to_string(),
        )
        .unwrap();
        fs::write(models.script(extra), script).unwrap();
    }

    fn add_items(library: &mut Library, dir: &Path, count: usize) {
        let originals = dir.join("originals");
        fs::create_dir_all(&originals).unwrap();
        for index in 0..count {
            let path = originals.join(format!("item-{index}.txt"));
            fs::write(&path, format!("item {index}")).unwrap();
            library.add(&path, None, Metadata::default()).unwrap();
        }
    }

    #[test]
    fn hardware_rules_recommend_extras() {
        let gpu = nvidia();
        assert_eq!(
            rule(Extra::Search, &gpu),
            (true, "enough memory and free disk".into())
        );
        assert_eq!(
            rule(Extra::Captions, &gpu),
            (true, "NVIDIA GPU found".into())
        );
        let mac = machine(
            Some(16.0),
            Some(120.0),
            Some(("apple", "Apple M4")),
            "macos",
        );
        assert!(rule(Extra::Search, &mac).0);
        assert_eq!(
            rule(Extra::Captions, &mac),
            (false, "best with an NVIDIA GPU".into())
        );
        let cpu = machine(Some(16.0), Some(120.0), None, "linux");
        assert!(rule(Extra::Search, &cpu).0);
        assert_eq!(
            rule(Extra::Captions, &cpu),
            (false, "best with an NVIDIA GPU".into())
        );
        // An "8 GB" machine reports a little less than 8 GiB.
        assert!(
            rule(
                Extra::Search,
                &machine(Some(7.6), Some(120.0), None, "linux")
            )
            .0
        );
        let small = machine(Some(4.0), Some(120.0), Some(("nvidia", "GPU")), "linux");
        for extra in Extra::ALL {
            let (fits, why) = rule(extra, &small);
            assert!(!fits);
            assert_eq!(why, "needs 8 GB of memory; this machine has 4.0 GB");
        }
        let full = machine(Some(32.0), Some(2.5), Some(("nvidia", "GPU")), "linux");
        for extra in Extra::ALL {
            assert_eq!(
                rule(extra, &full),
                (false, "needs 5 GB of free disk; 2.5 GB free".into())
            );
        }
        // Unknown memory and disk count as enough.
        assert!(rule(Extra::Search, &machine(None, None, None, "linux")).0);
    }

    #[test]
    fn torch_backend_follows_device_and_platform() {
        assert_eq!(torch_backend("cuda", "linux", true), Some("cu128"));
        assert_eq!(torch_backend("cuda", "linux", false), Some("cu128"));
        assert_eq!(torch_backend("auto", "linux", true), Some("cu128"));
        assert_eq!(torch_backend("auto", "linux", false), Some("cpu"));
        assert_eq!(torch_backend("cpu", "linux", true), Some("cpu"));
        assert_eq!(torch_backend("mps", "linux", false), Some("cpu"));
        assert_eq!(torch_backend("cpu", "windows", false), Some("cpu"));
        for device in ["auto", "cpu", "mps"] {
            assert_eq!(torch_backend(device, "macos", false), None, "{device}");
        }
        assert_eq!(default_device(true), "cuda");
        assert_eq!(default_device(false), "cpu");
    }

    #[test]
    fn extras_are_selected_in_order_once() {
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            select(&names(&["captions", "search", "search"]), || unreachable!()).unwrap(),
            [Extra::Search, Extra::Captions]
        );
        assert_eq!(
            select(&names(&["captions", "recommended"]), || vec![Extra::Search]).unwrap(),
            [Extra::Search, Extra::Captions]
        );
        assert!(
            select(&names(&["recommended"]), Vec::new)
                .unwrap()
                .is_empty()
        );
        assert!(select(&names(&["models"]), Vec::new).is_err());
    }

    #[test]
    fn another_device_moves_the_installed_extras() {
        let dir = tempfile::tempdir().unwrap();
        let models = Models::new(dir.path());
        assert_eq!(
            with_moved(vec![Extra::Captions], &models, None, "cuda"),
            [Extra::Captions]
        );
        fake_install(dir.path(), Extra::Search, "# earlier\n");
        // Same device: only what was asked for.
        assert_eq!(
            with_moved(vec![Extra::Captions], &models, Some("cuda"), "cuda"),
            [Extra::Captions]
        );
        // Search shares the PyTorch build that changes: it moves to cpu too.
        assert_eq!(
            with_moved(vec![Extra::Captions], &models, Some("cuda"), "cpu"),
            [Extra::Search, Extra::Captions]
        );
    }

    #[test]
    fn registration_keeps_placement_and_backfills_only_new_processors() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let mut library = Library::init(&home, "test", None, None).unwrap();
        add_items(&mut library, dir.path(), 2);
        // An earlier `vamp setup models --install`: registered without queuing existing items,
        // then moved to a GPU worker.
        library
            .processor_add(ProcessorSpec {
                name: "siglip-embedding".into(),
                events: vec!["representation_ready".into()],
                command: vec!["/earlier/python".into()],
                input: "preview".into(),
                output: "observation".into(),
                timeout_seconds: 300,
                placement: Placement::Remote,
            })
            .unwrap();
        let models = Models::new(&home);
        let (registered, backfill) = register(
            &mut library,
            &models,
            &[Extra::Search, Extra::Captions],
            "cpu",
        )
        .unwrap();
        let specs: BTreeMap<String, ProcessorSpec> = library
            .processors()
            .unwrap()
            .into_iter()
            .map(|spec| (spec.name.clone(), spec))
            .collect();
        assert_eq!(specs["siglip-embedding"].placement, Placement::Remote);
        assert_eq!(
            specs["siglip-embedding"].command,
            models.command(Extra::Search, "cpu", &[]).unwrap()
        );
        assert_eq!(specs["florence-description"].placement, Placement::Any);
        assert_eq!(
            specs["florence-tags"].command,
            models
                .command(Extra::Captions, "cpu", &["--task", "tags"])
                .unwrap()
        );
        assert_eq!(
            registered["processors"],
            json!([{"name":"siglip-embedding", "placement":"remote", "new":false},
                {"name":"florence-description", "placement":"any", "new":true},
                {"name":"florence-tags", "placement":"any", "new":true}])
        );
        assert_eq!(registered["embedder"], SIGLIP_MODEL);
        assert_eq!(
            sorted_keys(&backfill),
            ["florence-description", "florence-tags"]
        );
        assert_eq!(backfill["florence-tags"]["enqueued"], 2);
        let pending = library.jobs(Some("pending"), 100).unwrap();
        assert_eq!(pending.len(), 4);
        assert!(
            pending
                .iter()
                .all(|job| job.processor.name != "siglip-embedding")
        );
        let embedder = library.embedder().unwrap().unwrap();
        assert_eq!(
            embedder.command,
            models.command(Extra::Search, "auto", &[]).unwrap()
        );
        assert!(embedder.serve);
        // Registering again queues nothing: every processor is registered now.
        let (_, again) = register(
            &mut library,
            &models,
            &[Extra::Search, Extra::Captions],
            "cpu",
        )
        .unwrap();
        assert_eq!(again, json!({}));
        assert_eq!(library.jobs(Some("pending"), 100).unwrap().len(), 4);
    }

    #[cfg(unix)]
    #[test]
    fn bench_times_one_cold_run_like_the_drain() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let sample = dir.path().join("cache/sample.jpg");
        write_sample(&sample).unwrap();
        let decoded = image::open(&sample).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (960, 640));
        let processor = |name: &str, body: &str| {
            let script = dir.path().join(name);
            fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            ProcessorSpec {
                name: name.into(),
                events: vec!["representation_ready".into()],
                command: vec![path_string(&script).unwrap()],
                input: "preview".into(),
                output: "observation".into(),
                timeout_seconds: 10,
                placement: Placement::Any,
            }
        };
        // Keeps the request it reads on stdin, then answers like a SigLIP processor.
        let embed = processor(
            "fake-embedding",
            r#"cat > "$0.request"; printf '{"observation":{"kind":"embedding","model":"fake","value":{}}}'"#,
        );
        let runs = bench(&[embed], &sample).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, "fake-embedding");
        assert!(runs[0].1 > 0.0);
        let request: ToolRequest =
            serde_json::from_slice(&fs::read(dir.path().join("fake-embedding.request")).unwrap())
                .unwrap();
        assert_eq!(request.operation, "process");
        assert_eq!(request.protocol_version, vamp_protocol::PROTOCOL_VERSION);
        assert_eq!(
            request.input_path.as_deref(),
            Some(sample.to_str().unwrap())
        );
        let broken = processor("fake-broken", "cat >/dev/null; echo 'no model' >&2; exit 1");
        let error = format!("{:#}", bench(&[broken], &sample).unwrap_err());
        assert!(
            error.contains("test run of fake-broken failed") && error.contains("no model"),
            "{error}"
        );
        let silent = processor("fake-silent", "cat >/dev/null; printf '{}'");
        let error = format!("{:#}", bench(&[silent], &sample).unwrap_err());
        assert!(error.contains("returned no observation"), "{error}");
    }

    #[test]
    fn choices_round_trip_and_decline() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let mut choices = Choices::load(&home).unwrap();
        assert_eq!(
            choices,
            Choices {
                version: 1,
                extras: BTreeMap::new()
            }
        );
        choices.record(Extra::Search, "installed", Some(3.2));
        choices.save(&home).unwrap();
        let saved: Value =
            serde_json::from_slice(&fs::read(home.join("setup.json")).unwrap()).unwrap();
        assert_eq!(sorted_keys(&saved), ["extras", "version"]);
        assert_eq!(saved["version"], 1);
        assert_eq!(saved["extras"]["search"]["state"], "installed");
        assert_eq!(saved["extras"]["search"]["seconds_per_item"], 3.2);
        let at = saved["extras"]["search"]["at"].as_str().unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(at).is_ok(), "{at}");
        assert_eq!(Choices::load(&home).unwrap(), choices);

        let declined = decline(&home, &["captions".into()]).unwrap();
        assert_eq!(declined["declined"], json!(["captions"]));
        let reloaded = Choices::load(&home).unwrap();
        assert_eq!(reloaded.extras["captions"].state, "declined");
        assert_eq!(reloaded.extras["captions"].seconds_per_item, None);
        assert_eq!(reloaded.extras["search"], choices.extras["search"]);
        assert_eq!(
            declined["choices"],
            serde_json::to_value(&reloaded).unwrap()
        );
        let saved: Value =
            serde_json::from_slice(&fs::read(home.join("setup.json")).unwrap()).unwrap();
        assert_eq!(sorted_keys(&saved["extras"]["captions"]), ["at", "state"]);
        // Written by rename: no temporary file is left beside it.
        let files: Vec<_> = fs::read_dir(&home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(files, ["setup.json"]);

        fs::write(home.join("setup.json"), r#"{"version":2,"extras":{}}"#).unwrap();
        assert!(format!("{:#}", Choices::load(&home).unwrap_err()).contains("version 2"));
    }

    #[test]
    fn plan_reports_state_recommendations_and_speed() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let plan = plan_for(&home, None, &nvidia()).unwrap();
        assert!(!home.exists(), "the plan writes nothing");
        assert_eq!(
            sorted_keys(&plan),
            [
                "choices_file",
                "device",
                "extras",
                "home",
                "library",
                "machine",
                "offered",
                "recommended",
                "tools"
            ]
        );
        assert_eq!(
            sorted_keys(&plan["machine"]),
            [
                "arch",
                "cores",
                "cpu",
                "free_bytes",
                "gpu",
                "memory_bytes",
                "os"
            ]
        );
        assert_eq!(
            plan["machine"]["gpu"],
            json!({"vendor":"nvidia", "name":"NVIDIA GeForce RTX 4090"})
        );
        assert_eq!(sorted_keys(&plan["tools"]), ["ffmpeg", "uv", "vamp_media"]);
        assert_eq!(plan["choices_file"], json!(home.join("setup.json")));
        assert_eq!(plan["device"], "cuda");
        assert_eq!(plan["library"], Value::Null);
        let extras = plan["extras"].as_array().unwrap();
        let names: Vec<&str> = extras
            .iter()
            .map(|extra| extra["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["previews", "video", "search", "captions"]);
        for extra in extras {
            assert_eq!(
                sorted_keys(extra),
                [
                    "disk_bytes",
                    "download_bytes",
                    "enables",
                    "hint",
                    "installable",
                    "library_seconds",
                    "name",
                    "recommended",
                    "seconds_measured",
                    "seconds_per_item",
                    "state",
                    "title",
                    "why"
                ]
            );
        }
        assert_eq!(plan["recommended"], json!(["search", "captions"]));
        assert_eq!(plan["offered"], json!(["search", "captions"]));
        let search = &extras[2];
        assert_eq!(search["state"], "available");
        assert_eq!(search["hint"], "vamp setup install search");
        assert_eq!(search["download_bytes"], RUNTIME_CUDA.0 + SIGLIP_WEIGHTS);
        assert_eq!(search["disk_bytes"], RUNTIME_CUDA.1 + SIGLIP_WEIGHTS);
        assert_eq!(search["seconds_per_item"], SECONDS_CUDA.0);
        assert_eq!(search["seconds_measured"], false);
        assert_eq!(search["library_seconds"], Value::Null);
        let mac = plan_for(
            &home,
            None,
            &machine(
                Some(16.0),
                Some(120.0),
                Some(("apple", "Apple M4")),
                "macos",
            ),
        )
        .unwrap();
        assert_eq!(mac["device"], "cpu");
        assert_eq!(mac["recommended"], json!(["search"]));
        assert_eq!(mac["extras"][3]["why"], "best with an NVIDIA GPU");
        assert_eq!(
            mac["extras"][2]["download_bytes"],
            RUNTIME_MACOS.0 + SIGLIP_WEIGHTS
        );
        assert_eq!(mac["extras"][2]["seconds_per_item"], SECONDS_APPLE.0);

        // Search installed and timed, captions declined, a library of 4 items.
        fake_install(&home, Extra::Search, SIGLIP_SCRIPT);
        fs::write(home.join("tools/models/device"), "cpu").unwrap();
        let mut choices = Choices::load(&home).unwrap();
        choices.record(Extra::Search, "installed", Some(2.5));
        choices.record(Extra::Captions, "declined", None);
        choices.save(&home).unwrap();
        let mut library = Library::init(&home, "refs", None, None).unwrap();
        add_items(&mut library, dir.path(), 4);
        let plan = plan_for(&home, None, &nvidia()).unwrap();
        assert_eq!(plan["device"], "cpu");
        assert_eq!(plan["library"]["context"], "refs");
        assert_eq!(
            plan["library"]["library_id"],
            library.context.library_id.as_str()
        );
        assert_eq!(plan["library"]["items"], 4);
        let (search, captions) = (&plan["extras"][2], &plan["extras"][3]);
        assert_eq!(search["state"], "installed");
        assert_eq!(search["recommended"], false);
        assert_eq!(search["hint"], Value::Null);
        assert_eq!(
            (
                search["download_bytes"].as_u64(),
                search["disk_bytes"].as_u64()
            ),
            (Some(0), Some(0))
        );
        assert_eq!(search["seconds_per_item"], 2.5);
        assert_eq!(search["seconds_measured"], true);
        assert_eq!(search["library_seconds"], 10);
        assert_eq!(captions["state"], "declined");
        assert_eq!(captions["recommended"], false);
        // The runtime is there already: captions adds its weights only.
        assert_eq!(captions["download_bytes"], FLORENCE_WEIGHTS);
        assert_eq!(captions["seconds_per_item"], SECONDS_CPU.1);
        assert_eq!(
            captions["library_seconds"],
            (SECONDS_CPU.1 * 4.0).round() as u64
        );
        assert_eq!(plan["recommended"], json!([]));
        assert_eq!(plan["offered"], json!(["captions"]));
        // A script from an earlier vamp is not this vamp's install.
        fs::write(home.join("tools/models/siglip.py"), "# earlier\n").unwrap();
        assert_eq!(
            plan_for(&home, None, &nvidia()).unwrap()["extras"][2]["state"],
            "available"
        );
    }

    #[test]
    fn text_plan_has_one_line_per_extra() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fake_install(&home, Extra::Search, SIGLIP_SCRIPT);
        fs::write(home.join("tools/models/device"), "cpu").unwrap();
        let mut choices = Choices::load(&home).unwrap();
        choices.record(Extra::Search, "installed", Some(2.5));
        choices.save(&home).unwrap();
        let mut library = Library::init(&home, "refs", None, None).unwrap();
        add_items(&mut library, dir.path(), 4);
        let mac = machine(
            Some(16.0),
            Some(120.0),
            Some(("apple", "Apple M4")),
            "macos",
        );
        let text = render(&plan_for(&home, None, &mac).unwrap());
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("This machine: macOS aarch64, Test CPU, 8 cores, 16 GB memory, 120 GB free, Apple M4"), "{text}");
        assert_eq!(lines[1], "Library refs: 4 items");
        let line = |name: &str| {
            *lines
                .iter()
                .find(|line| line.starts_with(&format!("  {name} ")))
                .unwrap()
        };
        assert!(
            line("search").contains("installed")
                && line("search").contains("2.5 s per item, about 10 s for 4 items"),
            "{text}"
        );
        assert!(!line("search").contains("estimate"));
        let captions = line("captions");
        assert!(
            captions.contains("available")
                && captions.contains("469 MB download")
                && captions.contains("(estimate)"),
            "{text}"
        );
        assert!(captions.ends_with("best with an NVIDIA GPU"), "{text}");
        assert_eq!(
            *lines.last().unwrap(),
            "Nothing else is recommended for this machine. To install anyway: vamp setup install captions"
        );
    }

    #[test]
    fn machine_facts_parse() {
        let df = "Filesystem     1024-blocks      Used Available Capacity Mounted on\n/dev/nvme1n1p5   959786032 630094564 280938032      70% /\n";
        assert_eq!(available_kib(df), Some(280938032));
        let spaced = "Filesystem 1024-blocks Used Available Capacity Mounted on\nmap auto home 0 0 0 100% /System/Volumes/Data/home dir\n";
        assert_eq!(available_kib(spaced), Some(0));
        let machine = Machine::detect(Path::new("/definitely/not/here"));
        assert_eq!(machine.os, env::consts::OS);
        assert!(machine.cores.is_some_and(|cores| cores > 0));
        assert!(
            machine.free_bytes.is_some(),
            "free disk of the nearest existing ancestor"
        );
    }

    #[cfg(feature = "remote")]
    #[test]
    fn uv_archives_are_pinned() {
        for (target, sha256) in UV_ARCHIVES {
            assert!(
                sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "{target}"
            );
        }
        if matches!(env::consts::OS, "linux" | "macos")
            && matches!(env::consts::ARCH, "x86_64" | "aarch64")
        {
            assert!(uv_archive().is_some());
        }
    }

    /// Desktop production before setup.json existed: `vamp setup models --install` left both
    /// extras with scripts of that vamp and `device` cuda. `vamp worker` must keep offering the
    /// same three processors, now running this vamp's scripts.
    #[cfg(feature = "remote")]
    #[test]
    fn pre_cutover_install_keeps_its_worker_processors() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        fake_install(home, Extra::Search, "# siglip of an earlier vamp\n");
        fake_install(home, Extra::Captions, "# florence of an earlier vamp\n");
        fs::write(home.join("tools/models/device"), "cuda").unwrap();
        let mut processors = model_processors(home).unwrap();
        processors.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        let root = home.join("tools/models");
        let argv = |script: &str, manifest: &str, extra: &[&str]| -> Vec<String> {
            let mut argv = vec![
                root.join("bin/python").to_str().unwrap().to_owned(),
                root.join(script).to_str().unwrap().to_owned(),
                "--manifest".into(),
                root.join(manifest).to_str().unwrap().to_owned(),
                "--device".into(),
                "cuda".into(),
            ];
            argv.extend(extra.iter().map(|argument| (*argument).to_owned()));
            argv
        };
        let expected = [
            (
                "florence-description",
                argv("florence.py", "florence.json", &[]),
                600,
            ),
            (
                "florence-tags",
                argv("florence.py", "florence.json", &["--task", "tags"]),
                600,
            ),
            (
                "siglip-embedding",
                argv("siglip.py", "siglip.json", &[]),
                300,
            ),
        ];
        assert_eq!(processors.len(), expected.len());
        for ((spec, servable), (name, command, timeout)) in processors.iter().zip(expected) {
            assert_eq!(spec.name, name);
            assert_eq!(spec.command, command);
            assert_eq!(spec.timeout_seconds, timeout);
            assert_eq!(spec.events, ["representation_ready"]);
            assert_eq!(
                (spec.input.as_str(), spec.output.as_str()),
                ("preview", "observation")
            );
            assert_eq!(spec.placement, Placement::Any);
            assert!(*servable);
        }
        assert_eq!(
            fs::read_to_string(root.join("siglip.py")).unwrap(),
            SIGLIP_SCRIPT
        );
        assert_eq!(
            fs::read_to_string(root.join("florence.py")).unwrap(),
            FLORENCE_SCRIPT
        );
        assert_eq!(fs::read_to_string(root.join("device")).unwrap(), "cuda");
        assert!(!home.join("setup.json").exists());

        let empty = tempfile::tempdir().unwrap();
        let error = model_processors(empty.path()).unwrap_err().to_string();
        assert!(error.contains("vamp setup install search"), "{error}");
    }
}
