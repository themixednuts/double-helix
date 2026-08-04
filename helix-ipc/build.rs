use std::borrow::Cow;
use std::path::Path;
use std::process::Command;

const MAJOR: &str = env!("CARGO_PKG_VERSION_MAJOR");
const MINOR: &str = env!("CARGO_PKG_VERSION_MINOR");
const PATCH: &str = env!("CARGO_PKG_VERSION_PATCH");

fn main() {
    println!(
        "cargo:rustc-env=BUILD_TARGET={}",
        std::env::var("TARGET").expect("Cargo did not set TARGET")
    );
    let git_hash = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .or_else(|| option_env!("HELIX_NIX_BUILD_REV").map(str::to_owned));

    let minor = if MINOR.len() == 1 {
        format!("0{MINOR}")
    } else {
        MINOR.to_owned()
    };
    let calver = if PATCH == "0" {
        format!("{MAJOR}.{minor}")
    } else {
        format!("{MAJOR}.{minor}.{PATCH}")
    };
    let version: Cow<'_, str> = match &git_hash {
        Some(git_hash) => format!("{calver} ({})", &git_hash[..8]).into(),
        None => calver.into(),
    };
    println!("cargo:rustc-env=VERSION_AND_GIT_HASH={version}");

    let Some(git_dir) = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.trim().to_owned())
    else {
        return;
    };
    let head = Path::new(&git_dir).join("HEAD");
    if head.exists() {
        println!("cargo:rerun-if-changed={}", head.display());
    }
    let Some(head_ref) = Command::new("git")
        .args(["symbolic-ref", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.trim().to_owned())
    else {
        return;
    };
    let head_ref = Path::new(&git_dir).join(head_ref);
    if head_ref.exists() {
        println!("cargo:rerun-if-changed={}", head_ref.display());
    }
}
