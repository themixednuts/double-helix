use anyhow::{Context, Error, Result};
use helix_loader::VERSION_AND_GIT_HASH;
use helix_pkg::{OpEvent, Ops, PackageChange, PackageSpec, PkgKind, RegistrySource, UpdatePlan};
use helix_term::application::Application;
use helix_term::args::{Args, PkgCommand};
use helix_term::config::{Config, ConfigLoadError};

fn setup_logging(verbosity: u64) -> Result<()> {
    let mut base_config = fern::Dispatch::new();

    base_config = match verbosity {
        0 => base_config.level(log::LevelFilter::Warn),
        1 => base_config.level(log::LevelFilter::Info),
        2 => base_config.level(log::LevelFilter::Debug),
        _3_or_more => base_config.level(log::LevelFilter::Trace),
    };

    // Separate file config so we can include year, month and day in file logs
    let file_config = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} {} [{}] {}",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f"),
                record.target(),
                record.level(),
                message
            ))
        })
        .chain(fern::log_file(helix_loader::log_file())?);

    base_config.chain(file_config).apply()?;

    Ok(())
}

fn main() -> Result<()> {
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("unable to build tokio runtime")?;
    let runtime = helix_runtime::Runtime::new(tokio_runtime.handle().clone());
    let exit_code = tokio_runtime.block_on(main_impl(runtime))?;
    std::process::exit(exit_code);
}

async fn main_impl(runtime: helix_runtime::Runtime) -> Result<i32> {
    let args = Args::parse_args().context("could not parse arguments")?;

    if args.migrate {
        helix_term::migration::migrate_from_helix()?;
        return Ok(0);
    }

    helix_loader::initialize_config_file(args.config_file.clone());
    helix_loader::initialize_log_file(args.log_file.clone());

    // Help has a higher priority and should be handled separately.
    if args.display_help {
        print!(
            "\
{} {}
{}
{}

USAGE:
    dhx [FLAGS] [files]...

ARGS:
    <files>...    Set the input file to use, position can also be specified via file[:row[:col]]

FLAGS:
    -h, --help                     Print help information
    --tutor                        Load the tutorial
    --migrate                      Copy existing Helix config into Double Helix config paths
    pkg <cmd>                      Manage runtime packages (install, update, rollback, list, search, sync, doctor)
    --health [CATEGORY]            Check for potential errors in editor setup
                                   CATEGORY can be a language or one of 'clipboard', 'languages',
                                   'all-languages' or 'all'. 'languages' is filtered according to
                                   user config, 'all-languages' and 'all' are not. If not specified,
                                   the default is the same as 'all', but with languages filtering.
    -g, --grammar {{fetch|build}}    Fetch or builds tree-sitter grammars listed in languages.toml
    -c, --config <file>            Specify a file to use for configuration
    -v                             Increase logging verbosity each use for up to 3 times
    --log <file>                   Specify a file to use for logging
                                   (default file: {})
    -V, --version                  Print version information
    --vsplit                       Split all given files vertically into different windows
    --hsplit                       Split all given files horizontally into different windows
    -w, --working-dir <path>       Specify an initial working directory
    +[N]                           Open the first given file at line number N, or the last line, if
                                   N is not specified.
",
            env!("CARGO_PKG_NAME"),
            VERSION_AND_GIT_HASH,
            env!("CARGO_PKG_AUTHORS"),
            env!("CARGO_PKG_DESCRIPTION"),
            helix_loader::default_log_file().display(),
        );
        std::process::exit(0);
    }

    if args.display_version {
        println!("double-helix {}", VERSION_AND_GIT_HASH);
        std::process::exit(0);
    }

    if let Some(pkg) = args.pkg {
        return run_pkg(pkg.command);
    }

    if args.health {
        if let Err(err) = helix_term::health::print_health(args.health_arg) {
            // Piping to for example `head -10` requires special handling:
            // https://stackoverflow.com/a/65760807/7115678
            if err.kind() != std::io::ErrorKind::BrokenPipe {
                return Err(err.into());
            }
        }

        std::process::exit(0);
    }

    if args.fetch_grammars {
        helix_loader::grammar::fetch_grammars()?;
        return Ok(0);
    }

    if args.build_grammars {
        helix_loader::grammar::build_grammars(None)?;
        return Ok(0);
    }

    setup_logging(args.verbosity).context("failed to initialize logging")?;

    // NOTE: Set the working directory early so the correct configuration is loaded. Be aware that
    // Application::new() depends on this logic so it must be updated if this changes.
    if let Some(path) = &args.working_directory {
        helix_stdx::env::set_current_working_dir(path)?;
    } else if let Some((path, _)) = args.files.first().filter(|p| p.0.is_dir()) {
        // If the first file is a directory, it will be the working directory unless -w was specified
        helix_stdx::env::set_current_working_dir(path)?;
    }

    let config = match Config::load_default() {
        Ok(config) => config,
        Err(ConfigLoadError::Error(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            Config::default()
        }
        Err(ConfigLoadError::Error(err)) => return Err(Error::new(err)),
        Err(ConfigLoadError::BadConfig(err)) => {
            eprintln!("Bad config: {}", err);
            eprintln!("Press <ENTER> to continue with default config");
            use std::io::Read;
            let _ = std::io::stdin().read(&mut []);
            Config::default()
        }
    };

    let lang_loader = helix_core::config::user_lang_loader().unwrap_or_else(|err| {
        eprintln!("{}", err);
        eprintln!("Press <ENTER> to continue with default language config");
        use std::io::Read;
        // This waits for an enter press.
        let _ = std::io::stdin().read(&mut []);
        helix_core::config::default_lang_loader()
    });

    let mut app =
        Application::new(args, config, lang_loader, runtime).context("unable to start Helix")?;
    let mut events = app.event_stream();

    let exit_code = app.run(&mut events).await?;

    Ok(exit_code)
}

fn run_pkg(command: PkgCommand) -> Result<i32> {
    match command {
        PkgCommand::Help => {
            print_pkg_help();
            Ok(0)
        }
        PkgCommand::Install(names) => {
            let ops = Ops::open_default()?;
            ops.install(&names, &mut print_pkg_event)?;
            Ok(0)
        }
        PkgCommand::Remove(name) => {
            let ops = Ops::open_default()?;
            ops.remove(std::slice::from_ref(&name))?;
            println!("removed {name}");
            Ok(0)
        }
        PkgCommand::List { kind } => {
            let ops = Ops::open_default()?;
            let kind = kind
                .as_deref()
                .map(str::parse::<PkgKind>)
                .transpose()
                .context("invalid --kind")?;
            let receipts = ops.store().receipts()?;
            let mut count = 0usize;
            for receipt in receipts {
                if kind.is_some_and(|kind| kind != receipt.kind) {
                    continue;
                }
                count += 1;
                println!(
                    "{:<12} {:<24} {:<16} {}",
                    receipt.kind, receipt.name, receipt.version, receipt.shim
                );
            }
            if count == 0 {
                println!("no packages installed");
            }
            Ok(0)
        }
        PkgCommand::Search(term) => {
            let ops = Ops::open_default()?;
            for package in ops.registry().search(&term) {
                println!(
                    "{:<12} {:<24} {:<28} {}",
                    package.kind,
                    package.name,
                    package_tags(package),
                    package.description
                );
            }
            Ok(0)
        }
        PkgCommand::Lock {
            project,
            fetch_hashes,
            names,
        } => {
            let ops = Ops::open_default()?;
            let options = helix_pkg::LockOptions { fetch_hashes };
            let lock = if let Some(project) = project {
                ops.lock_project_with_options(&project, &names, options, &mut print_pkg_event)?
            } else {
                ops.lock_manifest_with_options(&names, options, &mut print_pkg_event)?
            };
            println!("wrote pkg.lock with {} package(s)", lock.packages.len());
            Ok(0)
        }
        PkgCommand::Sync { project } => {
            let ops = Ops::open_default()?;
            if let Some(project) = project {
                ops.sync_with_project(&project, &mut print_pkg_event)?;
            } else {
                ops.sync(&mut print_pkg_event)?;
            }
            Ok(0)
        }
        PkgCommand::Doctor => {
            let ops = Ops::open_default()?;
            let report = ops.doctor()?;
            for name in &report.ok {
                println!("ok {name}");
            }
            for (name, err) in &report.bad {
                eprintln!("bad {name}: {err}");
            }
            Ok(if report.bad.is_empty() { 0 } else { 1 })
        }
        PkgCommand::Outdated(names) => {
            let ops = Ops::open_default()?;
            let report = ops.outdated(&names)?;
            let mut count = 0usize;
            for package in report {
                match (package.latest, package.error) {
                    (Some(latest), _) if latest != package.installed => {
                        count += 1;
                        println!(
                            "{:<12} {:<28} {:<16} {}",
                            package.kind, package.name, package.installed, latest
                        );
                    }
                    (None, Some(error)) => {
                        count += 1;
                        println!(
                            "{:<12} {:<28} {:<16} error: {}",
                            package.kind, package.name, package.installed, error
                        );
                    }
                    _ => {}
                }
            }
            if count == 0 {
                println!("all installed packages are current");
            }
            Ok(0)
        }
        PkgCommand::Update(names) => {
            let ops = Ops::open_default()?;
            ops.update(&names, &mut print_pkg_event)?;
            Ok(0)
        }
        PkgCommand::UpdatePlan(names) => {
            let ops = Ops::open_default()?;
            let plan = ops.plan_update(&names)?;
            print_update_plan(&plan);
            Ok(if plan.has_errors() { 1 } else { 0 })
        }
        PkgCommand::RegistryList => {
            let ops = Ops::open_default()?;
            print_registry_sources(&ops);
            Ok(0)
        }
        PkgCommand::RegistryUpdate(names) => {
            let ops = Ops::open_default()?;
            let updates = ops.update_registries(&names)?;
            if updates.is_empty() {
                println!("no registry sources configured");
            }
            for update in updates {
                println!(
                    "{:<12} {:<8} {:<40} {}",
                    update.name,
                    update.status,
                    update.path.display(),
                    update.source
                );
            }
            Ok(0)
        }
        PkgCommand::Rollback(name) => {
            let ops = Ops::open_default()?;
            let locked = ops.rollback(&name)?;
            println!("rolled back {} to {}", locked.name, locked.version);
            Ok(0)
        }
    }
}

fn print_registry_sources(ops: &Ops) {
    let config = ops.config();
    let mut count = 0usize;
    for path in &config.registries {
        count += 1;
        println!("{:<12} {:<8} {}", "local", "path", path.display());
    }
    for source in &config.registry_sources {
        count += 1;
        println!(
            "{:<12} {:<8} {:<40} {}",
            source.name,
            registry_source_kind(source),
            source
                .active_dir(ops.store())
                .map(|path| path.display().to_string())
                .unwrap_or_else(|err| format!("error: {err}")),
            source.source_label()
        );
    }
    if count == 0 {
        println!("no registry sources configured");
    }
}

fn registry_source_kind(source: &RegistrySource) -> &'static str {
    if source.path.is_some() {
        "path"
    } else {
        "git"
    }
}

fn print_update_plan(plan: &UpdatePlan) {
    if plan.changes.is_empty() {
        println!("no packages to plan");
        return;
    }
    for change in &plan.changes {
        let candidate = change
            .candidate
            .as_ref()
            .map(|candidate| candidate.version.as_str())
            .unwrap_or("-");
        let source = change
            .candidate
            .as_ref()
            .map(|candidate| candidate.source.as_str())
            .unwrap_or("-");
        println!(
            "{:<8} {:<12} {:<28} {:<16} -> {:<16} {:<14} {}",
            update_plan_action(change),
            change.kind,
            change.name,
            change.installed.as_deref().unwrap_or("-"),
            candidate,
            source,
            change
                .candidate
                .as_ref()
                .map(|candidate| candidate.url.as_str())
                .unwrap_or("")
        );
        for warning in &change.warnings {
            println!("  warning: {warning}");
        }
        if let Some(error) = &change.error {
            println!("  error: {error}");
        }
    }
}

fn update_plan_action(change: &PackageChange) -> &'static str {
    if change.error.is_some() {
        "error"
    } else if change.installed.is_none() && change.candidate.is_some() {
        "install"
    } else if change.needs_apply() {
        "update"
    } else {
        "current"
    }
}

fn package_tags(package: &PackageSpec) -> String {
    let mut tags = vec![package.kind.default_category().to_owned()];
    tags.extend(package.languages.iter().cloned());
    tags.extend(package.categories.iter().cloned());
    if !package.aliases.is_empty() {
        tags.push(format!("alias:{}", package.aliases.join("|")));
    }
    tags.join(",")
}

fn print_pkg_event(event: OpEvent) {
    match event {
        OpEvent::Started { name } => println!("installing {name}"),
        OpEvent::Progress { name, message, .. } => println!("{name}: {message}"),
        OpEvent::Done { name } => println!("done {name}"),
        OpEvent::Failed { name, message } => eprintln!("failed {name}: {message}"),
    }
}

fn print_pkg_help() {
    print!(
        "\
USAGE:
    dhx pkg <COMMAND>

COMMANDS:
    install <name>...       Install packages from the builtin registry
    update [name]...        Update installed packages
    update --plan [name]... Show the update plan without installing
    registry list          List configured registry sources
    registry update [name] Update cached git registry sources
    outdated [name]...      Show installed packages with newer versions
    rollback <name>         Reactivate the previous installed version
    remove <name>           Deactivate an installed package
    list [--kind <kind>]    List installed packages
    search <term>           Search registry names, languages, aliases, categories, and schemas
    lock [--project <dir>] [name]...
                            Refresh pkg.lock without installing
    sync                    Install packages pinned in pkg.lock
    sync --project <dir>    Merge user pkg.toml with <dir>/.helix/pkg.toml
    doctor                  Verify receipts and installed files
"
    );
}
