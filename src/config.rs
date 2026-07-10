use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use directories::{BaseDirs, UserDirs};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::cli::Cli;

pub const DEFAULT_WINDOW_OPACITY: f32 = 0.84;
pub const MIN_WINDOW_OPACITY: f32 = 0.55;
pub const MAX_WINDOW_OPACITY: f32 = 1.0;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GuiTheme {
    Light,
    Dark,
    #[default]
    System,
}

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

#[derive(Clone, Debug)]
pub struct Config {
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    pub mode: Mode,
}

#[derive(Clone, Debug)]
pub struct EditableConfig {
    pub path: PathBuf,
    pub inputs: Vec<PathBuf>,
    pub output: Option<PathBuf>,
    pub mode: Mode,
    pub theme: GuiTheme,
    pub window_opacity: f32,
    default_output: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default, alias = "input", alias = "source")]
    inputs: Option<OneOrManyPaths>,
    #[serde(alias = "destination")]
    output: Option<PathBuf>,
    mode: Option<Mode>,
    #[serde(default)]
    gui: FileGuiConfig,
}

#[derive(Debug, Default, Deserialize)]
struct FileGuiConfig {
    theme: Option<GuiTheme>,
    opacity: Option<f32>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrManyPaths {
    One(PathBuf),
    Many(Vec<PathBuf>),
}

#[derive(Serialize)]
struct WritableFileConfig<'a> {
    inputs: &'a [PathBuf],
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<&'a Path>,
    mode: Mode,
    gui: WritableGuiConfig,
}

#[derive(Serialize)]
struct WritableGuiConfig {
    theme: GuiTheme,
    opacity: f64,
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
        let (exe_dir, cwd) = application_directories()?;
        let (config_path, explicit_config) = resolve_config_path(cli.config.take(), &cwd)?;
        let file_config = load_file_config(&config_path, explicit_config)?;
        let config_dir = config_path.parent().unwrap_or(&exe_dir);
        let default_output = default_output_path()?;

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

        let output = cli
            .output
            .take()
            .map(|path| absolutize(&cwd, path))
            .or_else(|| file_config.output.map(|path| absolutize(config_dir, path)))
            .unwrap_or(default_output);

        Self::from_paths(
            raw_inputs,
            output,
            cli.mode.or(file_config.mode).unwrap_or(Mode::Original),
        )
        .with_context(|| format!("configuration resolved from {}", config_path.display()))
    }

    pub fn from_paths(inputs: Vec<PathBuf>, output: PathBuf, mode: Mode) -> Result<Self> {
        if inputs.is_empty() {
            bail!("no input was provided; add or drop at least one file or directory");
        }

        fs::create_dir_all(&output)
            .with_context(|| format!("failed to create output directory {}", output.display()))?;
        let output = fs::canonicalize(&output)
            .with_context(|| format!("failed to resolve output directory {}", output.display()))?;

        let mut seen = HashSet::new();
        let mut normalized_inputs = Vec::new();
        for input in inputs {
            if !input.exists() {
                bail!("input does not exist: {}", input.display());
            }
            let input = fs::canonicalize(&input)
                .with_context(|| format!("failed to resolve input {}", input.display()))?;
            if seen.insert(input.clone()) {
                normalized_inputs.push(input);
            }
        }

        Ok(Self {
            inputs: normalized_inputs,
            output,
            mode,
        })
    }
}

impl EditableConfig {
    pub fn empty_default() -> Result<Self> {
        let path = default_config_path()?;
        Ok(Self {
            path,
            inputs: Vec::new(),
            output: None,
            mode: Mode::Original,
            theme: GuiTheme::System,
            window_opacity: DEFAULT_WINDOW_OPACITY,
            default_output: default_output_path()?,
        })
    }

    pub fn load_default() -> Result<Self> {
        let mut editable = Self::empty_default()?;
        let create_default = !editable.path.exists();
        let file_config = load_file_config(&editable.path, false)?;
        editable.inputs = file_config
            .inputs
            .map(OneOrManyPaths::into_vec)
            .unwrap_or_default();
        editable.output = file_config.output;
        editable.mode = file_config.mode.unwrap_or(Mode::Original);
        editable.theme = file_config.gui.theme.unwrap_or_default();
        editable.window_opacity =
            normalize_window_opacity(file_config.gui.opacity.unwrap_or(DEFAULT_WINDOW_OPACITY));
        if create_default {
            editable.save()?;
        }
        Ok(editable)
    }

    pub fn resolved_inputs(&self) -> Vec<PathBuf> {
        let base = self.path.parent().unwrap_or_else(|| Path::new("."));
        self.inputs
            .iter()
            .cloned()
            .map(|path| absolutize(base, path))
            .collect()
    }

    pub fn resolved_output(&self) -> PathBuf {
        let base = self.path.parent().unwrap_or_else(|| Path::new("."));
        self.output
            .clone()
            .map(|path| absolutize(base, path))
            .unwrap_or_else(|| self.default_output.clone())
    }

    pub fn runtime_config(&self, session_inputs: &[PathBuf]) -> Result<Config> {
        let mut inputs = self.resolved_inputs();
        inputs.extend(session_inputs.iter().cloned());
        Config::from_paths(inputs, self.resolved_output(), self.mode)
    }

    pub fn save(&self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("configuration file has no parent directory")?;
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create configuration directory {}",
                parent.display()
            )
        })?;
        let document = toml::to_string_pretty(&WritableFileConfig {
            inputs: &self.inputs,
            output: self.output.as_deref(),
            mode: self.mode,
            gui: WritableGuiConfig {
                theme: self.theme,
                opacity: config_window_opacity(self.window_opacity),
            },
        })
        .context("failed to serialize configuration")?;
        let mut temporary = NamedTempFile::new_in(parent).with_context(|| {
            format!(
                "failed to create configuration temporary file in {}",
                parent.display()
            )
        })?;
        temporary
            .write_all(document.as_bytes())
            .context("failed to write configuration temporary file")?;
        temporary
            .as_file()
            .sync_all()
            .context("failed to sync configuration temporary file")?;
        temporary
            .persist(&self.path)
            .map_err(|error| error.error)
            .with_context(|| format!("failed to publish configuration {}", self.path.display()))?;
        Ok(())
    }
}

pub fn normalize_window_opacity(opacity: f32) -> f32 {
    if opacity.is_finite() {
        opacity.clamp(MIN_WINDOW_OPACITY, MAX_WINDOW_OPACITY)
    } else {
        DEFAULT_WINDOW_OPACITY
    }
}

fn config_window_opacity(opacity: f32) -> f64 {
    let opacity = f64::from(normalize_window_opacity(opacity));
    (opacity * 100.0).round() / 100.0
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

fn resolve_config_path(explicit: Option<PathBuf>, cwd: &Path) -> Result<(PathBuf, bool)> {
    match explicit {
        Some(path) => Ok((absolutize(cwd, path), true)),
        None => Ok((default_config_path()?, false)),
    }
}

fn default_config_path() -> Result<PathBuf> {
    let base_dirs = BaseDirs::new().context("failed to locate the platform config directory")?;
    Ok(config_path_in(base_dirs.config_dir()))
}

fn config_path_in(config_root: &Path) -> PathBuf {
    config_root.join("w4dj").join("config.toml")
}

fn default_output_path() -> Result<PathBuf> {
    let user_dirs = UserDirs::new().context("failed to locate the platform user directories")?;
    let music_dir = user_dirs
        .audio_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| user_dirs.home_dir().join("Music"));
    Ok(output_path_in(&music_dir))
}

fn output_path_in(music_root: &Path) -> PathBuf {
    music_root.join("w4djdump")
}

fn application_directories() -> Result<(PathBuf, PathBuf)> {
    let exe = env::current_exe().context("failed to locate the w4dj executable")?;
    let exe_dir = exe
        .parent()
        .context("the w4dj executable has no parent directory")?
        .to_path_buf();
    let cwd = env::current_dir().context("failed to read the current directory")?;
    Ok((exe_dir, cwd))
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
    fn explicit_config_path_takes_precedence() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let (path, explicit) =
            resolve_config_path(Some(PathBuf::from("custom.toml")), workspace.path())?;
        assert!(explicit);
        assert_eq!(path, workspace.path().join("custom.toml"));
        Ok(())
    }

    #[test]
    fn standard_config_path_uses_w4dj_directory() {
        let config_root = Path::new("platform-config");
        assert_eq!(
            config_path_in(config_root),
            config_root.join("w4dj").join("config.toml")
        );
    }

    #[test]
    fn standard_output_path_uses_w4djdump_in_music_directory() {
        let music_root = Path::new("platform-music");
        assert_eq!(output_path_in(music_root), music_root.join("w4djdump"));
    }

    #[test]
    fn editable_config_atomically_replaces_an_existing_file() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let path = workspace.path().join(".config.toml");
        fs::write(&path, "mode = 'original'\n")?;
        let editable = EditableConfig {
            path: path.clone(),
            inputs: vec![PathBuf::from(r"C:\Cloud Music")],
            output: Some(PathBuf::from(r"D:\Library")),
            mode: Mode::Mp3,
            theme: GuiTheme::Light,
            window_opacity: 0.72,
            default_output: workspace.path().join("w4djdump"),
        };

        editable.save()?;
        let loaded = load_file_config(&path, true)?;
        assert_eq!(
            loaded.inputs.context("missing saved inputs")?.into_vec(),
            editable.inputs
        );
        assert_eq!(loaded.output, editable.output);
        assert_eq!(loaded.mode, Some(Mode::Mp3));
        assert_eq!(loaded.gui.theme, Some(GuiTheme::Light));
        assert_eq!(loaded.gui.opacity, Some(0.72));
        Ok(())
    }

    #[test]
    fn gui_opacity_is_clamped_and_non_finite_values_use_the_default() {
        assert_eq!(normalize_window_opacity(0.1), MIN_WINDOW_OPACITY);
        assert_eq!(normalize_window_opacity(1.5), MAX_WINDOW_OPACITY);
        assert_eq!(normalize_window_opacity(f32::NAN), DEFAULT_WINDOW_OPACITY);
    }

    #[test]
    fn saved_gui_opacity_has_at_most_two_decimal_places() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let path = workspace.path().join(".config.toml");
        let editable = EditableConfig {
            path: path.clone(),
            inputs: Vec::new(),
            output: None,
            mode: Mode::Original,
            theme: GuiTheme::System,
            window_opacity: 0.6,
            default_output: workspace.path().join("w4djdump"),
        };

        editable.save()?;

        let document = fs::read_to_string(path)?;
        assert!(document.contains("opacity = 0.6\n"), "{document}");
        Ok(())
    }

    #[test]
    fn gui_theme_accepts_light_dark_and_system() -> Result<()> {
        for (name, expected) in [
            ("light", GuiTheme::Light),
            ("dark", GuiTheme::Dark),
            ("system", GuiTheme::System),
        ] {
            let document = format!("[gui]\ntheme = '{name}'\n");
            let config: FileConfig = toml::from_str(&document)?;
            assert_eq!(config.gui.theme, Some(expected));
        }
        Ok(())
    }
}
