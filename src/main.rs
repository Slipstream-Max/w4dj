use clap::Parser;
use std::{fs, io};
use std::fs::File;
use std::io::{Write, Error};
use std::path::Path;
use indicatif::{ProgressBar, ProgressStyle};
use toml;
use serde::{Deserialize};
use std::collections::{HashMap, HashSet};
use walkdir::{DirEntry, WalkDir};
use ncmdump::Ncmdump;
use rayon::prelude::*; // Import rayon prelude for parallel iterators

#[derive(Parser)]
#[command(name = "w4dj", version = "0.1.0", author = "slipstream", about = "网易云音乐曲库同步器")]
struct Cmd {
    #[arg(long,short,default_value="config.toml")]
    config: Option<String>
}

#[derive(Debug, Deserialize)]
struct Config {
    source: String,
    destination: String,
}

// This struct remains unused in the provided logic but is kept as is.
#[derive(Debug)]
pub struct NcmInfo {
    pub name: String,
    pub id: u64,
    pub album: String,
    pub artist: Vec<(String, u64)>,
    pub bitrate: u64,
    pub duration: u64,
    pub format: String,
    pub mv_id: Option<u64>,
    pub alias: Option<Vec<String>>,
}

fn get_music_dict(folder: &str) -> HashMap<String, HashMap<String, String>> {
    let mut music_dict = HashMap::new();
    
    for entry in WalkDir::new(folder)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(is_valid_music_file)
    {
        let path = entry.path();
        let stem = path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default() // Files without stems (e.g. ".rcfile") get an empty string key.
            .to_string();
        
        let size = entry.metadata()
            .map(|m| m.len().to_string())
            .unwrap_or_else(|_| "0".to_string());
        
        let full_path = path.to_string_lossy().into_owned();
        
        let mut file_info = HashMap::new();
        file_info.insert("size".to_string(), size);
        file_info.insert("path".to_string(), full_path);
        
        music_dict.insert(stem, file_info);
    }
    
    music_dict
}

fn is_valid_music_file(entry: &DirEntry) -> bool {
    if !entry.file_type().is_file() {
        return false;
    }
    
    entry.path().extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            let ext_lower = ext.to_lowercase();
            ext_lower == "mp3" || ext_lower == "flac" || ext_lower == "ncm"
        })
        .unwrap_or(false)
}

pub fn compare_music_dicts<'a>(
    wf_dict: &'a HashMap<String, HashMap<String, String>>,
    sf_dict: &'a HashMap<String, HashMap<String, String>>,
) -> HashMap<&'a String, &'a HashMap<String, String>> {
    let mut wf_keys: HashSet<_> = wf_dict.keys().collect();
    let sf_keys: HashSet<_> = sf_dict.keys().collect();
    
    let common_keys: HashSet<_> = wf_keys.intersection(&sf_keys).cloned().collect();
    
    for name in &common_keys {
        if let (Some(wf_info), Some(sf_info)) = (wf_dict.get(*name), sf_dict.get(*name)) {
            if let (Ok(size1), Ok(size2)) = (
                wf_info.get("size").unwrap().parse::<u64>(),
                sf_info.get("size").unwrap().parse::<u64>(),
            ) {
                let max_size = size1.max(size2) as f64;
                let diff = (size1 as f64 - size2 as f64).abs();
                
                if max_size > 0.0 && (diff / max_size) < 0.03 {
                    wf_keys.remove(name);
                }
            }
        }
    }
    
    wf_keys.iter()
        .filter_map(|name| wf_dict.get_key_value(*name))
        .collect()
}

pub fn sync_music_library(
    new_songs: &HashMap<&String, &HashMap<String, String>>,
    dest_folder: &str,
) -> io::Result<()> {
    let num_songs = new_songs.len();
    if num_songs == 0 {
        // No explicit message here in original, can be added if desired.
        // e.g. println!("No new songs to sync.");
        return Ok(());
    }

    let bar = ProgressBar::new(num_songs as u64);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})\n{msg}",
        )
        .unwrap()
        .progress_chars("#>-"),
    );
    // Optional: Set an initial message
    // bar.set_message("Processing files...");

    // Use par_iter from rayon to process songs in parallel
    // Collect results to handle potential errors from any of the tasks
    let results: Vec<io::Result<()>> = new_songs
        .par_iter() // Parallel iterator
        .map(|(name, info)| { // 'name' here is &&String
            let src_path = Path::new(&info["path"]);
            
            // The 'name' key from the HashMap (file stem) is used for messages.
            // If you prefer the full filename in messages, adjust accordingly.
            // let file_display_name = src_path.file_name().unwrap_or_default().to_string_lossy();
            
            let extension = src_path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
                .to_lowercase();

            // Each task can update the message on its cloned progress bar handle.
            // The `indicatif::ProgressBar` is designed to be thread-safe for such updates.
            let task_result = match extension.as_str() {
                "mp3" | "flac" => {
                    bar.set_message(format!("Copying: {}", **name)); // Dereference name twice
                    copy_file(src_path, dest_folder, name)
                }
                "ncm" => {
                    bar.set_message(format!("Dumping: {}", **name)); // Dereference name twice
                    process_ncm_file(src_path, dest_folder, name)
                }
                _ => {
                    // This case should be unreachable due to prior filtering in `get_music_dict`
                    // and `is_valid_music_file`.
                    unreachable!(
                        "Invalid file extension '{}' for song '{}'. This should have been filtered out.",
                        extension, **name
                    );
                }
            };

            bar.inc(1); // Increment progress for each processed song
            task_result // Return the result of copy_file or process_ncm_file
        })
        .collect(); // Collect all results (Vec<io::Result<()>>)

    // After all parallel tasks are done (or attempted), check for errors.
    let mut first_error: Option<io::Error> = None;
    for result in results {
        if let Err(e) = result {
            if first_error.is_none() {
                first_error = Some(e);
            }
            // Optionally, log all errors if desired, e.g.:
            // eprintln!("Error during sync task: {}", e);
        }
    }

    if let Some(err) = first_error {
        bar.abandon_with_message(format!("Sync encountered errors. First error: {}", err));
        Err(err) // Return the first error encountered
    } else {
        bar.finish_with_message("Sync processing complete.");
        Ok(())
    }
}

fn copy_file(src_path: &Path, dest_folder: &str, name: &str) -> io::Result<()> {
    let file_name = src_path.file_name()
        .ok_or_else(|| io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Invalid source filename for: {}", name) // Use 'name' (stem) for context
        ))?;

    let dest_path = Path::new(dest_folder).join(file_name);
    
    // Create parent directories if they don't exist for dest_path
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::copy(src_path, &dest_path)?;
    
    Ok(())
}

// Improved error handling for ncm.get_info()
fn process_ncm_file(src_path: &Path, dest_folder: &str, name: &str) -> io::Result<()> {
    let file = File::open(src_path)?;
    
    let mut ncm = Ncmdump::from_reader(file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("Failed to parse NCM file {}: {}", name, e)))?;
    
    let music_data = ncm.get_data()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("Failed to get data from NCM file {}: {}", name, e)))?;
    
    // Get metadata info using ncmdump's Info struct
    let ncm_metadata = ncm.get_info()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("Failed to get metadata from NCM file {}: {}", name, e)))?;
    
    let mut file_format = ncm_metadata.format.to_lowercase();
    if file_format.is_empty() {
        // Default to a common format like flac or mp3 if empty. Original used "flac".
        file_format = "flac".to_string(); 
    }

    // Create target file path using the (file stem) 'name' and determined format
    let dest_file_name = format!("{}.{}", name, file_format);
    let dest_path = Path::new(dest_folder).join(dest_file_name);
    
    // Create parent directories if they don't exist for dest_path
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    
    let mut target_file = File::create(&dest_path)?;
    target_file.write_all(&music_data)?;
    
    Ok(())
}


fn main() -> Result<(), Error> {
    let cmd = Cmd::parse();
    let config_file_path = cmd.config.unwrap_or_else(|| "config.toml".to_string());
    
    let config_content = fs::read_to_string(&config_file_path)
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("Failed to read config file '{}': {}", config_file_path, e)))?;
    
    let config: Config = toml::from_str(&config_content)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("Failed to parse TOML from config file '{}': {}", config_file_path, e)))?;
    
    println!("Config loaded: Source='{}', Destination='{}'", config.source, config.destination);
    
    let wf = &config.source;
    let sf = &config.destination;
    
    if !Path::new(wf).exists() {
        eprintln!("Source folder does not exist: {}", wf);
        // Return an error instead of Ok(()) to indicate failure.
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("Source folder not found: {}", wf)));
    }
    
    if !Path::new(sf).exists() {
        println!("Destination folder '{}' does not exist, creating...", sf);
        fs::create_dir_all(sf)?; // Propagate error if creation fails
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
        sync_music_library(&new_songs, sf)?;
    }
    
    println!("Sync completed successfully.");
    Ok(())
}