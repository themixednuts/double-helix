fn main() -> anyhow::Result<()> {
    helix_loader::initialize_config_file(None);
    helix_loader::initialize_log_file(None);
    let exit_code = helix_term::storybook::run_cli(std::env::args().skip(1))?;
    std::process::exit(exit_code);
}
