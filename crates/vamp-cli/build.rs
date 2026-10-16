//! `vamp guide` text: docs/agents/SKILL.md without its YAML front matter. Release builds
//! (`VAMP_RELEASE_TARGET`, read by `vamp self-update`) recompile when it changes.

use std::{env, fs, path::Path};

fn main() {
    println!("cargo::rerun-if-env-changed=VAMP_RELEASE_TARGET");
    let skill = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../docs/agents/SKILL.md");
    println!("cargo::rerun-if-changed={}", skill.display());
    let text = fs::read_to_string(&skill).expect("reading docs/agents/SKILL.md");
    let body = text
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map_or(text.as_str(), |(_, body)| body.trim_start());
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("guide.md"), body).unwrap();
}
