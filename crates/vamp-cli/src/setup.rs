use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const PROCESSOR: &str = include_str!("../../../processors/florence/processor.py");
const SIGLIP_PROCESSOR: &str = include_str!("../../../processors/siglip/processor.py");
const MODELS_PYPROJECT: &str = include_str!("../../../processors/pyproject.toml");
const SIGLIP_MODEL: &str = "google/siglip-base-patch16-224";
const SIGLIP_REVISION: &str = "7fd15f0689c79d79e38b1c2e2e2370a7bf2761ed";
const MODEL: &str = "florence-community/Florence-2-base";
const REVISION: &str = "00921df66db728a9ceb750f5eca43e5c203a2051";
const PACKAGES: &[&str] = &[
    "torch==2.8.0",
    "torchvision==0.23.0",
    "transformers==4.57.1",
    "Pillow>=11,<12",
    "huggingface-hub>=0.34,<1",
    "safetensors>=0.5,<0.7",
];

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .context("setup paths must be valid UTF-8")
}

fn execute(argv: &[String]) -> Result<()> {
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| {
            format!(
                "cannot run {}; install it and ensure it is on PATH",
                argv[0]
            )
        })?;
    if !status.success() {
        bail!("setup command failed ({status}): {argv:?}");
    }
    Ok(())
}

fn workspace(source: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(source) = source {
        let source = source
            .canonicalize()
            .context("cannot resolve --source workspace")?;
        if !source.join("crates/vamp-media/Cargo.toml").is_file() {
            bail!("--source must be the VAMP workspace containing crates/vamp-media/Cargo.toml");
        }
        return Ok(Some(source));
    }
    let cwd = env::current_dir()?;
    Ok(cwd
        .ancestors()
        .find(|p| p.join("crates/vamp-media/Cargo.toml").is_file())
        .map(Path::to_path_buf))
}

fn media(home: &Path, install: bool, source: Option<&Path>) -> Result<Value> {
    let root = home.join("tools");
    let executable = root
        .join("bin")
        .join(format!("vamp-media{}", env::consts::EXE_SUFFIX));
    let source = workspace(source)?;
    let argv = source
        .as_ref()
        .map(|source| -> Result<Vec<String>> {
            Ok(vec![
                "cargo".into(),
                "install".into(),
                "--locked".into(),
                "--path".into(),
                path_string(&source.join("crates/vamp-media"))?,
                "--root".into(),
                path_string(&root)?,
            ])
        })
        .transpose()?;
    if install {
        let command = argv.as_ref().context(
            "media installation requires --source /path/to/vamp (or run inside the VAMP workspace)",
        )?;
        let source = source.as_ref().expect("argv requires source");
        if !source.join("Cargo.lock").is_file() {
            bail!(
                "source workspace is missing Cargo.lock; supply a complete locked VAMP source checkout"
            );
        }
        execute(command)?;
        if !executable.is_file() {
            bail!(
                "cargo install succeeded but {} is missing",
                executable.display()
            );
        }
    }
    Ok(
        json!({"capability":"media", "installed":executable.is_file(),
        "check":"file_presence", "executable":executable, "command":[executable],
        "install_argv":argv, "source":source,
        "requirements":["Rust toolchain with cargo", "locked VAMP source workspace"],
        "setup_argv":["vamp","--home",home,"setup","media","--install","--source",
            source.as_deref().unwrap_or(Path::new("/path/to/vamp"))]}),
    )
}

fn florence(home: &Path, install: bool) -> Result<Value> {
    let root = home.join("tools/florence");
    let python = if cfg!(windows) {
        root.join("Scripts/python.exe")
    } else {
        root.join("bin/python")
    };
    let script = root.join("processor.py");
    let manifest = root.join("model.json");
    let command = vec![
        path_string(&python)?,
        path_string(&script)?,
        "--manifest".into(),
        path_string(&manifest)?,
        "--device".into(),
        "cpu".into(),
    ];
    let venv = vec![
        "uv".into(),
        "venv".into(),
        "--python".into(),
        "3.11".into(),
        path_string(&root)?,
    ];
    let mut packages = vec![
        "uv".into(),
        "--cache-dir".into(),
        path_string(&home.join("tools/cache/uv"))?,
        "pip".into(),
        "install".into(),
        "--python".into(),
        path_string(&python)?,
    ];
    if !cfg!(target_os = "macos") {
        packages.extend([
            "--extra-index-url".into(),
            "https://download.pytorch.org/whl/cpu".into(),
        ]);
    }
    packages.extend(PACKAGES.iter().map(|package| {
        if !cfg!(target_os = "macos")
            && (package.starts_with("torch==") || package.starts_with("torchvision=="))
        {
            format!("{package}+cpu")
        } else {
            (*package).to_owned()
        }
    }));
    let mut prepare = command.clone();
    prepare.push("--prepare".into());
    if install {
        if !python.is_file() {
            execute(&venv)?;
        }
        execute(&packages)?;
        fs::write(&script, PROCESSOR).context("cannot write Florence processor")?;
        execute(&prepare)?;
    }
    let model_present = manifest_present(&manifest, MODEL, REVISION);
    let script_present = fs::read_to_string(&script).is_ok_and(|text| text == PROCESSOR);
    let installed = python.is_file() && script_present && model_present;
    let spec = json!({"name":"florence-description", "events":["representation_ready"],
        "command":command, "input":"preview", "output":"observation", "timeout_seconds":600});
    Ok(
        json!({"capability":"florence", "installed":installed, "check":"file_presence",
        "components":{"python":python.is_file(),"processor":script_present,"model":model_present},
        "python":python,"processor":script,"manifest":manifest,"model":MODEL,"revision":REVISION,
        "processor_spec":spec,"command":command,"install_argv":[venv,packages,prepare],
        "setup_argv":["vamp","--home",home,"setup","florence","--install"],
        "requirements":["uv on PATH","Python 3.11 (uv may download it on explicit install)"],
        "notes":["Inspection only checks installed files; it does not import packages, load weights, download, or modify anything.",
            "Explicit installation verifies the native model loads. Inference is strictly offline and uses local preview files.",
            "Uses the official Transformers-compatible converted Florence-2-base checkpoint and reuses its Hugging Face cache.",
            "Legacy microsoft/Florence-2-base cached weights have incompatible parameter names; they are not loaded as native weights or executed as remote Python.",
            "CPU float32 is the default. MPS is explicit via processor --device mps and fails if unavailable or unsupported.",
            "Save processor_spec as JSON and register with vamp processors add --spec PATH; registration is not automatic."]}),
    )
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

/// `cuda` on Linux hosts with a working `nvidia-smi`, else `cpu` (MPS made one-shot
/// Florence slower on Apple silicon, so macOS stays on CPU unless asked).
fn default_device() -> &'static str {
    let nvidia = cfg!(target_os = "linux")
        && Command::new("nvidia-smi")
            .arg("-L")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
    if nvidia { "cuda" } else { "cpu" }
}

/// Florence-2 captions + tags and SigLIP embeddings in one uv environment, registered as the
/// active library's default processors and text embedder on explicit install.
fn models(
    home: &Path,
    context: Option<&str>,
    install: bool,
    device: Option<&str>,
) -> Result<Value> {
    let root = home.join("tools/models");
    let python = if cfg!(windows) {
        root.join("Scripts/python.exe")
    } else {
        root.join("bin/python")
    };
    let florence_script = root.join("florence.py");
    let siglip_script = root.join("siglip.py");
    let pyproject = root.join("pyproject.toml");
    let florence_manifest = root.join("florence.json");
    let siglip_manifest = root.join("siglip.json");
    // Explicit --device wins, then the device recorded at install, then host detection.
    let device_file = root.join("device");
    let device = match device {
        Some(device) => device.to_owned(),
        None => fs::read_to_string(&device_file)
            .ok()
            .map(|d| d.trim().to_owned())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| default_device().to_owned()),
    };
    if !matches!(device.as_str(), "auto" | "cpu" | "cuda" | "mps") {
        bail!("--device must be auto, cpu, cuda or mps");
    }
    let command =
        |script: &Path, manifest: &Path, device: &str, extra: &[&str]| -> Result<Vec<String>> {
            let mut argv = vec![
                path_string(&python)?,
                path_string(script)?,
                "--manifest".into(),
                path_string(manifest)?,
                "--device".into(),
                device.into(),
            ];
            argv.extend(extra.iter().map(|s| (*s).to_owned()));
            Ok(argv)
        };
    let description = command(&florence_script, &florence_manifest, &device, &[])?;
    let tags = command(
        &florence_script,
        &florence_manifest,
        &device,
        &["--task", "tags"],
    )?;
    let embedding = command(&siglip_script, &siglip_manifest, &device, &[])?;
    // `serve`: the command also accepts --serve --socket PATH --idle-seconds N; vamp worker
    // keeps such processors warm. Registration ignores the key.
    let specs = [
        json!({"name":"florence-description","events":["representation_ready"],"command":description,
            "input":"preview","output":"observation","timeout_seconds":600,"serve":true}),
        json!({"name":"florence-tags","events":["representation_ready"],"command":tags,
            "input":"preview","output":"observation","timeout_seconds":600,"serve":true}),
        json!({"name":"siglip-embedding","events":["representation_ready"],"command":embedding,
            "input":"preview","output":"observation","timeout_seconds":300,"serve":true}),
    ];
    // The warm server benefits from an accelerator (~10 ms on MPS vs ~23 ms CPU per query);
    // auto picks CUDA, then MPS, then CPU. Same model as the image embeddings either way.
    let query_embedding = command(&siglip_script, &siglip_manifest, "auto", &[])?;
    let embedder =
        json!({"command":query_embedding,"model":SIGLIP_MODEL,"timeout_seconds":120,"serve":true});
    let venv = vec![
        "uv".into(),
        "venv".into(),
        "--python".into(),
        "3.11".into(),
        path_string(&root)?,
    ];
    let mut packages = vec![
        "uv".into(),
        "--cache-dir".into(),
        path_string(&home.join("tools/cache/uv"))?,
        "pip".into(),
        "install".into(),
        "--python".into(),
        path_string(&python)?,
    ];
    if device == "cuda" {
        // CUDA 12.8 wheels of the pinned torch/torchvision from the PyTorch index.
        packages.extend(["--torch-backend".into(), "cu128".into()]);
    }
    packages.extend(["-r".into(), path_string(&pyproject)?]);
    let mut prepare_florence = description.clone();
    prepare_florence.push("--prepare".into());
    let mut prepare_siglip = embedding.clone();
    prepare_siglip.push("--prepare".into());
    let mut registered = Value::Null;
    if install {
        if !python.is_file() {
            execute(&venv)?;
        }
        fs::write(&pyproject, MODELS_PYPROJECT).context("cannot write model requirements")?;
        execute(&packages)?;
        fs::write(&florence_script, PROCESSOR).context("cannot write Florence processor")?;
        fs::write(&siglip_script, SIGLIP_PROCESSOR).context("cannot write SigLIP processor")?;
        execute(&prepare_florence)?;
        execute(&prepare_siglip)?;
        fs::write(&device_file, &device).context("cannot record model device")?;
        // Registration targets the active (or --context) library; setup without a catalog
        // still installs and reports the specs for later registration.
        registered = match vamp_core::Library::open(home, context) {
            Ok(mut library) => {
                for spec in &specs {
                    library.processor_add(serde_json::from_value(spec.clone())?)?;
                }
                library.embedder_set(serde_json::from_value(embedder.clone())?)?;
                json!({"library_id":library.context.library_id,
                    "processors":specs.iter().map(|s| s["name"].clone()).collect::<Vec<_>>(),
                    "embedder":SIGLIP_MODEL})
            }
            Err(error) => json!({"skipped":format!("{error:#}")}),
        };
    }
    let florence_present = manifest_present(&florence_manifest, MODEL, REVISION);
    let siglip_present = manifest_present(&siglip_manifest, SIGLIP_MODEL, SIGLIP_REVISION);
    let scripts_present = fs::read_to_string(&florence_script).is_ok_and(|t| t == PROCESSOR)
        && fs::read_to_string(&siglip_script).is_ok_and(|t| t == SIGLIP_PROCESSOR);
    let installed = python.is_file() && scripts_present && florence_present && siglip_present;
    Ok(
        json!({"capability":"models", "installed":installed, "check":"file_presence",
        "components":{"python":python.is_file(),"processors":scripts_present,
            "florence":florence_present,"siglip":siglip_present},
        "models":[{"model":MODEL,"revision":REVISION,"role":"description + tags"},
            {"model":SIGLIP_MODEL,"revision":SIGLIP_REVISION,"role":"image/text embeddings (768-d)"}],
        "device":device,
        "processor_specs":specs,"embedder":embedder,"registered":registered,
        "install_argv":[venv,packages,prepare_florence,prepare_siglip],
        "setup_argv":["vamp","--home",home,"setup","models","--install","--device",device],
        "requirements":["uv on PATH","Python 3.11 (uv may download it on explicit install)","~1.1 GB model downloads"],
        "notes":["Install registers florence-description, florence-tags and siglip-embedding processors (event representation_ready, input preview) plus the SigLIP text embedder in the active library (or --context).",
            "Inference is offline and local. The first semantic query starts a persistent SigLIP server (Unix socket, idle exit after 15 min) and runs one-shot; later queries take tens of milliseconds.",
            "--device defaults to cuda on Linux with nvidia-smi (CUDA 12.8 torch wheels), else cpu; the choice is recorded for vamp worker."]}),
    )
}

/// This machine's installed model processors as `vamp worker` runs them: (spec, servable).
#[cfg(feature = "remote")]
pub fn model_processors(home: &Path) -> Result<Vec<(vamp_protocol::ProcessorSpec, bool)>> {
    let report = run(home, None, Some("models"), false, None, None)?;
    if report["installed"] != true {
        bail!(
            "model processors are not installed here; run vamp setup models --install (components: {})",
            report["components"]
        );
    }
    report["processor_specs"]
        .as_array()
        .context("setup report lacks processor_specs")?
        .iter()
        .map(|spec| Ok((serde_json::from_value(spec.clone())?, spec["serve"] == true)))
        .collect()
}

pub fn run(
    home: &Path,
    context: Option<&str>,
    capability: Option<&str>,
    install: bool,
    source: Option<&Path>,
    device: Option<&str>,
) -> Result<Value> {
    let home = if home.is_absolute() {
        home.to_path_buf()
    } else {
        env::current_dir()?.join(home)
    };
    match capability {
        Some("media") => media(&home, install, source),
        Some("florence") => florence(&home, install),
        Some("models") => models(&home, context, install, device),
        Some(other) => {
            bail!("unknown setup capability {other:?}; expected media, florence or models")
        }
        None if install => {
            bail!("choose an explicit capability: vamp setup media|florence|models --install")
        }
        None => Ok(
            json!({"capabilities":[media(&home, false, source)?,florence(&home, false)?,models(&home, context, false, device)?]}),
        ),
    }
}
