use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn build_value(name: &str, fallback: &str) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn git_revision() -> String {
    if let Ok(revision) = env::var("BUILD_REVISION") {
        if !revision.trim().is_empty() {
            return revision;
        }
    }

    Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|revision| revision.trim().to_string())
        .filter(|revision| !revision.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn build_time() -> String {
    build_value(
        "BUILD_TIME",
        &SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
            |_| String::from("unknown"),
            |value| value.as_secs().to_string(),
        ),
    )
}

fn emit_git_rerun_paths() {
    let head = Path::new("../../.git/HEAD");
    println!("cargo:rerun-if-changed={}", head.display());
    if let Ok(contents) = fs::read_to_string(head) {
        if let Some(reference) = contents.trim().strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed=../../.git/{reference}");
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=BUILD_REVISION");
    println!("cargo:rerun-if-env-changed=BUILD_TIME");
    emit_git_rerun_paths();
    println!("cargo:rustc-env=TM_GIT_REVISION={}", git_revision());
    println!("cargo:rustc-env=TM_BUILD_TIME={}", build_time());
    println!(
        "cargo:rustc-env=TM_BUILD_TARGET={}",
        build_value("TARGET", "unknown")
    );
}
