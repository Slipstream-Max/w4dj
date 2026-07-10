use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

use crate::cli::DoctorArgs;

pub fn run(args: DoctorArgs) -> Result<()> {
    println!("W4DJ doctor");
    println!("  system : {} {}", env::consts::OS, env::consts::ARCH);

    if let Some(path) = find_ffmpeg() {
        let report = verify_ffmpeg(&path)?;
        print_report(&path, &report);
        println!("  status : ready");
        return Ok(());
    }

    if let Some(path) = find_any_ffmpeg() {
        match verify_ffmpeg(&path) {
            Ok(report) => print_report(&path, &report),
            Err(error) => eprintln!("  invalid: {} ({error:#})", path.display()),
        }
        if !args.install {
            bail!("FFmpeg is missing an encoder required by W4DJ; run `w4dj doctor --install`");
        }
    } else if !args.install {
        bail!("FFmpeg was not found; run `w4dj doctor --install`");
    }

    install_ffmpeg()?;
    let path = find_ffmpeg().context(
        "the package manager completed but FFmpeg is not visible yet; reopen the terminal and run `w4dj doctor`",
    )?;
    let report = verify_ffmpeg(&path)?;
    print_report(&path, &report);
    if !report.is_usable() {
        bail!("the installed FFmpeg build does not provide all encoders required by W4DJ");
    }
    println!("  status : installed and ready");
    Ok(())
}

pub fn find_ffmpeg() -> Option<PathBuf> {
    ffmpeg_candidates()
        .into_iter()
        .find(|path| verify_ffmpeg(path).is_ok_and(|report| report.is_usable()))
}

fn find_any_ffmpeg() -> Option<PathBuf> {
    ffmpeg_candidates()
        .into_iter()
        .find(|path| version_command(path).is_ok_and(|output| output.status.success()))
}

#[derive(Debug)]
struct DoctorReport {
    version: String,
    libmp3lame: bool,
    pcm_s16le: bool,
}

impl DoctorReport {
    fn is_usable(&self) -> bool {
        self.libmp3lame && self.pcm_s16le
    }
}

fn verify_ffmpeg(path: &Path) -> Result<DoctorReport> {
    let version_output =
        version_command(path).with_context(|| format!("failed to execute {}", path.display()))?;
    ensure_success(path, "version", &version_output)?;
    let version = String::from_utf8_lossy(&version_output.stdout)
        .lines()
        .next()
        .unwrap_or("unknown FFmpeg version")
        .trim()
        .to_string();

    let encoders = Command::new(path)
        .arg("-hide_banner")
        .arg("-encoders")
        .output()
        .with_context(|| format!("failed to query encoders from {}", path.display()))?;
    ensure_success(path, "encoder query", &encoders)?;
    let listing = String::from_utf8_lossy(&encoders.stdout);
    Ok(DoctorReport {
        version,
        libmp3lame: encoder_is_present(&listing, "libmp3lame"),
        pcm_s16le: encoder_is_present(&listing, "pcm_s16le"),
    })
}

fn version_command(path: &Path) -> std::io::Result<Output> {
    Command::new(path).arg("-version").output()
}

fn ensure_success(path: &Path, action: &str, output: &Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "FFmpeg {} failed for {}: {}",
        action,
        path.display(),
        stderr.trim()
    )
}

fn encoder_is_present(listing: &str, encoder: &str) -> bool {
    listing.lines().any(|line| {
        line.split_whitespace()
            .nth(1)
            .is_some_and(|name| name == encoder)
    })
}

fn print_report(path: &Path, report: &DoctorReport) {
    println!("  ffmpeg : {}", path.display());
    println!("  version: {}", report.version);
    println!(
        "  mp3    : {} (libmp3lame)",
        availability(report.libmp3lame)
    );
    println!("  wav    : {} (pcm_s16le)", availability(report.pcm_s16le));
}

fn availability(available: bool) -> &'static str {
    if available { "available" } else { "missing" }
}

fn ffmpeg_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = env::current_exe()
        && let Some(parent) = exe.parent()
    {
        candidates.push(parent.join("ffmpeg.exe"));
        candidates.push(parent.join("ffmpeg"));
    }
    if let Ok(path) = which::which("ffmpeg") {
        candidates.push(path);
    }

    match env::consts::OS {
        "windows" => {
            if let Some(local) = env::var_os("LOCALAPPDATA") {
                candidates.push(PathBuf::from(local).join("Microsoft/WinGet/Links/ffmpeg.exe"));
            }
            if let Some(program_data) = env::var_os("ProgramData") {
                candidates.push(PathBuf::from(program_data).join("chocolatey/bin/ffmpeg.exe"));
            }
            if let Some(home) = env::var_os("USERPROFILE") {
                candidates.push(PathBuf::from(home).join("scoop/shims/ffmpeg.exe"));
            }
        }
        "macos" => {
            candidates.push(PathBuf::from("/opt/homebrew/bin/ffmpeg"));
            candidates.push(PathBuf::from("/usr/local/bin/ffmpeg"));
            candidates.push(PathBuf::from("/opt/local/bin/ffmpeg"));
        }
        _ => {
            candidates.push(PathBuf::from("/usr/bin/ffmpeg"));
            candidates.push(PathBuf::from("/usr/local/bin/ffmpeg"));
            candidates.push(PathBuf::from("/bin/ffmpeg"));
        }
    }

    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|path| path.is_file() && seen.insert(path.clone()))
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PackageManager {
    Winget,
    Scoop,
    Chocolatey,
    Homebrew,
    MacPorts,
    Apt,
    Dnf,
    Pacman,
    Zypper,
    Apk,
}

fn install_ffmpeg() -> Result<()> {
    let manager = select_package_manager(env::consts::OS, |name| find_program(name).is_some())
        .with_context(|| unsupported_manager_message(env::consts::OS))?;
    println!("  install: {}", manager.name());
    for step in manager.steps()? {
        println!(
            "  running: {} {}",
            step.program.display(),
            step.args.join(" ")
        );
        let status = if step.elevated {
            if let Some(sudo) = find_program("sudo") {
                Command::new(sudo)
                    .arg(&step.program)
                    .args(&step.args)
                    .status()
            } else {
                Command::new(&step.program).args(&step.args).status()
            }
        } else {
            Command::new(&step.program).args(&step.args).status()
        }
        .with_context(|| format!("failed to start {}", step.program.display()))?;
        if !status.success() {
            bail!(
                "{} failed with status {}; FFmpeg was not installed",
                manager.name(),
                status
            );
        }
    }
    Ok(())
}

fn select_package_manager<F>(os: &str, available: F) -> Option<PackageManager>
where
    F: Fn(&str) -> bool,
{
    let choices: &[PackageManager] = match os {
        "windows" => &[
            PackageManager::Winget,
            PackageManager::Scoop,
            PackageManager::Chocolatey,
        ],
        "macos" => &[PackageManager::Homebrew, PackageManager::MacPorts],
        "linux" => &[
            PackageManager::Apt,
            PackageManager::Dnf,
            PackageManager::Pacman,
            PackageManager::Zypper,
            PackageManager::Apk,
        ],
        _ => &[],
    };
    choices
        .iter()
        .copied()
        .find(|manager| available(manager.program_name()))
}

impl PackageManager {
    fn name(self) -> &'static str {
        match self {
            Self::Winget => "winget",
            Self::Scoop => "Scoop",
            Self::Chocolatey => "Chocolatey",
            Self::Homebrew => "Homebrew",
            Self::MacPorts => "MacPorts",
            Self::Apt => "APT",
            Self::Dnf => "DNF",
            Self::Pacman => "pacman",
            Self::Zypper => "Zypper",
            Self::Apk => "APK",
        }
    }

    fn program_name(self) -> &'static str {
        match self {
            Self::Winget => "winget",
            Self::Scoop => "scoop",
            Self::Chocolatey => "choco",
            Self::Homebrew => "brew",
            Self::MacPorts => "port",
            Self::Apt => "apt-get",
            Self::Dnf => "dnf",
            Self::Pacman => "pacman",
            Self::Zypper => "zypper",
            Self::Apk => "apk",
        }
    }

    fn steps(self) -> Result<Vec<InstallStep>> {
        let program = find_program(self.program_name())
            .with_context(|| format!("{} disappeared from PATH", self.program_name()))?;
        let step = |args: &[&str], elevated| InstallStep {
            program: program.clone(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            elevated,
        };

        Ok(match self {
            Self::Winget => vec![step(
                &[
                    "install",
                    "--id",
                    "Gyan.FFmpeg",
                    "--exact",
                    "--accept-package-agreements",
                    "--accept-source-agreements",
                ],
                false,
            )],
            Self::Scoop => vec![step(&["install", "ffmpeg"], false)],
            Self::Chocolatey => vec![step(&["install", "ffmpeg", "-y"], false)],
            Self::Homebrew => vec![step(&["install", "ffmpeg"], false)],
            Self::MacPorts => vec![step(&["install", "ffmpeg"], true)],
            Self::Apt => vec![
                step(&["update"], true),
                step(&["install", "-y", "ffmpeg"], true),
            ],
            Self::Dnf => vec![step(&["install", "-y", "ffmpeg"], true)],
            Self::Pacman => vec![step(&["-S", "--needed", "--noconfirm", "ffmpeg"], true)],
            Self::Zypper => vec![step(&["--non-interactive", "install", "ffmpeg"], true)],
            Self::Apk => vec![step(&["add", "ffmpeg"], true)],
        })
    }
}

struct InstallStep {
    program: PathBuf,
    args: Vec<String>,
    elevated: bool,
}

fn find_program(name: &str) -> Option<PathBuf> {
    which::which(name).ok().or_else(|| match name {
        "brew" => ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file()),
        "port" => {
            let path = PathBuf::from("/opt/local/bin/port");
            path.is_file().then_some(path)
        }
        _ => None,
    })
}

fn unsupported_manager_message(os: &str) -> String {
    match os {
        "windows" => "no supported package manager found; install winget, Scoop, or Chocolatey"
            .to_string(),
        "macos" => "no supported package manager found; install Homebrew or MacPorts".to_string(),
        "linux" => {
            "no supported package manager found; supported managers are apt, dnf, pacman, zypper, and apk"
                .to_string()
        }
        other => format!("automatic FFmpeg installation is not supported on {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_listing_requires_exact_encoder_names() {
        let listing = " A....D libmp3lame libmp3lame MP3\n A....D pcm_s16le PCM signed 16-bit";
        assert!(encoder_is_present(listing, "libmp3lame"));
        assert!(encoder_is_present(listing, "pcm_s16le"));
        assert!(!encoder_is_present(listing, "mp3"));
    }

    #[test]
    fn package_manager_selection_follows_platform_priority() {
        let manager = select_package_manager("windows", |name| matches!(name, "winget" | "choco"));
        assert_eq!(manager, Some(PackageManager::Winget));

        let manager = select_package_manager("linux", |name| matches!(name, "pacman" | "apk"));
        assert_eq!(manager, Some(PackageManager::Pacman));
    }
}
