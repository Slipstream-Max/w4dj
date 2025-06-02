use clap::Parser;
use std::fs;
use toml;
use serde::{Deserialize};
use std::collections::{HashMap, HashSet};
use walkdir::{DirEntry, WalkDir};

#[derive(Parser)]
#[command(name = "w4dj", version = "0.1.0", author = "slipstream", about = "网易云音乐曲库同步器")]
struct Cmd {
    #[arg(long,short,default_value="config.toml")]
    config:Option<String>
}

#[derive(Debug,Deserialize)]
struct Config {
    source: String,
    destination: String,
}



fn get_music_dict(folder: &str) -> HashMap<String, HashMap<String, String>> {
    let mut music_dict = HashMap::new();
    
    for entry in WalkDir::new(folder)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(is_valid_music_file)
    {
        let path = entry.path();
        
        // 获取文件名（无后缀）
        let stem = path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        
        // 获取文件大小
        let size = entry.metadata()
            .map(|m| m.len().to_string())
            .unwrap_or_else(|_| "0".to_string());
        
        // 获取完整路径
        let full_path = path.to_string_lossy().into_owned();
        
        // 构建内部 HashMap
        let mut file_info = HashMap::new();
        file_info.insert("size".to_string(), size);
        file_info.insert("path".to_string(), full_path);
        
        // 插入到主字典（自动覆盖同名文件）
        music_dict.insert(stem, file_info);
    }
    
    music_dict
}

// 辅助函数：检查是否为有效音乐文件
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

pub fn compare_music_dicts(
    wf_dict: &HashMap<String, HashMap<String, String>>,
    sf_dict: &HashMap<String, HashMap<String, String>>,
) -> Vec<String> {
    // 1. 创建两个键的集合
    let mut wf_keys: HashSet<_> = wf_dict.keys().cloned().collect();
    let sf_keys: HashSet<_> = sf_dict.keys().cloned().collect();
    
    // 2. 找出两个集合中相同的键（同名文件）
    let common_keys: HashSet<_> = wf_keys.intersection(&sf_keys).cloned().collect();
    
    // 3. 遍历同名文件，检查大小是否相似
    for name in &common_keys {
        // 获取两个文件的大小信息
        if let (Some(wf_info), Some(sf_info)) = (wf_dict.get(name), sf_dict.get(name)) {
            // 解析文件大小为 u64
            if let (Ok(size1), Ok(size2)) = (
                wf_info.get("size").unwrap().parse::<u64>(),
                sf_info.get("size").unwrap().parse::<u64>(),
            ) {
                // 计算相对差异（避免除零错误）
                let max_size = size1.max(size2) as f64;
                let diff = (size1 as f64 - size2 as f64).abs();
                
                // 如果大小差异小于3%，认为是相同文件
                if max_size > 0.0 && (diff / max_size) < 0.03 {
                    // 从集合中移除这些文件
                    wf_keys.remove(name);
                }
            }
        }
    }
    
    // 4. 将剩余的文件名转换为 Vec
    let to_add: Vec<String> = wf_keys.into_iter().collect();
    to_add
}

fn main (){
    let cmd = Cmd::parse();
    let config_file_path = cmd.config.unwrap_or("config.toml".to_string());
    let config:Config = toml::from_str(&fs::read_to_string(config_file_path).unwrap()).unwrap();
    println!("{:?}",config);
    
    let wf = &config.source;
    let sf = &config.destination;
    if !std::path::Path::new(wf).exists() {
        eprintln!("源文件夹不存在: {}", wf);
        return;
    }
    if !std::path::Path::new(sf).exists() {
        eprintln!("目标文件夹不存在,自动创建");
        std::fs::create_dir_all(sf).unwrap();
    }
    let wf_dict = get_music_dict(wf);
    let sf_dict = get_music_dict(sf);
    // println!("{:#?}",wf_dict);
    // println!("{:?}",sf_dict);

    let to_add = compare_music_dicts(&wf_dict, &sf_dict);

    println!("需要新增{}首歌曲",to_add.len());

    



    
}