use anyhow::Result;
use helix_core::Position;
use helix_view::tree::Layout;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Args {
    pub display_help: bool,
    pub display_version: bool,
    pub health: bool,
    pub health_arg: Option<String>,
    pub load_tutor: bool,
    pub migrate: bool,
    pub fetch_grammars: bool,
    pub build_grammars: bool,
    pub pkg: Option<PkgArgs>,
    pub split: Option<Layout>,
    pub verbosity: u64,
    pub log_file: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    pub files: IndexMap<PathBuf, Vec<Position>>,
    pub working_directory: Option<PathBuf>,
}

pub struct PkgArgs {
    pub command: PkgCommand,
}

pub enum PkgCommand {
    Install(Vec<String>),
    Remove(String),
    List {
        kind: Option<String>,
    },
    Search(String),
    Lock {
        project: Option<PathBuf>,
        names: Vec<String>,
    },
    Sync {
        project: Option<PathBuf>,
    },
    Doctor,
    Outdated(Vec<String>),
    Update(Vec<String>),
    UpdatePlan(Vec<String>),
    RegistryList,
    RegistryUpdate(Vec<String>),
    Rollback(String),
    Help,
}

impl Args {
    pub fn parse_args() -> Result<Args> {
        let mut args = Args::default();
        let mut argv = std::env::args().peekable();
        let mut line_number = 0;

        let mut insert_file_with_position = |file_with_position: &str| {
            let (filename, position) = parse_file(file_with_position);

            // Before setting the working directory, resolve all the paths in args.files
            let filename = helix_stdx::path::canonicalize(filename);

            args.files
                .entry(filename)
                .and_modify(|positions| positions.push(position))
                .or_insert_with(|| vec![position]);
        };

        argv.next(); // skip the program, we don't care about that

        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "--" => break, // stop parsing at this point treat the remaining as files
                "--version" => args.display_version = true,
                "--help" => args.display_help = true,
                "--tutor" => args.load_tutor = true,
                "--migrate" => args.migrate = true,
                "--vsplit" => match args.split {
                    Some(_) => anyhow::bail!("can only set a split once of a specific type"),
                    None => args.split = Some(Layout::Vertical),
                },
                "--hsplit" => match args.split {
                    Some(_) => anyhow::bail!("can only set a split once of a specific type"),
                    None => args.split = Some(Layout::Horizontal),
                },
                "--health" => {
                    args.health = true;
                    args.health_arg = argv.next_if(|opt| !opt.starts_with('-'));
                }
                "-g" | "--grammar" => match argv.next().as_deref() {
                    Some("fetch") => args.fetch_grammars = true,
                    Some("build") => args.build_grammars = true,
                    _ => {
                        anyhow::bail!("--grammar must be followed by either 'fetch' or 'build'")
                    }
                },
                "pkg" => {
                    args.pkg = Some(parse_pkg_args(argv.by_ref().collect())?);
                    break;
                }
                "-c" | "--config" => match argv.next().as_deref() {
                    Some(path) => args.config_file = Some(path.into()),
                    None => anyhow::bail!("--config must specify a path to read"),
                },
                "--log" => match argv.next().as_deref() {
                    Some(path) => args.log_file = Some(path.into()),
                    None => anyhow::bail!("--log must specify a path to write"),
                },
                "-w" | "--working-dir" => match argv.next().as_deref() {
                    Some(path) => {
                        args.working_directory = if Path::new(path).is_dir() {
                            Some(PathBuf::from(path))
                        } else {
                            anyhow::bail!(
                                "--working-dir specified does not exist or is not a directory"
                            )
                        }
                    }
                    None => {
                        anyhow::bail!("--working-dir must specify an initial working directory")
                    }
                },
                arg if arg.starts_with("--") => {
                    anyhow::bail!("unexpected double dash argument: {}", arg)
                }
                arg if arg.starts_with('-') => {
                    let arg = arg.get(1..).unwrap().chars();
                    for chr in arg {
                        match chr {
                            'v' => args.verbosity += 1,
                            'V' => args.display_version = true,
                            'h' => args.display_help = true,
                            _ => anyhow::bail!("unexpected short arg {}", chr),
                        }
                    }
                }
                "+" => line_number = usize::MAX,
                arg if arg.starts_with('+') => {
                    match arg[1..].parse::<usize>() {
                        Ok(n) => line_number = n.saturating_sub(1),
                        _ => insert_file_with_position(arg),
                    };
                }
                arg => insert_file_with_position(arg),
            }
        }

        // push the remaining args, if any to the files
        for arg in argv {
            insert_file_with_position(&arg);
        }

        if line_number != 0 {
            if let Some(first_position) = args
                .files
                .first_mut()
                .and_then(|(_, positions)| positions.first_mut())
            {
                first_position.row = line_number;
            }
        }

        Ok(args)
    }
}

fn parse_pkg_args(args: Vec<String>) -> Result<PkgArgs> {
    let mut args = args.into_iter();
    let command = match args.next().as_deref() {
        Some("install") => {
            let names: Vec<String> = args.collect();
            if names.is_empty() {
                anyhow::bail!("pkg install requires at least one package name");
            }
            PkgCommand::Install(names)
        }
        Some("remove") => {
            let Some(name) = args.next() else {
                anyhow::bail!("pkg remove requires a package name");
            };
            if args.next().is_some() {
                anyhow::bail!("pkg remove accepts one package name");
            }
            PkgCommand::Remove(name)
        }
        Some("list") => {
            let mut kind = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--kind" => {
                        let Some(value) = args.next() else {
                            anyhow::bail!("pkg list --kind requires a value");
                        };
                        kind = Some(value);
                    }
                    other => anyhow::bail!("unexpected pkg list argument: {other}"),
                }
            }
            PkgCommand::List { kind }
        }
        Some("search") => {
            let Some(term) = args.next() else {
                anyhow::bail!("pkg search requires a search term");
            };
            if args.next().is_some() {
                anyhow::bail!("pkg search accepts one search term");
            }
            PkgCommand::Search(term)
        }
        Some("lock") => {
            let mut project = None;
            let mut names = Vec::new();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--project" => {
                        let Some(path) = args.next() else {
                            anyhow::bail!("pkg lock --project requires a directory");
                        };
                        project = Some(PathBuf::from(path));
                    }
                    name => names.push(name.to_owned()),
                }
            }
            PkgCommand::Lock { project, names }
        }
        Some("sync") => {
            let mut project = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--project" => {
                        let Some(path) = args.next() else {
                            anyhow::bail!("pkg sync --project requires a directory");
                        };
                        project = Some(PathBuf::from(path));
                    }
                    other => anyhow::bail!("unexpected pkg sync argument: {other}"),
                }
            }
            PkgCommand::Sync { project }
        }
        Some("doctor") => PkgCommand::Doctor,
        Some("outdated") => PkgCommand::Outdated(args.collect()),
        Some("update") => {
            let mut plan = false;
            let mut names = Vec::new();
            for arg in args {
                if arg == "--plan" {
                    plan = true;
                } else {
                    names.push(arg);
                }
            }
            if plan {
                PkgCommand::UpdatePlan(names)
            } else {
                PkgCommand::Update(names)
            }
        }
        Some("registry") => match args.next().as_deref() {
            Some("list") => {
                if args.next().is_some() {
                    anyhow::bail!("pkg registry list accepts no arguments");
                }
                PkgCommand::RegistryList
            }
            Some("update") => PkgCommand::RegistryUpdate(args.collect()),
            Some("-h" | "--help") | None => PkgCommand::Help,
            Some(other) => anyhow::bail!("unknown pkg registry command: {other}"),
        },
        Some("rollback") => {
            let Some(name) = args.next() else {
                anyhow::bail!("pkg rollback requires a package name");
            };
            if args.next().is_some() {
                anyhow::bail!("pkg rollback accepts one package name");
            }
            PkgCommand::Rollback(name)
        }
        Some("-h" | "--help") | None => PkgCommand::Help,
        Some(other) => anyhow::bail!("unknown pkg command: {other}"),
    };
    Ok(PkgArgs { command })
}

/// Parse arg into [`PathBuf`] and position.
pub(crate) fn parse_file(s: &str) -> (PathBuf, Position) {
    let def = || (PathBuf::from(s), Position::default());
    if Path::new(s).exists() {
        return def();
    }
    split_path_row_col(s)
        .or_else(|| split_path_row(s))
        .unwrap_or_else(def)
}

/// Split file.rs:10:2 into [`PathBuf`], row and col.
///
/// Does not validate if file.rs is a file or directory.
fn split_path_row_col(s: &str) -> Option<(PathBuf, Position)> {
    let mut s = s.trim_end_matches(':').rsplitn(3, ':');
    let col: usize = s.next()?.parse().ok()?;
    let row: usize = s.next()?.parse().ok()?;
    let path = s.next()?.into();
    let pos = Position::new(row.saturating_sub(1), col.saturating_sub(1));
    Some((path, pos))
}

/// Split file.rs:10 into [`PathBuf`] and row.
///
/// Does not validate if file.rs is a file or directory.
fn split_path_row(s: &str) -> Option<(PathBuf, Position)> {
    let (path, row) = s.trim_end_matches(':').rsplit_once(':')?;
    let row: usize = row.parse().ok()?;
    let path = path.into();
    let pos = Position::new(row.saturating_sub(1), 0);
    Some((path, pos))
}
