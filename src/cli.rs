use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};

use crate::config::Mode;

#[derive(Debug, Parser)]
#[command(
    name = "w4dj",
    version,
    author = "slipstream",
    about = "Netease Cloud Music library synchronizer",
    subcommand_precedence_over_arg = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// One or more files or directories to scan.
    #[arg(
        long,
        short = 'i',
        value_name = "PATH",
        num_args = 1..,
        action = ArgAction::Append
    )]
    pub input: Vec<PathBuf>,

    /// Files or directories dropped onto the executable.
    #[arg(value_name = "PATH")]
    pub dropped_input: Vec<PathBuf>,

    /// Output directory. Defaults to w4djdump next to the executable.
    #[arg(long, short = 'o', value_name = "DIR")]
    pub output: Option<PathBuf>,

    /// Output profile.
    #[arg(long, short = 'm', value_enum)]
    pub mode: Option<Mode>,

    /// TOML configuration file. Defaults to .config.toml next to the executable.
    #[arg(long, short = 'c', value_name = "FILE")]
    pub config: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check FFmpeg support and optionally install it.
    Doctor(DoctorArgs),
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Install FFmpeg with the first supported system package manager.
    #[arg(long)]
    pub install: bool,
}

impl Cli {
    pub fn take_inputs(&mut self) -> Vec<PathBuf> {
        self.input.append(&mut self.dropped_input);
        std::mem::take(&mut self.input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_option_accepts_multiple_paths_with_spaces() {
        let cli = Cli::try_parse_from([
            "w4dj",
            "--input",
            "folder with spaces",
            "song with spaces.ncm",
            "--mode",
            "mp3",
        ])
        .unwrap();
        assert_eq!(cli.input.len(), 2);
        assert_eq!(cli.mode, Some(Mode::Mp3));
    }

    #[test]
    fn positional_inputs_support_multi_file_drag_and_drop() {
        let cli = Cli::try_parse_from(["w4dj", "first song.ncm", "second folder"]).unwrap();
        assert_eq!(cli.dropped_input.len(), 2);
    }

    #[test]
    fn doctor_is_parsed_before_positional_inputs() {
        let cli = Cli::try_parse_from(["w4dj", "doctor", "--install"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Doctor(DoctorArgs { install: true }))
        ));
        assert!(cli.dropped_input.is_empty());
    }
}
