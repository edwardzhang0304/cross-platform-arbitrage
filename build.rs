use sha2::{Digest, Sha256};
use std::{env, fs, path::Path, process::Command};

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=config/shared-core-files.json");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string("config/shared-core-files.json").expect("shared core manifest")
    ).expect("valid shared core manifest");
    let mut hash = Sha256::new();
    for file in manifest["files"].as_array().expect("core file list") {
        let file = file.as_str().expect("core path");
        println!("cargo:rerun-if-changed={file}");
        // Windows checkout line endings must not change a source identity.
        let source = fs::read_to_string(file).expect("shared core source").replace("\r\n", "\n");
        hash.update(file.as_bytes()); hash.update([0]);
        hash.update(source.as_bytes()); hash.update([0]);
    }
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    if let Ok(expected) = env::var("GITHUB_SHA") {
        assert_eq!(commit, expected, "build must use the checked-out release commit");
    }
    // Refresh the stamp when an existing checkout switches revisions.
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&["rev-parse", "--git-path", &reference]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_none_or(|s| !s.is_empty());
    let info = serde_json::json!({
        "schema": 1, "version": env::var("CARGO_PKG_VERSION").unwrap(),
        "source_commit": commit, "dirty": dirty,
        "strategy_core_sha256": format!("{:x}", hash.finalize()),
        "target": env::var("TARGET").unwrap(),
        "runtime": if env::var_os("CARGO_FEATURE_PAPER_RUNTIME").is_some() {"paper"} else {"live"}
    });
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("build-info.json"), info.to_string()).unwrap();
}
