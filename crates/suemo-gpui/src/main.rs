//! The `suemo` binary: core verbs dispatch into the suemo lib, `gui` into
//! the GPUI front-end (see this package's Cargo.toml for why it lives here).

use clap::Parser;

fn main() {
    env_logger::init();
    let cli = suemo::cli::Cli::parse();
    let result = if matches!(cli.cmd, suemo::cli::Cmd::Gui) {
        suemo_gpui::run()
    } else {
        suemo::cli::run(cli)
    };
    if let Err(err) = result {
        log::error!("error: {err:#}");
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
