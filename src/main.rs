use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use ncmdump::Ncmdump;
use rayon::prelude::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Error, ErrorKind, Write};
use std::path::Path;
use std::process::Command;
use toml;
use walkdir::WalkDir;
use text_to_ascii_art::to_art;

#[derive(Parser)]
#[command(
    name = "w4dj",
    version = "0.1.0",
    author = "slipstream",
    about = "网易云音乐曲库同步器"
)]
struct Cmd {
    #[arg(long, short, default_value = "config.toml")]
    config: Option<String>,
}

#[derive(Debug, Deserialize)]
enum Mode {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "legacy")]
    Legacy,
}

#[derive(Debug, Deserialize)]
struct Config {
    source: String,
    destination: String,
    mode: Mode,
}

fn get_music_dict(folder: &str) -> HashMap<String, (String, String)> {
    WalkDir::new(folder)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_file()
                && e.path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map_or(false, |ext_str| {
                        matches!(ext_str.to_lowercase().as_str(), "mp3" | "flac" | "ncm")
                    })
        })
        .map(|entry| {
            let path = entry.path().to_string_lossy().into_owned();
            let stem = entry
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();

            let size = entry
                .metadata()
                .map(|m| m.len().to_string())
                .unwrap_or_else(|_| "0".to_string());

            (stem, (size, path))
        })
        .collect()
}

pub fn compare_music_dicts<'a>(
    wf_dict: &'a HashMap<String, (String, String)>,
    sf_dict: &'a HashMap<String, (String, String)>,
) -> HashMap<&'a String, &'a (String, String)> {
    wf_dict
        .iter()
        .filter(|(name, wf_info)| {
            if let Some(sf_info) = sf_dict.get(*name) {
                // Both exist, compare sizes
                if let (Ok(size1), Ok(size2)) = (wf_info.0.parse::<u64>(), sf_info.0.parse::<u64>())
                {
                    let max_size = size1.max(size2) as f64;
                    if max_size > 0.0 {
                        let diff = (size1 as f64 - size2 as f64).abs();
                        return (diff / max_size) >= 0.05; // Keep if different enough
                    }
                    return size1 != size2; // If one size is 0, they are different unless both are 0
                }
                true // Parsing failed, assume different
            } else {
                true // Not in sf_dict, so it's new
            }
        })
        .collect()
}

fn sync_music_library(
    new_songs: &HashMap<&String, &(String, String)>,
    dest_folder: &str,
    mode: &Mode,
) -> io::Result<()> {
    if new_songs.is_empty() {
        return Ok(());
    }

    let bar = ProgressBar::new(new_songs.len() as u64);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})\n{msg}",
        )
        .unwrap(),
    );

    let results: Vec<io::Result<()>> = new_songs
        .par_iter()
        .map(|(&name, info)| {
            let src_path = Path::new(&info.1);
            let extension = src_path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
                .to_lowercase();

            let task_result = match extension.as_str() {
                "mp3" => {
                    bar.set_message(format!("Copying MP3: {}", name));
                    copy_file(src_path, dest_folder, name)
                }
                "flac" => {
                    bar.set_message(format!("Processing FLAC: {}", name));
                    match mode {
                        Mode::Default => copy_file(src_path, dest_folder, name),
                        Mode::Legacy => convert_flac_to_mp3(src_path, dest_folder, name),
                    }
                }
                "ncm" => {
                    bar.set_message(format!("Dumping NCM: {}", name));
                    process_ncm_file(src_path, dest_folder, name, mode)
                }
                _ => unreachable!(
                    "Invalid file extension '{}' for song '{}'. Filter failed.",
                    extension, name
                ),
            };
            bar.inc(1);
            task_result
        })
        .collect();

    // Find the first error, if any
    if let Some(err) = results.into_iter().find_map(Result::err) {
        bar.abandon_with_message(format!("Sync encountered errors. First error: {}", err));
        Err(err)
    } else {
        bar.finish_with_message("Sync processing complete.");
        Ok(())
    }
}

fn copy_file(src_path: &Path, dest_folder: &str, name_stem: &str) -> io::Result<()> {
    let file_name = src_path.file_name().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("Invalid source filename for: {}", name_stem),
        )
    })?;

    let dest_path = Path::new(dest_folder).join(file_name);

    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src_path, &dest_path).map(|_| ())
}

fn convert_flac_to_mp3(src_path: &Path, dest_folder: &str, name_stem: &str) -> io::Result<()> {
    let output_path = Path::new(dest_folder).join(format!("{}.mp3", name_stem));
    
    let status = Command::new("ffmpeg")
        .arg("-i")
        .arg(src_path)
        .arg("-q:a")
        .arg("0")
        .arg("-map_metadata")
        .arg("0")
        .arg("-id3v2_version")
        .arg("3")
        .arg(&output_path)
        .status()?;

    if !status.success() {
        return Err(Error::new(
            ErrorKind::Other,
            format!("FFmpeg conversion failed for {}", name_stem),
        ));
    }

    Ok(())
}

fn process_ncm_file(
    src_path: &Path,
    dest_folder: &str,
    name_stem: &str,
    mode: &Mode,
) -> io::Result<()> {
    let file = File::open(src_path)?;
    let mut ncm = Ncmdump::from_reader(file).map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("NCM parse error for {}: {}", name_stem, e),
        )
    })?;

    let music_data = ncm.get_data().map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("NCM data extraction error for {}: {}", name_stem, e),
        )
    })?;

    let ncm_metadata = ncm.get_info().map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("NCM metadata error for {}: {}", name_stem, e),
        )
    })?;

    let file_format = if ncm_metadata.format.is_empty() {
        "flac".to_string()
    } else {
        ncm_metadata.format.to_lowercase()
    };

    // First write the original format
    let temp_file_name = format!("{}.{}", name_stem, file_format);
    let temp_path = Path::new(dest_folder).join(&temp_file_name);

    if let Some(parent) = temp_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut temp_file = File::create(&temp_path)?;
    temp_file.write_all(&music_data)?;

    // Handle conversion if needed
    match (mode, file_format.as_str()) {
        (Mode::Legacy, "flac") => {
            // Convert FLAC to MP3
            let mp3_path = Path::new(dest_folder).join(format!("{}.mp3", name_stem));
            
            let status = Command::new("ffmpeg")
                .arg("-i")
                .arg(&temp_path)
                .arg("-q:a")
                .arg("0")
                .arg("-map_metadata")
                .arg("0")
                .arg("-id3v2_version")
                .arg("3")
                .arg(&mp3_path)
                .status()?;

            if !status.success() {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("FFmpeg conversion failed for {}", name_stem),
                ));
            }

            // Remove the temporary FLAC file
            fs::remove_file(&temp_path)?;
        }
        _ => {
            // In default mode or if not FLAC, keep the original format
        }
    }

    Ok(())
}

fn main() -> Result<(), Error> {
    match to_art("W4DJ".to_string(), "standard", 0, 2, 0) {
        Ok(string) => println!("{}", string),
        Err(err) => println!("Error: {}", err),
    }

    let cmd = Cmd::parse();
    let config_file_path = cmd.config.expect("Clap should provide default value");

    let config_content = fs::read_to_string(&config_file_path).map_err(|e| {
        Error::new(
            e.kind(),
            format!("Failed to read config '{}': {}", config_file_path, e),
        )
    })?;

    let config: Config = toml::from_str(&config_content).map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("Failed to parse TOML from '{}': {}", config_file_path, e),
        )
    })?;

    println!(
        "Config loaded: Source='{}', Destination='{}', Mode={:?}",
        config.source, config.destination, config.mode
    );

    let wf = &config.source;
    let sf = &config.destination;

    if !Path::new(wf).exists() {
        eprintln!("Source folder does not exist: {}", wf);
        return Err(Error::new(
            ErrorKind::NotFound,
            format!("Source folder not found: {}", wf),
        ));
    }

    if !Path::new(sf).exists() {
        println!("Destination folder '{}' does not exist, creating...", sf);
        fs::create_dir_all(sf)?;
    }

    println!("Scanning source folder: {}", wf);
    let wf_dict = get_music_dict(wf);
    println!("Found {} music files in source.", wf_dict.len());

    println!("Scanning destination folder: {}", sf);
    let sf_dict = get_music_dict(sf);
    println!("Found {} music files in destination.", sf_dict.len());

    let new_songs = compare_music_dicts(&wf_dict, &sf_dict);
    println!("Found {} new songs to sync.", new_songs.len());

    if !new_songs.is_empty() {
        sync_music_library(&new_songs, sf, &config.mode)?;
    }

    println!("Sync completed successfully.");
    Ok(())
}