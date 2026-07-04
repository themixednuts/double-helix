use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::DynError;

const UPSTREAM_REPO: &str = "https://github.com/dmtrKovalenko/fff.git";
const UPSTREAM_CORE_PATH: &str = "crates/fff-core";
const VENDOR_CORE_PATH: &str = "vendor/fff-search";

const LOCAL_EXTENSION_SYMBOLS: &[&str] = &[
    "FilePickerScanOptions",
    "scan: FilePickerScanOptions",
    "ContentOverlay",
    "OwnedGrepMatch",
    "OwnedGrepResult",
    "grep_owned",
    "grep_bytes",
    "FrecencyStore",
    "QueryTrackerStore",
];

#[derive(Debug)]
struct Options {
    git_ref: String,
    fail_on_drift: bool,
    keep_temp: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            git_ref: "main".to_string(),
            fail_on_drift: false,
            keep_temp: false,
        }
    }
}

pub fn check(args: impl Iterator<Item = String>) -> Result<(), DynError> {
    let options = parse_args(args)?;
    let root = crate::path::project_root();
    let vendor = root.join(VENDOR_CORE_PATH);
    let vendor_src = vendor.join("src");
    if !vendor_src.is_dir() {
        return Err(format!("vendored FFF source not found: {}", vendor_src.display()).into());
    }

    let temp = std::env::temp_dir().join(format!(
        "dhx-fff-upstream-{}-{}",
        sanitize_ref(&options.git_ref),
        std::process::id()
    ));
    if temp.exists() {
        remove_temp_dir(&temp)?;
    }

    run(Command::new("git")
        .args(["clone", "--depth", "1", "--branch"])
        .arg(&options.git_ref)
        .arg(UPSTREAM_REPO)
        .arg(&temp))?;

    let upstream = temp.join(UPSTREAM_CORE_PATH);
    let upstream_src = upstream.join("src");
    if !upstream_src.is_dir() {
        return Err(format!(
            "upstream FFF core source not found at ref {}: {}",
            options.git_ref,
            upstream_src.display()
        )
        .into());
    }

    let head = output(
        Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .current_dir(&temp),
    )?;
    let tags = output(
        Command::new("git")
            .args(["tag", "--points-at", "HEAD"])
            .current_dir(&temp),
    )?;

    println!("FFF upstream: {UPSTREAM_REPO}");
    println!("Compared ref: {} ({head})", options.git_ref);
    if tags.trim().is_empty() {
        println!("Tags at ref: <none>");
    } else {
        println!(
            "Tags at ref: {}",
            tags.split_whitespace().collect::<Vec<_>>().join(", ")
        );
    }
    println!("Vendored core: {}", vendor.display());
    println!("Upstream core: {}", upstream.display());

    let diff = diff_stat(&vendor_src, &upstream_src)?;
    if diff.has_drift {
        println!("\nSource drift:");
        print!("{}", diff.stat);
    } else {
        println!("\nSource drift: none");
    }

    let vendor_text = read_rust_sources(&vendor_src)?;
    let upstream_text = read_rust_sources(&upstream_src)?;
    let local_only = local_only_symbols(&vendor_text, &upstream_text);
    if local_only.is_empty() {
        println!("\nLocal extension symbols: all present upstream");
    } else {
        println!("\nLocal extension symbols absent upstream:");
        for symbol in &local_only {
            println!("  - {symbol}");
        }
    }

    let parser_config = temp
        .join("crates")
        .join("fff-query-parser")
        .join("src")
        .join("config.rs");
    if let Ok(config) = fs::read_to_string(parser_config) {
        let has_dir_search = config.contains("DirSearchConfig");
        let has_mixed_search = config.contains("MixedSearchConfig");
        println!(
            "\nUpstream query parser additions: DirSearchConfig={} MixedSearchConfig={}",
            has_dir_search, has_mixed_search
        );
    }

    if !options.keep_temp {
        remove_temp_dir(&temp)?;
    } else {
        println!("\nKept upstream checkout: {}", temp.display());
    }

    if options.fail_on_drift && (diff.has_drift || !local_only.is_empty()) {
        return Err("FFF upstream drift detected".into());
    }

    Ok(())
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Options, DynError> {
    let mut options = Options::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ref" => {
                let Some(git_ref) = args.next() else {
                    return Err("--ref requires a value".into());
                };
                options.git_ref = git_ref;
            }
            "--fail-on-drift" => options.fail_on_drift = true,
            "--keep-temp" => options.keep_temp = true,
            "--help" | "-h" => {
                println!(
                    "\
Usage: cargo xtask fff-upstream [--ref REF] [--fail-on-drift] [--keep-temp]

Compares vendor/fff-search against dmtrKovalenko/fff crates/fff-core.
Defaults to --ref main. Use a nightly tag, e.g. --ref 0.9.7-nightly.1cd8d31,
to inspect a published prerelease.
"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown fff-upstream argument: {other}").into()),
        }
    }
    Ok(options)
}

#[derive(Debug)]
struct DiffStat {
    has_drift: bool,
    stat: String,
}

fn diff_stat(vendor_src: &Path, upstream_src: &Path) -> Result<DiffStat, DynError> {
    let output = Command::new("git")
        .args(["diff", "--no-index", "--stat"])
        .arg(vendor_src)
        .arg(upstream_src)
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    match output.status.code() {
        Some(0) => Ok(DiffStat {
            has_drift: false,
            stat: stdout,
        }),
        Some(1) => Ok(DiffStat {
            has_drift: true,
            stat: stdout,
        }),
        _ => Err(format!("git diff failed: {stderr}").into()),
    }
}

fn local_only_symbols(vendor_text: &str, upstream_text: &str) -> Vec<&'static str> {
    LOCAL_EXTENSION_SYMBOLS
        .iter()
        .copied()
        .filter(|symbol| vendor_text.contains(symbol) && !upstream_text.contains(symbol))
        .collect()
}

fn read_rust_sources(root: &Path) -> Result<String, DynError> {
    let mut content = String::new();
    read_rust_sources_into(root, &mut content)?;
    Ok(content)
}

fn read_rust_sources_into(path: &Path, content: &mut String) -> Result<(), DynError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            read_rust_sources_into(&path, content)?;
        } else if path.extension() == Some(OsStr::new("rs")) {
            content.push_str(&fs::read_to_string(path)?);
            content.push('\n');
        }
    }
    Ok(())
}

fn run(command: &mut Command) -> Result<(), DynError> {
    let output = command.output()?;
    if output.status.success() {
        return Ok(());
    }

    Err(format!(
        "command failed: {:?}\n{}",
        command,
        String::from_utf8_lossy(&output.stderr)
    )
    .into())
}

fn output(command: &mut Command) -> Result<String, DynError> {
    let output = command.output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!(
            "command failed: {:?}\n{}",
            command,
            String::from_utf8_lossy(&output.stderr)
        )
        .into())
    }
}

fn sanitize_ref(git_ref: &str) -> String {
    git_ref
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn remove_temp_dir(path: &PathBuf) -> Result<(), DynError> {
    let temp_root = std::env::temp_dir().canonicalize()?;
    let target = if path.exists() {
        path.canonicalize()?
    } else {
        path.clone()
    };
    if !target.starts_with(&temp_root) {
        return Err(format!(
            "refusing to remove non-temp directory: {}",
            target.display()
        )
        .into());
    }
    fs::remove_dir_all(path)?;
    Ok(())
}
