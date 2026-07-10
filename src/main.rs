mod cli;
mod config;
mod doctor;
mod dump;
mod gui;
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
    if std::env::args_os().len() == 1 {
        detach_console_for_gui();
        return gui::run();
    }

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

#[cfg(windows)]
fn detach_console_for_gui() {
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn FreeConsole() -> i32;
    }

    // A console-subsystem binary keeps CLI output working, while the no-argument
    // GUI path detaches the transient console created by a Windows double-click.
    unsafe {
        FreeConsole();
    }
}

#[cfg(not(windows))]
fn detach_console_for_gui() {}
