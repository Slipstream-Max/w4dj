use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use id3::frame::{ExtendedText, Picture, PictureType as Id3PictureType};
use id3::{TagLike, Version};
use lofty::config::ParseOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::PictureType as LoftyPictureType;
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey};
use ncmdump::{NcmInfo, Ncmdump};
use serde::{Deserialize, Serialize};
use tempfile::{Builder as TempBuilder, TempPath};

use crate::config::Mode;
use crate::doctor;

const W4DJ_ID: &str = "W4DJ_ID";

#[derive(Debug)]
pub(crate) struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("operation cancelled")
    }
}

impl std::error::Error for Cancelled {}

pub(crate) fn ensure_not_cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Cancelled.into())
    } else {
        Ok(())
    }
}

pub(crate) fn is_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Cancelled>().is_some()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceVariant {
    pub format: String,
    pub bitrate: Option<u64>,
    pub size: u64,
}

impl SourceVariant {
    pub fn is_better_than(&self, other: &Self) -> bool {
        let rank = format_rank(&self.format);
        let other_rank = format_rank(&other.format);
        if rank != other_rank {
            return rank > other_rank;
        }

        match (self.bitrate, other.bitrate) {
            (Some(current), Some(previous)) if current != previous => current > previous,
            _ => {
                let threshold = other.size / 20;
                self.size > other.size.saturating_add(threshold)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct SourceItem {
    pub path: PathBuf,
    pub id: String,
    pub fallback_id: String,
    pub display_name: String,
    pub variant: SourceVariant,
}

#[derive(Clone, Debug)]
pub struct OutputIdentity {
    pub id: String,
    pub fallback_id: String,
    pub has_embedded_id: bool,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub source: SourceItem,
    pub target: PathBuf,
    pub old_output: Option<PathBuf>,
    pub mode: Mode,
    pub ffmpeg: Option<PathBuf>,
}

#[derive(Clone, Debug, Default)]
struct MediaMetadata {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    genre: Option<String>,
    track: Option<u32>,
    track_total: Option<u32>,
    disc: Option<u32>,
    disc_total: Option<u32>,
    duration_secs: u64,
    platform_id: Option<String>,
    cover: Option<Vec<u8>>,
}

pub fn inspect_source(path: &Path) -> Result<SourceItem> {
    let size = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?
        .len();
    let display_name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("track")
        .to_string();
    let extension = extension(path);

    if extension == "ncm" {
        inspect_ncm(path, size, display_name)
    } else {
        let (metadata, properties) = read_regular_metadata(path, false)?;
        let fallback_id = metadata_id(&metadata, &display_name);
        let id = read_embedded_id(path)
            .or(metadata.platform_id.clone())
            .unwrap_or_else(|| fallback_id.clone());
        Ok(SourceItem {
            path: path.to_path_buf(),
            id,
            fallback_id,
            display_name,
            variant: SourceVariant {
                format: extension,
                bitrate: properties.audio_bitrate().map(u64::from),
                size,
            },
        })
    }
}

pub fn inspect_output(path: &Path) -> Result<OutputIdentity> {
    let display_name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("track");
    let (metadata, _) = read_regular_metadata(path, false)?;
    let fallback_id = metadata_id(&metadata, display_name);
    let embedded_id = read_embedded_id(path);
    let has_embedded_id = embedded_id.is_some();
    let id = embedded_id
        .or(metadata.platform_id)
        .unwrap_or_else(|| fallback_id.clone());
    Ok(OutputIdentity {
        id,
        fallback_id,
        has_embedded_id,
    })
}

pub(crate) fn process_with_cancel(job: &Job, cancel: &AtomicBool) -> Result<()> {
    ensure_not_cancelled(cancel)?;
    let parent = job
        .target
        .parent()
        .context("target file has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create output directory {}", parent.display()))?;

    let (metadata, prepared_audio) = prepare_source(&job.source, parent, cancel)?;
    ensure_not_cancelled(cancel)?;
    let target_format = job.mode.extension(&job.source.variant.format);
    let final_temp = if job.mode.needs_ffmpeg() {
        let temp = create_temp(parent, target_format)?;
        transcode(
            job.ffmpeg
                .as_deref()
                .context("FFmpeg is required for this output mode")?,
            prepared_audio.path(),
            temp.as_ref(),
            job.mode,
            cancel,
        )?;
        temp
    } else {
        match prepared_audio {
            PreparedAudio::Temporary(path) => path,
            PreparedAudio::Borrowed(path) => {
                let temp = create_temp(parent, target_format)?;
                let temp_path: &Path = temp.as_ref();
                let mut input = File::open(&path)
                    .with_context(|| format!("failed to open {} for copying", path.display()))?;
                let mut output = File::create(temp_path)
                    .context("failed to create temporary output for copying")?;
                copy_with_cancel(&mut input, &mut output, cancel).with_context(|| {
                    format!("failed to copy {} to a temporary file", path.display())
                })?;
                temp
            }
        }
    };

    ensure_not_cancelled(cancel)?;
    write_metadata(
        final_temp.as_ref(),
        target_format,
        &metadata,
        &job.source.id,
    )?;
    ensure_not_cancelled(cancel)?;
    let identity = inspect_output(final_temp.as_ref())
        .with_context(|| format!("output validation failed for {}", job.source.path.display()))?;
    if !identity_matches_source(&identity, &job.source) {
        bail!(
            "output validation found the wrong track ID for {}",
            job.source.path.display()
        );
    }
    let final_temp_path: &Path = final_temp.as_ref();
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(final_temp_path)
        .and_then(|file| file.sync_all())
        .with_context(|| {
            format!(
                "failed to sync temporary output for {}",
                job.target.display()
            )
        })?;
    final_temp
        .persist(&job.target)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to publish output file {}", job.target.display()))?;

    if let Some(old_output) = &job.old_output
        && old_output != &job.target
        && old_output.exists()
    {
        let belongs_to_job = inspect_output(old_output)
            .map(|identity| identity_matches_source(&identity, &job.source))
            .unwrap_or(false);
        if belongs_to_job && let Err(error) = fs::remove_file(old_output) {
            eprintln!(
                "cleanup warning for superseded output {}: {}",
                old_output.display(),
                error
            );
        }
    }

    Ok(())
}

fn identity_matches_source(identity: &OutputIdentity, source: &SourceItem) -> bool {
    identity.id == source.id
        || (!identity.has_embedded_id && identity.fallback_id == source.fallback_id)
}

fn inspect_ncm(path: &Path, size: u64, display_name: String) -> Result<SourceItem> {
    let info = read_ncm_info(path)?;
    let format = if info.format.trim().is_empty() {
        sniff_ncm_format(path)?
    } else {
        info.format.to_ascii_lowercase()
    };
    let metadata = metadata_from_ncm(&info, None);
    let fallback_id = metadata_id(&metadata, &display_name);
    let id = if info.id > 0 {
        format!("ncm:{}", info.id)
    } else {
        fallback_id.clone()
    };

    Ok(SourceItem {
        path: path.to_path_buf(),
        id,
        fallback_id,
        display_name,
        variant: SourceVariant {
            format,
            bitrate: normalize_ncm_bitrate(info.bitrate),
            size,
        },
    })
}

fn prepare_source(
    source: &SourceItem,
    temp_dir: &Path,
    cancel: &AtomicBool,
) -> Result<(MediaMetadata, PreparedAudio)> {
    ensure_not_cancelled(cancel)?;
    if extension(&source.path) == "ncm" {
        let file = File::open(&source.path)
            .with_context(|| format!("failed to open {}", source.path.display()))?;
        let mut ncm = Ncmdump::from_reader(file)
            .with_context(|| format!("invalid NCM file {}", source.path.display()))?;
        let info = ncm.get_info().with_context(|| {
            format!("failed to read NCM metadata from {}", source.path.display())
        })?;
        let image = ncm
            .get_image()
            .with_context(|| format!("failed to read NCM cover from {}", source.path.display()))?;
        let metadata = metadata_from_ncm(&info, if image.is_empty() { None } else { Some(image) });

        let temp = create_temp(temp_dir, &source.variant.format)?;
        let input = File::open(&source.path)
            .with_context(|| format!("failed to reopen {}", source.path.display()))?;
        let mut ncm = Ncmdump::from_reader(input)
            .with_context(|| format!("invalid NCM file {}", source.path.display()))?;
        let temp_path: &Path = temp.as_ref();
        let mut output =
            File::create(temp_path).context("failed to create NCM temporary output")?;
        copy_with_cancel(&mut ncm, &mut output, cancel)
            .with_context(|| format!("failed to dump NCM file {}", source.path.display()))?;
        output.flush()?;
        Ok((metadata, PreparedAudio::Temporary(temp)))
    } else {
        let (metadata, _) = read_regular_metadata(&source.path, true)?;
        Ok((metadata, PreparedAudio::Borrowed(source.path.clone())))
    }
}

enum PreparedAudio {
    Borrowed(PathBuf),
    Temporary(TempPath),
}

impl PreparedAudio {
    fn path(&self) -> &Path {
        match self {
            Self::Borrowed(path) => path,
            Self::Temporary(path) => path.as_ref(),
        }
    }
}

fn transcode(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    mode: Mode,
    cancel: &AtomicBool,
) -> Result<()> {
    ensure_not_cancelled(cancel)?;
    let mut command = doctor::ffmpeg_command(ffmpeg);
    command
        .arg("-nostdin")
        .arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-xerror")
        .arg("-i")
        .arg(input)
        .arg("-map")
        .arg("0:a:0")
        .arg("-map_metadata")
        .arg("0")
        .arg("-threads")
        .arg("1");

    match mode {
        Mode::Mp3 => {
            command
                .arg("-c:a")
                .arg("libmp3lame")
                .arg("-q:a")
                .arg("2")
                .arg("-id3v2_version")
                .arg("4");
        }
        Mode::Wav => {
            command.arg("-c:a").arg("pcm_s16le");
        }
        Mode::Original => bail!("original mode must not invoke FFmpeg"),
    }

    let mut child = command
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run FFmpeg at {}", ffmpeg.display()))?;
    let mut child_stderr = child
        .stderr
        .take()
        .context("failed to capture FFmpeg error output")?;
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = child_stderr.read_to_end(&mut bytes);
        (result, bytes)
    });
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stderr_reader.join();
            return Err(Cancelled.into());
        }
        if let Some(status) = child
            .try_wait()
            .context("failed to wait for FFmpeg to finish")?
        {
            break status;
        }
        thread::sleep(Duration::from_millis(50));
    };
    let (stderr_result, stderr) = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("FFmpeg error reader panicked"))?;
    stderr_result.context("failed to read FFmpeg error output")?;
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr);
        if detail.trim().is_empty() {
            bail!(
                "FFmpeg failed for {} with status {}",
                input.display(),
                status
            );
        }
        bail!(
            "FFmpeg failed for {} with status {}: {}",
            input.display(),
            status,
            detail.trim()
        );
    }
    Ok(())
}

fn copy_with_cancel(
    input: &mut impl Read,
    output: &mut impl Write,
    cancel: &AtomicBool,
) -> Result<u64> {
    let mut buffer = [0_u8; 64 * 1024];
    let mut copied = 0_u64;
    loop {
        ensure_not_cancelled(cancel)?;
        let read = input.read(&mut buffer)?;
        if read == 0 {
            return Ok(copied);
        }
        output.write_all(&buffer[..read])?;
        copied += read as u64;
    }
}

fn read_ncm_info(path: &Path) -> Result<NcmInfo> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut ncm = Ncmdump::from_reader(file)
        .with_context(|| format!("invalid NCM file {}", path.display()))?;
    ncm.get_info()
        .with_context(|| format!("failed to read NCM metadata from {}", path.display()))
}

fn sniff_ncm_format(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut ncm = Ncmdump::from_reader(file)
        .with_context(|| format!("invalid NCM file {}", path.display()))?;
    let mut header = [0_u8; 12];
    let size = ncm
        .read(&mut header)
        .with_context(|| format!("failed to inspect NCM audio in {}", path.display()))?;
    if header[..size].starts_with(b"fLaC") {
        Ok("flac".to_string())
    } else if header[..size].starts_with(b"ID3")
        || header[..size]
            .windows(2)
            .any(|bytes| bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
    {
        Ok("mp3".to_string())
    } else {
        bail!(
            "unsupported audio format inside NCM file {}",
            path.display()
        )
    }
}

fn read_regular_metadata(
    path: &Path,
    include_cover: bool,
) -> Result<(MediaMetadata, lofty::properties::FileProperties)> {
    let tagged = Probe::open(path)
        .with_context(|| format!("failed to open audio metadata for {}", path.display()))?
        .guess_file_type()
        .with_context(|| format!("failed to identify audio format for {}", path.display()))?
        .options(ParseOptions::new().read_cover_art(include_cover))
        .read()
        .with_context(|| format!("failed to read audio metadata from {}", path.display()))?;
    let properties = tagged.properties().clone();
    let mut metadata = MediaMetadata {
        duration_secs: properties.duration().as_secs(),
        ..MediaMetadata::default()
    };
    for tag in tagged.primary_tag().into_iter().chain(tagged.tags()) {
        metadata.title = metadata
            .title
            .or_else(|| tag.title().map(|value| value.into_owned()));
        metadata.artist = metadata
            .artist
            .or_else(|| tag.artist().map(|value| value.into_owned()));
        metadata.album = metadata
            .album
            .or_else(|| tag.album().map(|value| value.into_owned()));
        metadata.genre = metadata
            .genre
            .or_else(|| tag.genre().map(|value| value.into_owned()));
        metadata.track = metadata.track.or_else(|| tag.track());
        metadata.track_total = metadata.track_total.or_else(|| tag.track_total());
        metadata.disc = metadata.disc.or_else(|| tag.disk());
        metadata.disc_total = metadata.disc_total.or_else(|| tag.disk_total());
        metadata.platform_id = metadata.platform_id.or_else(|| {
            tag.get_string(ItemKey::MusicBrainzRecordingId)
                .map(|id| format!("mb:{}", normalize(id)))
                .or_else(|| {
                    tag.get_string(ItemKey::Isrc)
                        .map(|id| format!("isrc:{}", normalize(id)))
                })
        });
        if include_cover && metadata.cover.is_none() {
            metadata.cover = tag
                .pictures()
                .iter()
                .find(|picture| picture.pic_type() == LoftyPictureType::CoverFront)
                .or_else(|| tag.pictures().first())
                .map(|picture| picture.data().to_vec());
        }
    }
    Ok((metadata, properties))
}

fn metadata_from_ncm(info: &NcmInfo, cover: Option<Vec<u8>>) -> MediaMetadata {
    MediaMetadata {
        title: Some(info.name.clone()),
        artist: Some(
            info.artist
                .iter()
                .map(|artist| artist.0.as_str())
                .collect::<Vec<_>>()
                .join("/"),
        ),
        album: Some(info.album.clone()),
        duration_secs: info.duration / 1000,
        cover,
        ..MediaMetadata::default()
    }
}

fn write_metadata(path: &Path, format: &str, metadata: &MediaMetadata, id: &str) -> Result<()> {
    match format {
        "mp3" | "wav" => write_id3_metadata(path, metadata, id),
        "flac" => write_flac_metadata(path, metadata, id),
        other => bail!("cannot write metadata for unsupported output format {other}"),
    }
}

fn write_id3_metadata(path: &Path, metadata: &MediaMetadata, id: &str) -> Result<()> {
    let mut tag = id3::Tag::read_from_path(path).unwrap_or_default();
    if let Some(title) = &metadata.title {
        tag.set_title(title);
    }
    if let Some(artist) = &metadata.artist {
        tag.set_artist(artist);
    }
    if let Some(album) = &metadata.album {
        tag.set_album(album);
    }
    if let Some(genre) = &metadata.genre {
        tag.set_genre(genre);
    }
    if let Some(track) = metadata.track {
        tag.set_track(track);
    }
    if let Some(total) = metadata.track_total {
        tag.set_total_tracks(total);
    }
    if let Some(disc) = metadata.disc {
        tag.set_disc(disc);
    }
    if let Some(total) = metadata.disc_total {
        tag.set_total_discs(total);
    }

    tag.remove_extended_text(Some(W4DJ_ID), None);
    tag.add_frame(ExtendedText {
        description: W4DJ_ID.to_string(),
        value: id.to_string(),
    });
    if let Some(cover) = &metadata.cover {
        let already_present = tag.pictures().any(|picture| picture.data == *cover);
        if !already_present {
            tag.remove_picture_by_type(Id3PictureType::CoverFront);
            tag.add_frame(Picture {
                mime_type: image_mime_type(cover).to_string(),
                picture_type: Id3PictureType::CoverFront,
                description: String::new(),
                data: cover.clone(),
            });
        }
    }

    tag.write_to_path(path, Version::Id3v24)
        .with_context(|| format!("failed to write ID3 metadata to {}", path.display()))
}

fn write_flac_metadata(path: &Path, metadata: &MediaMetadata, id: &str) -> Result<()> {
    let mut tag = metaflac::Tag::read_from_path(path)
        .with_context(|| format!("failed to read FLAC metadata from {}", path.display()))?;
    let comments = tag.vorbis_comments_mut();
    if let Some(title) = &metadata.title {
        comments.set_title(vec![title.clone()]);
    }
    if let Some(artist) = &metadata.artist {
        comments.set_artist(vec![artist.clone()]);
    }
    if let Some(album) = &metadata.album {
        comments.set_album(vec![album.clone()]);
    }
    if let Some(genre) = &metadata.genre {
        comments.set_genre(vec![genre.clone()]);
    }
    if let Some(track) = metadata.track {
        comments.set_track(track);
    }
    if let Some(total) = metadata.track_total {
        comments.set_total_tracks(total);
    }
    tag.set_vorbis(W4DJ_ID, vec![id]);
    if let Some(cover) = &metadata.cover {
        let already_present = tag.pictures().any(|picture| picture.data == *cover);
        if !already_present {
            tag.remove_picture_type(metaflac::block::PictureType::CoverFront);
            tag.add_picture(
                image_mime_type(cover),
                metaflac::block::PictureType::CoverFront,
                cover.clone(),
            );
        }
    }
    tag.save()
        .with_context(|| format!("failed to write FLAC metadata to {}", path.display()))
}

fn read_embedded_id(path: &Path) -> Option<String> {
    match extension(path).as_str() {
        "mp3" | "wav" => id3::Tag::read_from_path(path).ok().and_then(|tag| {
            tag.extended_texts()
                .find(|text| text.description.eq_ignore_ascii_case(W4DJ_ID))
                .map(|text| text.value.clone())
        }),
        "flac" => metaflac::Tag::read_from_path(path).ok().and_then(|tag| {
            tag.get_vorbis(W4DJ_ID)
                .and_then(|mut values| values.next().map(str::to_string))
        }),
        _ => None,
    }
}

fn metadata_id(metadata: &MediaMetadata, fallback_name: &str) -> String {
    let title = metadata.title.as_deref().unwrap_or(fallback_name);
    let value = format!(
        "{}|{}|{}|{}",
        normalize(title),
        normalize(metadata.artist.as_deref().unwrap_or("")),
        normalize(metadata.album.as_deref().unwrap_or("")),
        metadata.duration_secs
    );
    format!("meta:v1:{:016x}", fnv1a(value.as_bytes()))
}

fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn normalize_ncm_bitrate(bitrate: u64) -> Option<u64> {
    if bitrate == 0 {
        None
    } else if bitrate > 10_000 {
        Some(bitrate / 1000)
    } else {
        Some(bitrate)
    }
}

fn format_rank(format: &str) -> u8 {
    match format {
        "wav" => 4,
        "flac" => 3,
        "mp3" => 2,
        _ => 1,
    }
}

fn create_temp(directory: &Path, extension: &str) -> Result<TempPath> {
    let suffix = format!(".{extension}");
    let file = TempBuilder::new()
        .prefix(".w4dj-")
        .suffix(&suffix)
        .tempfile_in(directory)
        .with_context(|| format!("failed to create temporary file in {}", directory.display()))?;
    Ok(file.into_temp_path())
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn image_mime_type(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"BM") {
        "image/bmp"
    } else {
        "image/*"
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Cursor, Write as _};
    use std::sync::atomic::AtomicBool;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn cancelled_copy_stops_before_writing() {
        let cancel = AtomicBool::new(true);
        let mut input = Cursor::new(vec![1_u8; 128 * 1024]);
        let mut output = Vec::new();

        let error = copy_with_cancel(&mut input, &mut output, &cancel).unwrap_err();

        assert!(is_cancelled(&error));
        assert!(output.is_empty());
    }

    #[test]
    fn lossless_source_replaces_lossy_source() {
        let mp3 = SourceVariant {
            format: "mp3".to_string(),
            bitrate: Some(320),
            size: 10,
        };
        let flac = SourceVariant {
            format: "flac".to_string(),
            bitrate: Some(900),
            size: 20,
        };
        assert!(flac.is_better_than(&mp3));
        assert!(!mp3.is_better_than(&flac));
    }

    #[test]
    fn metadata_ids_are_stable_after_normalization() {
        let first = MediaMetadata {
            title: Some("  Song  Name ".to_string()),
            artist: Some("Artist".to_string()),
            album: Some("Album".to_string()),
            duration_secs: 180,
            ..MediaMetadata::default()
        };
        let second = MediaMetadata {
            title: Some("song name".to_string()),
            artist: Some("artist".to_string()),
            album: Some("album".to_string()),
            duration_secs: 180,
            ..MediaMetadata::default()
        };
        assert_eq!(
            metadata_id(&first, "ignored"),
            metadata_id(&second, "ignored")
        );
    }

    #[test]
    fn image_type_accepts_jpeg_without_requiring_an_app_marker() {
        assert_eq!(image_mime_type(&[0xff, 0xd8, 0xff, 0xdb]), "image/jpeg");
    }

    #[test]
    fn wav_id_and_cover_round_trip_without_losing_audio() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("cover test.wav");
        write_test_wav(&path)?;
        let cover = vec![
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0, b'I', b'H', b'D', b'R',
        ];
        let metadata = MediaMetadata {
            title: Some("Cover Test".to_string()),
            artist: Some("W4DJ".to_string()),
            album: Some("Tests".to_string()),
            duration_secs: 1,
            cover: Some(cover.clone()),
            ..MediaMetadata::default()
        };

        write_metadata(&path, "wav", &metadata, "ncm:42")?;
        let identity = inspect_output(&path)?;
        assert_eq!(identity.id, "ncm:42");
        let tag = id3::Tag::read_from_path(&path)?;
        assert_eq!(tag.title(), Some("Cover Test"));
        assert!(tag.pictures().any(|picture| picture.data == cover));
        let tagged = Probe::open(&path)?.guess_file_type()?.read()?;
        assert_eq!(tagged.properties().duration().as_secs(), 1);
        Ok(())
    }

    fn write_test_wav(path: &Path) -> Result<()> {
        let sample_rate = 8_000_u32;
        let samples = vec![0_u8; sample_rate as usize * 2];
        let data_len = samples.len() as u32;
        let mut file = fs::File::create(path)?;
        file.write_all(b"RIFF")?;
        file.write_all(&(36 + data_len).to_le_bytes())?;
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&16_u32.to_le_bytes())?;
        file.write_all(&1_u16.to_le_bytes())?;
        file.write_all(&1_u16.to_le_bytes())?;
        file.write_all(&sample_rate.to_le_bytes())?;
        file.write_all(&(sample_rate * 2).to_le_bytes())?;
        file.write_all(&2_u16.to_le_bytes())?;
        file.write_all(&16_u16.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&data_len.to_le_bytes())?;
        file.write_all(&samples)?;
        Ok(())
    }
}
