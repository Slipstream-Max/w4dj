use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub enum Mode {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "legacy")]
    Legacy,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub source: String,
    pub destination: String,
    pub mode: Mode,
}

#[derive(clap::Parser)]
#[command(
    name = "w4dj",
    version = "0.1.0",
    author = "slipstream",
    about = "网易云音乐曲库同步器"
)]
pub struct Cmd {
    #[arg(long, short, default_value = "config.toml")]
    pub config: Option<String>,
}
