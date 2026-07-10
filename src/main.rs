mod cli;
mod config;
mod doctor;
mod dump;
mod sync;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::config::Config;

fn main() {
    if let Err(error) = run() {
        eprintln!("w4dj: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut cli = Cli::parse();
    if let Some(Command::Doctor(args)) = cli.command.take() {
        return doctor::run(args);
    }
    let config = Config::resolve(cli)?;

    println!("W4DJ");
    println!("  inputs : {}", config.inputs.len());
    println!("  output : {}", config.output.display());
    println!("  profile: {}", config.mode.profile());

    sync::run(&config)
}
