use crate::config::Mode;
use crate::metadata::{FlacMetadata, Metadata, Mp3Metadata};
use ncmdump::Ncmdump;
use rayon::prelude::*;
use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, Error, ErrorKind, Write};
use std::path::Path;
use std::process::Command;
use which;

pub fn find_ffmpeg() -> Option<String> {
    if let Ok(exe_dir) = env::current_exe() {
        if let Some(parent) = exe_dir.parent() {
            let local_ffmpeg = parent.join("ffmpeg");
            if local_ffmpeg.exists() {
                return Some(local_ffmpeg.to_string_lossy().into_owned());
            }
        }
    }

    if let Ok(path) = which::which("ffmpeg") {
        return Some(path.to_string_lossy().into_owned());
    }

    None
}

pub fn get_music_dict(folder: &str) -> HashMap<String, (String, String)> {
    walkdir::WalkDir::new(folder)
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
    mode: &Mode,
) -> HashMap<&'a String, &'a (String, String)> {
    wf_dict
        .iter()
        .filter(|(name, wf_info)| match mode {
            Mode::Legacy => !sf_dict.contains_key(*name),
            Mode::Default => {
                if let Some(sf_info) = sf_dict.get(*name) {
                    if let (Ok(size1), Ok(size2)) =
                        (wf_info.0.parse::<u64>(), sf_info.0.parse::<u64>())
                    {
                        let max_size = size1.max(size2) as f64;
                        if max_size > 0.0 {
                            let diff = (size1 as f64 - size2 as f64).abs();
                            return (diff / max_size) >= 0.1;
                        }
                        return size1 != size2;
                    }
                    true
                } else {
                    true
                }
            }
        })
        .collect()
}

pub fn sync_music_library(
    new_songs: &HashMap<&String, &(String, String)>,
    dest_folder: &str,
    mode: &Mode,
) -> io::Result<()> {
    if new_songs.is_empty() {
        return Ok(());
    }

    let bar = indicatif::ProgressBar::new(new_songs.len() as u64);
    bar.set_style(
        indicatif::ProgressStyle::with_template(
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
    let ffmpeg_path = find_ffmpeg().unwrap_or_else(|| {
        eprintln!("FFmpeg not found. Please ensure it is installed and in your PATH.");
        std::process::exit(1);
    });

    let status = Command::new(&ffmpeg_path)
        .arg("-i")
        .arg(src_path)
        .arg("-loglevel")
        .arg("quiet")
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
            format!("NCM 解析错误 {}: {}", name_stem, e),
        )
    })?;
    // 提取原始音频数据
    let music_data = ncm.get_data().map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("NCM 数据提取错误 {}: {}", name_stem, e),
        )
    })?;
    // 提取专辑封面（关键修改点）
    let image_data = ncm.get_image().map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("NCM 封面提取错误 {}: {}", name_stem, e),
        )
    })?;
    // 提取歌曲元数据
    let ncm_metadata = ncm.get_info().map_err(|e| {
        Error::new(
            ErrorKind::InvalidData,
            format!("NCM 元数据错误 {}: {}", name_stem, e),
        )
    })?;
    // 确定输出格式（保持你的逻辑）
    let file_format = if ncm_metadata.format.is_empty() {
        "flac".to_string()
    } else {
        ncm_metadata.format.to_lowercase()
    };
    // 创建目标文件路径
    let temp_file_name = format!("{}.{}", name_stem, file_format);
    let temp_path = Path::new(dest_folder).join(&temp_file_name);
    // 确保目录存在
    if let Some(parent) = temp_path.parent() {
        fs::create_dir_all(parent)?;
    }
    // ===== 关键修改：注入元数据 =====
    let final_data = match file_format.as_str() {
        "mp3" => {
            // 创建MP3元数据注入器
            Mp3Metadata::new(&ncm_metadata, &image_data, &music_data)
                .inject_metadata(music_data.clone()) // 注入封面和元数据
                .map_err(|e| {
                    Error::new(
                        ErrorKind::InvalidData,
                        format!("MP3元数据注入失败 {}: {}", name_stem, e),
                    )
                })?
        }
        "flac" => {
            // 创建FLAC元数据注入器
            FlacMetadata::new(&ncm_metadata, &image_data, &music_data)
                .inject_metadata(music_data.clone()) // 注入封面和元数据
                .map_err(|e| {
                    Error::new(
                        ErrorKind::InvalidData,
                        format!("FLAC元数据注入失败 {}: {}", name_stem, e),
                    )
                })?
        }
        _ => music_data, // 其他格式保持原始数据
    };
    // 写入处理后的数据（包含封面）
    let mut temp_file = File::create(&temp_path)?;
    temp_file.write_all(&final_data)?;

    match (mode, file_format.as_str()) {
        (Mode::Legacy, "flac") => {
            convert_flac_to_mp3(&temp_path, dest_folder, name_stem)?;
            fs::remove_file(&temp_path)?;
        }
        _ => {}
    }

    Ok(())
}
