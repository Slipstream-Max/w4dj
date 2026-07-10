use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::cli::Cli;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[serde(alias = "default")]
    #[value(alias = "default")]
    Original,
    #[serde(alias = "legacy")]
    #[value(alias = "legacy")]
    Mp3,
    Wav,
}

impl Mode {
    pub fn profile(self) -> &'static str {
        match self {
            Self::Original => "original-v1",
            Self::Mp3 => "mp3-q2-v1",
            Self::Wav => "wav-pcm16-v1",
        }
    }

    pub fn extension(self, source_format: &str) -> &str {
        match self {
            Self::Original => source_format,
            Self::Mp3 => "mp3",
            Self::Wav => "wav",
        }
    }

    pub fn needs_ffmpeg(self) -> bool {
        !matches!(self, Self::Original)
    }
}

#[derive(Debug)]
pub struct Config {
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    pub mode: Mode,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default, alias = "input", alias = "source")]
    inputs: Option<OneOrManyPaths>,
    #[serde(alias = "destination")]
    output: Option<PathBuf>,
    mode: Option<Mode>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrManyPaths {
    One(PathBuf),
    Many(Vec<PathBuf>),
}

impl OneOrManyPaths {
    fn into_vec(self) -> Vec<PathBuf> {
        match self {
            Self::One(path) => vec![path],
            Self::Many(paths) => paths,
        }
    }
}

impl Config {
    pub fn resolve(mut cli: Cli) -> Result<Self> {
        let exe = env::current_exe().context("failed to locate the w4dj executable")?;
        let exe_dir = exe
            .parent()
            .context("the w4dj executable has no parent directory")?
            .to_path_buf();
        let cwd = env::current_dir().context("failed to read the current directory")?;

        let explicit_config = cli.config.is_some();
        let config_path = cli
            .config
            .take()
            .map(|path| absolutize(&cwd, path))
            .unwrap_or_else(|| default_config_path(&exe_dir, &cwd));
        let file_config = load_file_config(&config_path, explicit_config)?;
        let config_dir = config_path.parent().unwrap_or(&exe_dir);

        let cli_inputs = cli.take_inputs();
        let raw_inputs: Vec<PathBuf> = if cli_inputs.is_empty() {
            file_config
                .inputs
                .map(OneOrManyPaths::into_vec)
                .unwrap_or_default()
                .into_iter()
                .map(|path| absolutize(config_dir, path))
                .collect()
        } else {
            cli_inputs
                .into_iter()
                .map(|path| absolutize(&cwd, path))
                .collect()
        };

        if raw_inputs.is_empty() {
            bail!(
                "no input was provided; use --input, drop paths onto w4dj, or configure {}",
                config_path.display()
            );
        }

        let output = cli
            .output
            .take()
            .map(|path| absolutize(&cwd, path))
            .or_else(|| file_config.output.map(|path| absolutize(config_dir, path)))
            .unwrap_or_else(|| exe_dir.join("w4djdump"));
        fs::create_dir_all(&output)
            .with_context(|| format!("failed to create output directory {}", output.display()))?;
        let output = fs::canonicalize(&output)
            .with_context(|| format!("failed to resolve output directory {}", output.display()))?;

        let mut seen = HashSet::new();
        let mut inputs = Vec::new();
        for input in raw_inputs {
            if !input.exists() {
                bail!("input does not exist: {}", input.display());
            }
            let input = fs::canonicalize(&input)
                .with_context(|| format!("failed to resolve input {}", input.display()))?;
            if seen.insert(input.clone()) {
                inputs.push(input);
            }
        }

        Ok(Self {
            inputs,
            output,
            mode: cli.mode.or(file_config.mode).unwrap_or(Mode::Original),
        })
    }
}

fn load_file_config(path: &Path, required: bool) -> Result<FileConfig> {
    if !path.exists() {
        if required {
            bail!("configuration file does not exist: {}", path.display());
        }
        return Ok(FileConfig::default());
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read configuration file {}", path.display()))?;
    toml::from_str(&content).with_context(|| {
        format!(
            "failed to parse configuration file {}. Windows paths can use TOML literal strings \
             such as 'C:\\Music', or escaped basic strings such as \"C:\\\\Music\"",
            path.display()
        )
    })
}

fn absolutize(base: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn default_config_path(exe_dir: &Path, cwd: &Path) -> PathBuf {
    let beside_executable = exe_dir.join(".config.toml");
    if beside_executable.exists() {
        beside_executable
    } else {
        cwd.join(".config.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_profiles_include_encoder_versions() {
        assert_eq!(Mode::Original.profile(), "original-v1");
        assert_eq!(Mode::Mp3.profile(), "mp3-q2-v1");
        assert_eq!(Mode::Wav.profile(), "wav-pcm16-v1");
    }

    #[test]
    fn relative_paths_use_their_origin_directory() {
        assert_eq!(
            absolutize(Path::new("C:/config"), PathBuf::from("music")),
            PathBuf::from("C:/config/music")
        );
    }

    #[test]
    fn config_paths_support_windows_unix_and_toml_escapes() -> Result<()> {
        let long_windows_path = format!(r"\\?\C:\Music\{}", "a".repeat(280));
        let document = format!(
            r#"
inputs = [
    'C:\Users\listener\Cloud Music',
    '\\server\share\Cloud Music',
    '{long_windows_path}',
    "D:\\escaped\\Cloud Music",
    "/home/listener/Music",
    "/Users/listener/Music",
    "/tmp/\u97f3\u4e50",
]
output = 'D:\w4djdump'
mode = "original"
"#
        );

        let config: FileConfig = toml::from_str(&document)?;
        let inputs = config.inputs.context("missing test inputs")?.into_vec();

        assert_eq!(inputs[0], PathBuf::from(r"C:\Users\listener\Cloud Music"));
        assert_eq!(inputs[1], PathBuf::from(r"\\server\share\Cloud Music"));
        assert_eq!(inputs[2], PathBuf::from(long_windows_path));
        assert_eq!(inputs[3], PathBuf::from(r"D:\escaped\Cloud Music"));
        assert_eq!(inputs[4], PathBuf::from("/home/listener/Music"));
        assert_eq!(inputs[5], PathBuf::from("/Users/listener/Music"));
        assert_eq!(inputs[6], PathBuf::from("/tmp/\u{97f3}\u{4e50}"));
        assert_eq!(config.output, Some(PathBuf::from(r"D:\w4djdump")));
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn resolves_windows_extended_length_paths() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let extended_root = PathBuf::from(format!(r"\\?\{}", workspace.path().display()));
        let long_root = extended_root
            .join("input".repeat(24))
            .join("library".repeat(18));
        let input = long_root.join("Cloud Music");
        let output = long_root.join("w4djdump");
        fs::create_dir_all(&input)?;
        assert!(input.as_os_str().len() > 260);

        let config_path = workspace.path().join("long-paths.toml");
        fs::write(
            &config_path,
            format!(
                "inputs = ['{}']\noutput = '{}'\nmode = 'original'\n",
                input.display(),
                output.display()
            ),
        )?;

        let resolved = Config::resolve(Cli {
            command: None,
            input: Vec::new(),
            dropped_input: Vec::new(),
            output: None,
            mode: None,
            config: Some(config_path),
        })?;

        assert_eq!(resolved.inputs, vec![fs::canonicalize(&input)?]);
        assert_eq!(resolved.output, fs::canonicalize(&output)?);
        Ok(())
    }

    #[test]
    fn executable_config_takes_precedence_over_working_directory() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let executable = workspace.path().join("bin");
        let working = workspace.path().join("work");
        fs::create_dir_all(&executable)?;
        fs::create_dir_all(&working)?;
        fs::write(executable.join(".config.toml"), "mode = 'original'")?;
        assert_eq!(
            default_config_path(&executable, &working),
            executable.join(".config.toml")
        );
        Ok(())
    }
}
