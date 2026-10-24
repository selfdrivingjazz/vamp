//! `vamp setup` through the binary on throwaway homes: the plan is one line of JSON (or text
//! for people) and writes nothing, `decline` records the choice, and `vamp init` registers the
//! extras this machine installed.

use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

const SIGLIP_SCRIPT: &str = include_str!("../../../processors/siglip/processor.py");
const FLORENCE_SCRIPT: &str = include_str!("../../../processors/florence/processor.py");

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn vamp(home: &Path, args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_vamp"))
        .arg("--home")
        .arg(home)
        .args(args)
        .env_remove("VAMP_HOME")
        .env_remove("VAMP_CONTEXT")
        .output()
        .unwrap();
    Run {
        code: output.status.code().unwrap(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

fn json(run: &Run) -> Value {
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        run.stdout.lines().count(),
        1,
        "one line of JSON: {}",
        run.stdout
    );
    serde_json::from_str(&run.stdout).unwrap_or_else(|e| panic!("{e}: {}", run.stdout))
}

fn extra<'a>(plan: &'a Value, name: &str) -> &'a Value {
    plan["extras"]
        .as_array()
        .unwrap()
        .iter()
        .find(|extra| extra["name"] == name)
        .unwrap()
}

/// An extra as `vamp setup install` leaves it: venv python, this vamp's script, weights.
fn installed(home: &Path, script: &str, manifest: &str, model: &str, revision: &str, text: &str) {
    let models = home.join("tools/models");
    fs::create_dir_all(models.join("bin")).unwrap();
    fs::write(models.join("bin/python"), "").unwrap();
    let weights = home.join("weights").join(model.replace('/', "--"));
    fs::create_dir_all(&weights).unwrap();
    fs::write(weights.join("config.json"), "{}").unwrap();
    fs::write(weights.join("model.safetensors"), "").unwrap();
    fs::write(
        models.join(manifest),
        json!({"model":model, "revision":revision, "path":weights}).to_string(),
    )
    .unwrap();
    fs::write(models.join(script), text).unwrap();
    fs::write(models.join("device"), "cpu").unwrap();
}

#[test]
fn plan_is_read_only_json_or_text() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let plan = json(&vamp(&home, &["setup"]));
    assert_eq!(
        json(&vamp(&home, &["setup", "plan"]))["extras"],
        plan["extras"]
    );
    let names: Vec<&str> = plan["extras"]
        .as_array()
        .unwrap()
        .iter()
        .map(|extra| extra["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["previews", "video", "search", "captions"]);
    for key in ["recommended", "offered"] {
        for name in plan[key].as_array().unwrap() {
            assert!(
                matches!(name.as_str(), Some("search" | "captions")),
                "{key}: {name}"
            );
        }
    }
    assert_eq!(plan["offered"], json!(["search", "captions"]));
    assert_eq!(plan["library"], Value::Null);
    assert_eq!(plan["home"], json!(home));
    assert!(
        plan["machine"]["cores"]
            .as_u64()
            .is_some_and(|cores| cores > 0)
    );

    let text = vamp(&home, &["setup", "plan", "--text"]);
    assert_eq!(text.code, 0, "{}", text.stderr);
    assert!(text.stdout.starts_with("This machine: "), "{}", text.stdout);
    for name in names {
        assert!(
            text.stdout
                .lines()
                .any(|line| line.starts_with(&format!("  {name} "))),
            "{}",
            text.stdout
        );
    }
    assert!(!home.exists(), "plan wrote into the home");

    let unknown = vamp(&home, &["setup", "install", "models"]);
    assert_eq!(unknown.code, 2, "{}", unknown.stderr);
    assert_eq!(vamp(&home, &["setup", "models", "--install"]).code, 2);
}

#[test]
fn decline_records_the_choice() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let declined = json(&vamp(&home, &["setup", "decline", "captions"]));
    assert_eq!(declined["declined"], json!(["captions"]));
    let saved: Value = serde_json::from_slice(&fs::read(home.join("setup.json")).unwrap()).unwrap();
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["extras"]["captions"]["state"], "declined");
    assert!(saved["extras"]["captions"]["at"].is_string());
    assert_eq!(declined["choices"], saved);
    let plan = json(&vamp(&home, &["setup", "plan"]));
    assert_eq!(extra(&plan, "captions")["state"], "declined");
    assert_eq!(extra(&plan, "captions")["recommended"], false);
    assert!(
        !plan["recommended"]
            .as_array()
            .unwrap()
            .contains(&json!("captions"))
    );
    assert_eq!(plan["offered"], json!(["search", "captions"]));
}

#[test]
fn init_registers_installed_extras() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    installed(
        &home,
        "siglip.py",
        "siglip.json",
        "google/siglip-base-patch16-224",
        "7fd15f0689c79d79e38b1c2e2e2370a7bf2761ed",
        SIGLIP_SCRIPT,
    );
    // Captions are on disk but not recorded as installed in setup.json: not registered.
    installed(
        &home,
        "florence.py",
        "florence.json",
        "florence-community/Florence-2-base",
        "00921df66db728a9ceb750f5eca43e5c203a2051",
        FLORENCE_SCRIPT,
    );
    fs::write(
        home.join("setup.json"),
        r#"{"version":1,"extras":{"search":{"state":"installed","at":"2026-10-24T12:00:00Z","seconds_per_item":2.0}}}"#,
    )
    .unwrap();
    let init = json(&vamp(&home, &["init", "--name", "refs"]));
    assert_eq!(init["extras"], json!(["search"]));
    let processors = json(&vamp(&home, &["processors", "list"]));
    let names: Vec<&str> = processors
        .as_array()
        .unwrap()
        .iter()
        .map(|spec| spec["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["siglip-embedding"]);
    let command = processors[0]["command"].as_array().unwrap();
    assert_eq!(command[1], json!(home.join("tools/models/siglip.py")));
    assert_eq!(command[5], "cpu");
    let embedder = json(&vamp(&home, &["embedder", "show"]));
    assert_eq!(
        embedder["embedder"]["model"],
        "google/siglip-base-patch16-224"
    );

    let plan = json(&vamp(&home, &["setup", "plan"]));
    assert_eq!(plan["library"]["context"], "refs");
    assert_eq!(plan["library"]["items"], 0);
    assert_eq!(extra(&plan, "search")["state"], "installed");
    assert_eq!(extra(&plan, "search")["seconds_measured"], true);
    assert_eq!(extra(&plan, "captions")["state"], "installed");

    // A home without setup.json registers nothing.
    let bare = dir.path().join("bare");
    assert_eq!(
        json(&vamp(&bare, &["init", "--name", "refs"]))["extras"],
        json!([])
    );
}
