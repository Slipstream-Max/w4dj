use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use walkdir::{DirEntry, WalkDir};

use crate::config::Config;
use crate::doctor;
use crate::dump::{self, Job, OutputIdentity, SourceItem, SourceVariant};

const MANIFEST_NAME: &str = ".w4dj-state.json";
const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
struct Manifest {
    version: u32,
    entries: Vec<ManifestEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            entries: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ManifestEntry {
    id: String,
    output: PathBuf,
    profile: String,
    source: SourceVariant,
}

#[derive(Default)]
struct OutputIndex {
    by_id: HashMap<String, Vec<PathBuf>>,
    untagged_by_fallback: HashMap<String, Vec<PathBuf>>,
}

#[derive(Clone, Debug)]
pub enum SyncEvent {
    Status(String),
    Progress {
        completed: usize,
        total: usize,
        current: Option<String>,
    },
    Finished(SyncSummary),
    Cancelled(SyncSummary),
}

#[derive(Clone, Debug, Default)]
pub struct SyncSummary {
    pub processed: usize,
    pub skipped: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

pub fn run(config: &Config) -> Result<()> {
    let bar = ProgressBar::new(0);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:36.cyan/blue}] {pos}/{len} {msg}",
        )
        .expect("valid progress template"),
    );

    run_with_progress(config, |event| match event {
        SyncEvent::Status(status) => bar.set_message(status),
        SyncEvent::Progress {
            completed,
            total,
            current,
        } => {
            bar.set_length(total as u64);
            bar.set_position(completed as u64);
            if let Some(current) = current {
                bar.set_message(current);
            }
        }
        SyncEvent::Finished(summary) => {
            if summary.failed == 0 {
                bar.finish_and_clear();
            } else {
                bar.abandon_with_message("some files failed");
            }
            println!(
                "Sync complete: {} processed, {} skipped, {} failed.",
                summary.processed, summary.skipped, summary.failed
            );
            for error in summary.errors {
                eprintln!("  {error}");
            }
        }
        SyncEvent::Cancelled(summary) => {
            bar.abandon_with_message("sync cancelled");
            println!(
                "Sync cancelled: {} processed, {} skipped, {} failed.",
                summary.processed, summary.skipped, summary.failed
            );
        }
    })
    .map(|_| ())
}

pub fn run_with_progress(
    config: &Config,
    report: impl Fn(SyncEvent) + Sync,
) -> Result<SyncSummary> {
    let cancel = AtomicBool::new(false);
    run_with_progress_cancellable(config, &cancel, report)
}

pub fn run_with_progress_cancellable(
    config: &Config,
    cancel: &AtomicBool,
    report: impl Fn(SyncEvent) + Sync,
) -> Result<SyncSummary> {
    match run_with_progress_inner(config, cancel, &report) {
        Err(error) if dump::is_cancelled(&error) => {
            let summary = SyncSummary::default();
            report(SyncEvent::Cancelled(summary.clone()));
            Ok(summary)
        }
        result => result,
    }
}

fn run_with_progress_inner(
    config: &Config,
    cancel: &AtomicBool,
    report: &(impl Fn(SyncEvent) + Sync),
) -> Result<SyncSummary> {
    let pool = rayon::ThreadPoolBuilder::new()
        .build()
        .context("failed to create the worker pool")?;

    dump::ensure_not_cancelled(cancel)?;
    let source_paths = scan_inputs(&config.inputs, &config.output, cancel)?;
    report(SyncEvent::Status(format!(
        "Scanning metadata for {} input files...",
        source_paths.len()
    )));
    let inspections = pool.install(|| {
        source_paths
            .par_iter()
            .filter_map(|path| {
                if cancel.load(Ordering::Relaxed) {
                    None
                } else {
                    Some((path, dump::inspect_source(path)))
                }
            })
            .collect::<Vec<_>>()
    });
    dump::ensure_not_cancelled(cancel)?;

    let mut inspection_errors = Vec::new();
    let mut sources = BTreeMap::<String, SourceItem>::new();
    for (path, result) in inspections {
        match result {
            Ok(source) => select_best_source(&mut sources, source),
            Err(error) => inspection_errors.push(format!("{}: {error:#}", path.display())),
        }
    }

    let manifest_path = config.output.join(MANIFEST_NAME);
    let manifest = load_manifest(&manifest_path)?;
    let manifest_was_empty = manifest.entries.is_empty();
    let mut entries = manifest
        .entries
        .into_iter()
        .map(|entry| (entry.id.clone(), entry))
        .collect::<BTreeMap<_, _>>();

    let mut located = HashMap::<String, PathBuf>::new();
    let mut unresolved = Vec::new();
    for source in sources.values() {
        dump::ensure_not_cancelled(cancel)?;
        let Some(entry) = entries.get(&source.id) else {
            if manifest_was_empty {
                unresolved.push(source.id.clone());
            }
            continue;
        };
        let path = config.output.join(&entry.output);
        if output_matches(&path, source) {
            located.insert(source.id.clone(), path);
        } else {
            unresolved.push(source.id.clone());
        }
    }

    let output_index = if unresolved.is_empty() {
        None
    } else {
        report(SyncEvent::Status(
            "Searching the output tree for moved files...".to_string(),
        ));
        Some(build_output_index(&config.output, &pool, cancel))
    };
    dump::ensure_not_cancelled(cancel)?;

    if let Some(index) = &output_index {
        let mut location_claims = located
            .iter()
            .map(|(id, path)| (path_key(path), id.clone()))
            .collect::<HashMap<_, _>>();
        for id in unresolved {
            dump::ensure_not_cancelled(cancel)?;
            let source = &sources[&id];
            if let Some(path) = find_indexed_output(index, source) {
                let key = path_key(&path);
                if location_claims
                    .get(&key)
                    .is_none_or(|claimed_id| claimed_id == &id)
                {
                    location_claims.insert(key, id.clone());
                    located.insert(id, path);
                }
            }
        }
    }

    let mut claims = build_claims(&entries, &config.output);
    let mut jobs = Vec::new();
    let mut skipped = 0_usize;
    let profile = config.mode.profile().to_string();

    for source in sources.values() {
        dump::ensure_not_cancelled(cancel)?;
        let previous = entries.get(&source.id).cloned();
        let existing = located.get(&source.id).cloned();

        if let Some(existing) = &existing {
            claims.insert(path_key(existing), source.id.clone());
            let relative = relative_output(&config.output, existing)?;
            if let Some(entry) = entries.get_mut(&source.id) {
                entry.output = relative;
            } else if output_extension_matches(config, source, existing) {
                entries.insert(
                    source.id.clone(),
                    ManifestEntry {
                        id: source.id.clone(),
                        output: relative,
                        profile: profile.clone(),
                        source: source.variant.clone(),
                    },
                );
                skipped += 1;
                continue;
            }
        }

        let needs_processing = match (&previous, &existing) {
            (_, None) => true,
            (None, Some(_)) => true,
            (Some(entry), Some(_)) => {
                entry.profile != profile || source.variant.is_better_than(&entry.source)
            }
        };
        if !needs_processing {
            skipped += 1;
            continue;
        }

        let desired_extension = config.mode.extension(&source.variant.format);
        let base_target = if let Some(path) = &existing {
            path.with_extension(desired_extension)
        } else if let Some(entry) = &previous {
            config
                .output
                .join(&entry.output)
                .with_extension(desired_extension)
        } else {
            config
                .output
                .join(&source.display_name)
                .with_extension(desired_extension)
        };
        let target = reserve_target(base_target, source, &mut claims);
        jobs.push(Job {
            source: source.clone(),
            target,
            old_output: existing,
            mode: config.mode,
            ffmpeg: None,
        });
    }

    if config.mode.needs_ffmpeg() && !jobs.is_empty() {
        let ffmpeg = doctor::find_ffmpeg().context(
            "FFmpeg was not found next to w4dj or in PATH; it is required for mp3 and wav modes",
        )?;
        for job in &mut jobs {
            job.ffmpeg = Some(ffmpeg.clone());
        }
    }

    let total = jobs.len();
    report(SyncEvent::Progress {
        completed: 0,
        total,
        current: None,
    });
    let completed = AtomicUsize::new(0);
    let results = pool.install(|| {
        jobs.par_iter()
            .map(|job| {
                let result = dump::process_with_cancel(job, cancel);
                if !result.as_ref().is_err_and(dump::is_cancelled) {
                    let completed = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    report(SyncEvent::Progress {
                        completed,
                        total,
                        current: Some(job.source.display_name.clone()),
                    });
                }
                (job, result)
            })
            .collect::<Vec<_>>()
    });

    let mut process_errors = Vec::new();
    let mut processed = 0_usize;
    for (job, result) in results {
        match result {
            Ok(()) => {
                processed += 1;
                entries.insert(
                    job.source.id.clone(),
                    ManifestEntry {
                        id: job.source.id.clone(),
                        output: relative_output(&config.output, &job.target)?,
                        profile: profile.clone(),
                        source: job.source.variant.clone(),
                    },
                );
            }
            Err(error) if dump::is_cancelled(&error) => {}
            Err(error) => process_errors.push(format!("{}: {error:#}", job.source.path.display())),
        }
    }
    save_manifest(
        &manifest_path,
        Manifest {
            version: MANIFEST_VERSION,
            entries: entries.into_values().collect(),
        },
    )?;

    let errors = inspection_errors
        .into_iter()
        .chain(process_errors)
        .collect::<Vec<_>>();
    let summary = SyncSummary {
        processed,
        skipped,
        failed: errors.len(),
        errors,
    };
    if cancel.load(Ordering::Relaxed) {
        report(SyncEvent::Cancelled(summary.clone()));
        return Ok(summary);
    }
    report(SyncEvent::Finished(summary.clone()));
    if summary.failed > 0 {
        bail!("{} files could not be synchronized", summary.failed);
    }
    Ok(summary)
}

fn scan_inputs(inputs: &[PathBuf], output: &Path, cancel: &AtomicBool) -> Result<Vec<PathBuf>> {
    let mut files = HashSet::new();
    for input in inputs {
        dump::ensure_not_cancelled(cancel)?;
        if input.is_file() {
            if is_supported(input) {
                files.insert(input.clone());
            } else {
                bail!("unsupported input file: {}", input.display());
            }
            continue;
        }

        let walker = WalkDir::new(input)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| should_enter(entry, output));
        for entry in walker {
            dump::ensure_not_cancelled(cancel)?;
            match entry {
                Ok(entry) if entry.file_type().is_file() && is_supported(entry.path()) => {
                    let path = fs::canonicalize(entry.path()).with_context(|| {
                        format!("failed to resolve input file {}", entry.path().display())
                    })?;
                    files.insert(path);
                }
                Ok(_) => {}
                Err(error) => eprintln!("scan warning: {error}"),
            }
        }
    }
    let mut files = files.into_iter().collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn should_enter(entry: &DirEntry, output: &Path) -> bool {
    entry.depth() == 0 || !entry.path().starts_with(output)
}

fn select_best_source(sources: &mut BTreeMap<String, SourceItem>, candidate: SourceItem) {
    match sources.get(&candidate.id) {
        Some(current) if !candidate.variant.is_better_than(&current.variant) => {}
        _ => {
            sources.insert(candidate.id.clone(), candidate);
        }
    }
}

fn build_output_index(output: &Path, pool: &rayon::ThreadPool, cancel: &AtomicBool) -> OutputIndex {
    let paths = WalkDir::new(output)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            match entry {
                Ok(entry)
                    if entry.file_type().is_file()
                        && is_supported(entry.path())
                        && !is_temporary(entry.path()) =>
                {
                    Some(entry.path().to_path_buf())
                }
                Ok(_) => None,
                Err(error) => {
                    eprintln!("output scan warning: {error}");
                    None
                }
            }
        })
        .collect::<Vec<_>>();
    let identities = pool.install(|| {
        paths
            .par_iter()
            .filter_map(|path| {
                if cancel.load(Ordering::Relaxed) {
                    None
                } else {
                    match dump::inspect_output(path) {
                        Ok(identity) => Some((path.clone(), identity)),
                        Err(error) => {
                            eprintln!("output metadata warning for {}: {error:#}", path.display());
                            None
                        }
                    }
                }
            })
            .collect::<Vec<_>>()
    });

    let mut index = OutputIndex::default();
    for (path, identity) in identities {
        index
            .by_id
            .entry(identity.id)
            .or_default()
            .push(path.clone());
        if !identity.has_embedded_id {
            index
                .untagged_by_fallback
                .entry(identity.fallback_id)
                .or_default()
                .push(path);
        }
    }
    index
}

fn find_indexed_output(index: &OutputIndex, source: &SourceItem) -> Option<PathBuf> {
    if let Some(paths) = index.by_id.get(&source.id)
        && let Some(path) = paths.iter().min()
    {
        if paths.len() > 1 {
            eprintln!(
                "output warning: {} files carry track ID {}; using {}",
                paths.len(),
                source.id,
                path.display()
            );
        }
        return Some(path.clone());
    }
    unique_path(index.untagged_by_fallback.get(&source.fallback_id))
}

fn unique_path(paths: Option<&Vec<PathBuf>>) -> Option<PathBuf> {
    match paths {
        Some(paths) if paths.len() == 1 => Some(paths[0].clone()),
        _ => None,
    }
}

fn output_matches(path: &Path, source: &SourceItem) -> bool {
    if !path.is_file() {
        return false;
    }
    dump::inspect_output(path)
        .map(|identity| identity_matches(&identity, source))
        .unwrap_or(false)
}

fn identity_matches(identity: &OutputIdentity, source: &SourceItem) -> bool {
    identity.id == source.id
        || (!identity.has_embedded_id && identity.fallback_id == source.fallback_id)
}

fn output_extension_matches(config: &Config, source: &SourceItem, output: &Path) -> bool {
    output
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case(config.mode.extension(&source.variant.format))
        })
}

fn build_claims(
    entries: &BTreeMap<String, ManifestEntry>,
    output_root: &Path,
) -> HashMap<String, String> {
    entries
        .values()
        .map(|entry| (path_key(&output_root.join(&entry.output)), entry.id.clone()))
        .collect()
}

fn reserve_target(
    candidate: PathBuf,
    source: &SourceItem,
    claims: &mut HashMap<String, String>,
) -> PathBuf {
    if target_available(&candidate, source, claims) {
        claims.insert(path_key(&candidate), source.id.clone());
        return candidate;
    }

    let parent = candidate.parent().unwrap_or_else(|| Path::new(""));
    let stem = candidate
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("track");
    let extension = candidate
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    let suffix = id_suffix(&source.id);

    for number in 1.. {
        let name = if number == 1 {
            format!("{stem} [{suffix}]")
        } else {
            format!("{stem} [{suffix}-{number}]")
        };
        let path = parent.join(name).with_extension(extension);
        if target_available(&path, source, claims) {
            claims.insert(path_key(&path), source.id.clone());
            return path;
        }
    }
    unreachable!()
}

fn target_available(path: &Path, source: &SourceItem, claims: &HashMap<String, String>) -> bool {
    if let Some(id) = claims.get(&path_key(path)) {
        return id == &source.id;
    }
    if !path.exists() {
        return true;
    }
    output_matches(path, source)
}

fn id_suffix(id: &str) -> String {
    let value = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    if value.chars().count() <= 32 {
        value
    } else {
        value.chars().take(32).collect()
    }
}

fn path_key(path: &Path) -> String {
    let value = path.to_string_lossy();
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value.into_owned()
    }
}

fn relative_output(root: &Path, output: &Path) -> Result<PathBuf> {
    output
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .with_context(|| format!("output {} is outside {}", output.display(), root.display()))
}

fn load_manifest(path: &Path) -> Result<Manifest> {
    if !path.exists() {
        return Ok(Manifest::default());
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read manifest {}", path.display()))?;
    let manifest: Manifest = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse manifest {}", path.display()))?;
    if manifest.version != MANIFEST_VERSION {
        bail!(
            "unsupported manifest version {} in {}",
            manifest.version,
            path.display()
        );
    }
    for entry in &manifest.entries {
        if !safe_relative_path(&entry.output) {
            bail!("unsafe output path in manifest: {}", entry.output.display());
        }
    }
    Ok(manifest)
}

fn save_manifest(path: &Path, mut manifest: Manifest) -> Result<()> {
    manifest
        .entries
        .sort_by(|left, right| left.id.cmp(&right.id));
    let bytes = serde_json::to_vec_pretty(&manifest).context("failed to serialize manifest")?;
    let directory = path.parent().context("manifest has no parent directory")?;
    let mut temporary = NamedTempFile::new_in(directory).with_context(|| {
        format!(
            "failed to create manifest temporary file in {}",
            directory.display()
        )
    })?;
    temporary
        .write_all(&bytes)
        .context("failed to write manifest temporary file")?;
    temporary
        .as_file()
        .sync_all()
        .context("failed to sync manifest temporary file")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to publish manifest {}", path.display()))?;
    Ok(())
}

fn safe_relative_path(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "ncm" | "mp3" | "flac" | "wav"
            )
        })
}

fn is_temporary(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".w4dj-"))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::{Mutex, atomic::AtomicBool};

    use id3::frame::ExtendedText;
    use id3::{TagLike, Version};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn pre_cancelled_sync_reports_cancellation() -> Result<()> {
        let workspace = tempdir()?;
        let input = workspace.path().join("input");
        let output = workspace.path().join("output");
        fs::create_dir_all(&input)?;
        fs::create_dir_all(&output)?;
        let config = Config {
            inputs: vec![input],
            output: output.clone(),
            mode: crate::config::Mode::Original,
        };
        let cancel = AtomicBool::new(true);
        let cancelled = Mutex::new(false);

        let summary = run_with_progress_cancellable(&config, &cancel, |event| {
            if matches!(event, SyncEvent::Cancelled(_)) {
                *cancelled.lock().unwrap() = true;
            }
        })?;

        assert!(*cancelled.lock().unwrap());
        assert_eq!(summary.processed, 0);
        assert!(!output.join(MANIFEST_NAME).exists());
        Ok(())
    }

    #[test]
    fn cancellation_after_planning_does_not_publish_output() -> Result<()> {
        let workspace = tempdir()?;
        let input = workspace.path().join("input");
        let output = workspace.path().join("output");
        fs::create_dir_all(&input)?;
        fs::create_dir_all(&output)?;
        write_test_wav(&input.join("Song.wav"), None)?;
        let config = Config {
            inputs: vec![input],
            output: output.clone(),
            mode: crate::config::Mode::Original,
        };
        let cancel = AtomicBool::new(false);
        let cancelled = Mutex::new(false);

        let summary = run_with_progress_cancellable(&config, &cancel, |event| match event {
            SyncEvent::Progress {
                completed: 0,
                total: 1,
                ..
            } => cancel.store(true, Ordering::Relaxed),
            SyncEvent::Cancelled(_) => *cancelled.lock().unwrap() = true,
            _ => {}
        })?;

        assert!(*cancelled.lock().unwrap());
        assert_eq!(summary.processed, 0);
        assert!(!output.join("Song.wav").exists());
        Ok(())
    }

    fn source(id: &str) -> SourceItem {
        SourceItem {
            path: PathBuf::from("source.ncm"),
            id: id.to_string(),
            fallback_id: "meta:v1:test".to_string(),
            display_name: "Song".to_string(),
            variant: SourceVariant {
                format: "flac".to_string(),
                bitrate: Some(900),
                size: 100,
            },
        }
    }

    #[test]
    fn different_ids_receive_stable_filename_suffixes() {
        let mut claims = HashMap::new();
        claims.insert("song.mp3".to_string(), "ncm:1".to_string());
        let target = reserve_target(PathBuf::from("Song.mp3"), &source("ncm:2"), &mut claims);
        assert_eq!(target, PathBuf::from("Song [ncm-2].mp3"));
    }

    #[test]
    fn manifest_paths_cannot_escape_the_output_directory() {
        assert!(safe_relative_path(Path::new("artist/song.mp3")));
        assert!(!safe_relative_path(Path::new("../song.mp3")));
        assert!(!safe_relative_path(Path::new("C:/song.mp3")));
    }

    #[test]
    fn moved_outputs_are_adopted_and_deleted_outputs_are_restored() -> Result<()> {
        let workspace = tempdir()?;
        let input = workspace.path().join("input with spaces");
        let output = workspace.path().join("output");
        fs::create_dir_all(&input)?;
        fs::create_dir_all(&output)?;
        write_test_wav(&input.join("Song.wav"), None)?;
        let config = Config {
            inputs: vec![input],
            output: output.clone(),
            mode: crate::config::Mode::Original,
        };

        run(&config)?;
        let original = output.join("Song.wav");
        assert!(original.is_file());

        let organized = output.join("Artist").join("Album").join("Song.wav");
        fs::create_dir_all(organized.parent().unwrap())?;
        fs::rename(&original, &organized)?;
        run(&config)?;
        assert!(organized.is_file());
        assert!(!original.exists());
        let moved_manifest = load_manifest(&output.join(MANIFEST_NAME))?;
        assert_eq!(
            moved_manifest.entries[0].output,
            PathBuf::from("Artist/Album/Song.wav")
        );

        fs::remove_file(&organized)?;
        run(&config)?;
        assert!(organized.is_file());
        assert!(!original.exists());
        Ok(())
    }

    #[test]
    fn different_ids_with_the_same_filename_do_not_overwrite_each_other() -> Result<()> {
        let workspace = tempdir()?;
        let first = workspace.path().join("first");
        let second = workspace.path().join("second");
        let output = workspace.path().join("output");
        fs::create_dir_all(&first)?;
        fs::create_dir_all(&second)?;
        fs::create_dir_all(&output)?;
        write_test_wav(&first.join("Song.wav"), Some("ncm:1"))?;
        write_test_wav(&second.join("Song.wav"), Some("ncm:2"))?;
        let config = Config {
            inputs: vec![first, second],
            output: output.clone(),
            mode: crate::config::Mode::Original,
        };

        run(&config)?;
        assert!(output.join("Song.wav").is_file());
        assert!(output.join("Song [ncm-2].wav").is_file());
        let manifest = load_manifest(&output.join(MANIFEST_NAME))?;
        assert_eq!(manifest.entries.len(), 2);
        Ok(())
    }

    #[test]
    fn one_untagged_legacy_output_is_not_adopted_by_two_ids() -> Result<()> {
        let workspace = tempdir()?;
        let first = workspace.path().join("first");
        let second = workspace.path().join("second");
        let output = workspace.path().join("output");
        fs::create_dir_all(&first)?;
        fs::create_dir_all(&second)?;
        fs::create_dir_all(&output)?;
        write_test_wav(&first.join("Song.wav"), Some("ncm:1"))?;
        write_test_wav(&second.join("Song.wav"), Some("ncm:2"))?;
        write_test_wav(&output.join("Song.wav"), None)?;
        write_common_test_tags(&output.join("Song.wav"), None)?;
        let config = Config {
            inputs: vec![first, second],
            output: output.clone(),
            mode: crate::config::Mode::Original,
        };

        run(&config)?;
        let manifest = load_manifest(&output.join(MANIFEST_NAME))?;
        assert_eq!(manifest.entries.len(), 2);
        assert_ne!(manifest.entries[0].output, manifest.entries[1].output);
        assert!(output.join("Song.wav").is_file());
        assert!(output.join("Song [ncm-2].wav").is_file());
        Ok(())
    }

    fn write_test_wav(path: &Path, id: Option<&str>) -> Result<()> {
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
        drop(file);

        if id.is_some() {
            write_common_test_tags(path, id)?;
        }
        Ok(())
    }

    fn write_common_test_tags(path: &Path, id: Option<&str>) -> Result<()> {
        let mut tag = id3::Tag::new();
        tag.set_title("Same Song");
        tag.set_artist("Same Artist");
        tag.set_album("Same Album");
        if let Some(id) = id {
            tag.add_frame(ExtendedText {
                description: "W4DJ_ID".to_string(),
                value: id.to_string(),
            });
        }
        tag.write_to_path(path, Version::Id3v24)?;
        Ok(())
    }
}
