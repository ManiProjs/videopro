mod gpu_preview;
mod player;
mod proxy;
mod render;
mod sequence;

use gpu_preview::GpuPreview;
use player::VideoPlayer;
use slint::{Image, Model, Timer, TimerMode, wgpu_30};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

static NEXT_EXTRACTION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_PROJECT_SAVE_ID: AtomicU64 = AtomicU64::new(1);
static PENDING_PROJECT_PATH: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
const MAX_WORKSPACE_LAYOUTS: usize = 50;
const MAX_RECENT_PROJECTS: usize = 8;
static NEXT_PREVIEW_FRAME_ID: AtomicU64 = AtomicU64::new(1);

struct ImportedClip {
    name: String,
    path: String,
    duration: String,
    duration_seconds: f32,
    track: i32,
    thumbnails: [Option<std::path::PathBuf>; 3],
    waveform: Option<PathBuf>,
}

struct EditorRuntimeState {
    reset_requested: Arc<AtomicBool>,
    undo_stack: Rc<RefCell<Vec<Vec<Clip>>>>,
    redo_stack: Rc<RefCell<Vec<Vec<Clip>>>>,
    drag_snapshot: Rc<RefCell<Option<Vec<Clip>>>>,
}

#[derive(Clone)]
struct TimelinePlaybackState {
    clock: Arc<Mutex<Option<(f32, Instant)>>>,
    audio: Arc<Mutex<Option<render::TimelineAudioPlayback>>>,
    generation: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, Default)]
struct AudioTrackSettings {
    gain_db: f32,
    muted: bool,
    solo: bool,
}

fn audio_track_settings(ui: &EditorWindow) -> [AudioTrackSettings; 4] {
    [
        AudioTrackSettings {
            gain_db: ui.get_track_v2_gain_db(),
            muted: ui.get_track_v2_muted(),
            solo: ui.get_track_v2_solo(),
        },
        AudioTrackSettings {
            gain_db: ui.get_track_v1_gain_db(),
            muted: ui.get_track_v1_muted(),
            solo: ui.get_track_v1_solo(),
        },
        AudioTrackSettings {
            gain_db: ui.get_track_a1_gain_db(),
            muted: ui.get_track_a1_muted(),
            solo: ui.get_track_a1_solo(),
        },
        AudioTrackSettings {
            gain_db: ui.get_track_a2_gain_db(),
            muted: ui.get_track_a2_muted(),
            solo: ui.get_track_a2_solo(),
        },
    ]
}

fn set_audio_track_settings(ui: &EditorWindow, tracks: [AudioTrackSettings; 4]) {
    ui.set_track_v2_gain_db(tracks[0].gain_db);
    ui.set_track_v2_muted(tracks[0].muted);
    ui.set_track_v2_solo(tracks[0].solo);
    ui.set_track_v1_gain_db(tracks[1].gain_db);
    ui.set_track_v1_muted(tracks[1].muted);
    ui.set_track_v1_solo(tracks[1].solo);
    ui.set_track_a1_gain_db(tracks[2].gain_db);
    ui.set_track_a1_muted(tracks[2].muted);
    ui.set_track_a1_solo(tracks[2].solo);
    ui.set_track_a2_gain_db(tracks[3].gain_db);
    ui.set_track_a2_muted(tracks[3].muted);
    ui.set_track_a2_solo(tracks[3].solo);
}

fn clear_timeline_audio(audio: &Arc<Mutex<Option<render::TimelineAudioPlayback>>>) {
    audio
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
}

fn clear_timeline_clock(clock: &Arc<Mutex<Option<(f32, Instant)>>>) {
    clock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
}

fn render_clip_snapshot(ui: &EditorWindow) -> Vec<render::RenderClip> {
    let tracks = audio_track_settings(ui);
    let any_solo = tracks.iter().any(|track| track.solo);
    let clips: Vec<render::RenderClip> = ui
        .get_clips()
        .iter()
        .map(|clip| {
            let track = tracks[clip.track.clamp(0, 3) as usize];
            render::RenderClip {
                path: PathBuf::from(clip.path.as_str()),
                title_text: clip.title_text.to_string(),
                duration: clip.duration_seconds,
                start: clip.start_seconds,
                source_in: clip.source_in_seconds,
                source_out: clip.source_out_seconds,
                track: clip.track,
                exposure: clip.exposure,
                contrast: clip.contrast,
                saturation: clip.saturation,
                opacity: clip.opacity,
                audio_gain_db: clip.audio_gain_db,
                track_gain_db: track.gain_db,
                track_muted: track.muted,
                track_solo: track.solo,
                solo_mode: any_solo,
                blur: clip.blur,
                sharpen: clip.sharpen,
                fade_in: clip.transition_in_seconds,
                fade_out: clip.transition_out_seconds,
                scale: clip.scale,
                rotation: clip.rotation,
                animate_scale: clip.animate_scale,
                scale_end: clip.scale_end,
            }
        })
        .collect();
    // Represent the current flat timeline as the implicit root sequence. Once
    // the sequence UI is available, nested placements resolve through this same
    // flattening boundary before preview, meters, and export consume snapshots.
    let root = sequence::Sequence {
        id: "root".to_owned(),
        items: clips
            .iter()
            .cloned()
            .map(sequence::SequenceItem::Clip)
            .collect(),
    };
    sequence::flatten("root", &HashMap::from([("root".to_owned(), root)])).unwrap_or(clips)
}

/// Prefer a generated proxy for interactive preview only. Export snapshots keep
/// the original media path so proxy quality never limits final output.
fn preview_media_path(source: &str) -> String {
    proxy::proxy_path_for(source)
        .ok()
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(source))
        .to_string_lossy()
        .into_owned()
}

fn render_snapshot_signature(clips: &[render::RenderClip]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for clip in clips {
        clip.path.hash(&mut hasher);
        for value in [
            clip.duration,
            clip.start,
            clip.source_in,
            clip.source_out,
            clip.exposure,
            clip.contrast,
            clip.saturation,
            clip.opacity,
            clip.audio_gain_db,
            clip.track_gain_db,
            clip.blur,
            clip.sharpen,
            clip.fade_in,
            clip.fade_out,
        ] {
            value.to_bits().hash(&mut hasher);
        }
        clip.track.hash(&mut hasher);
        clip.track_muted.hash(&mut hasher);
        clip.track_solo.hash(&mut hasher);
        clip.solo_mode.hash(&mut hasher);
    }
    hasher.finish()
}

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn workspace_file() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)?
        .join("Library/Application Support/VideoPro");
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)?
        .join("VideoPro");
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?
        .join("videopro");
    Some(base.join("workspaces.tsv"))
}

fn encode_name(name: &str) -> String {
    name.as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_name(encoded: &str) -> Option<String> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    let bytes = (0..encoded.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&encoded[i..i + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes).ok()
}

#[derive(Clone)]
struct ProjectClipData {
    name: String,
    title_text: String,
    source_path: String,
    duration_seconds: f32,
    start_seconds: f32,
    source_in_seconds: f32,
    source_out_seconds: f32,
    track: i32,
    exposure: f32,
    contrast: f32,
    saturation: f32,
    opacity: f32,
    audio_gain_db: f32,
    blur: f32,
    sharpen: f32,
    transition_in_seconds: f32,
    transition_out_seconds: f32,
    scale: f32,
    rotation: f32,
    animate_scale: bool,
    scale_end: f32,
}

fn apply_clip_preview_settings(
    player: &VideoPlayer,
    clip: &Clip,
    _tracks: &[AudioTrackSettings; 4],
) {
    let _ = player
        .engine
        .set_property("brightness", clip.exposure as f64);
    let _ = player
        .engine
        .set_property("contrast", ((clip.contrast - 1.0) * 100.0) as f64);
    let _ = player
        .engine
        .set_property("saturation", ((clip.saturation - 1.0) * 100.0) as f64);
    // MPV supplies the selected clip's video decoder only. Timeline audio is
    // mixed and played by TimelineAudioPlayback so overlapping tracks never
    // double-play or bypass track mute/solo routing.
    let _ = player.engine.set_property("volume", 0.0_f64);

    let mut filters = Vec::new();
    if clip.transition_in_seconds > 0.001 {
        filters.push(format!(
            "fade=t=in:st={:.3}:d={:.3}",
            clip.source_in_seconds, clip.transition_in_seconds
        ));
    }
    if clip.transition_out_seconds > 0.001 {
        filters.push(format!(
            "fade=t=out:st={:.3}:d={:.3}",
            (clip.source_out_seconds - clip.transition_out_seconds).max(0.0),
            clip.transition_out_seconds
        ));
    }
    if clip.opacity < 0.999 {
        filters.push(format!("colorchannelmixer=aa={:.3}", clip.opacity));
    }
    if clip.blur > 0.001 {
        filters.push(format!("boxblur={:.3}:1", clip.blur.clamp(0.0, 10.0)));
    }
    if clip.sharpen > 0.001 {
        filters.push(format!(
            "unsharp=5:5:{:.3}:5:5:0",
            clip.sharpen.clamp(0.0, 5.0)
        ));
    }
    if filters.is_empty() {
        let _ = player.engine.command("vf", &["clr"]);
    } else {
        let filter = format!("lavfi=[{}]", filters.join(","));
        let _ = player.engine.command("vf", &["set", filter.as_str()]);
    }
}

fn project_storage_dir() -> Option<PathBuf> {
    workspace_file()?.parent().map(Path::to_path_buf)
}

fn recent_projects_file() -> Option<PathBuf> {
    Some(project_storage_dir()?.join("recent-projects.tsv"))
}

fn load_recent_projects() -> Vec<RecentProject> {
    let Some(path) = recent_projects_file() else {
        return Vec::new();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .take(MAX_RECENT_PROJECTS)
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = decode_name(fields.next()?)?;
            let path = decode_name(fields.next()?)?;
            let path_buf = PathBuf::from(&path);
            path_buf.is_file().then(|| RecentProject {
                name: name.into(),
                path: path.into(),
                location: path_buf
                    .parent()
                    .unwrap_or(Path::new(""))
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            })
        })
        .collect()
}

fn remember_recent_project(path: &Path, name: &str) {
    let Some(file) = recent_projects_file() else {
        return;
    };
    let mut recent = load_recent_projects();
    let path_string = path.to_string_lossy().into_owned();
    recent.retain(|project| project.path.as_str() != path_string);
    recent.insert(
        0,
        RecentProject {
            name: name.into(),
            path: path_string.into(),
            location: path
                .parent()
                .unwrap_or(Path::new(""))
                .to_string_lossy()
                .into_owned()
                .into(),
        },
    );
    recent.truncate(MAX_RECENT_PROJECTS);
    if let Some(parent) = file.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let contents = recent
        .iter()
        .map(|project| {
            format!(
                "{}\t{}\n",
                encode_name(&project.name),
                encode_name(&project.path)
            )
        })
        .collect::<String>();
    let temporary = file.with_extension("tsv.tmp");
    if fs::write(&temporary, contents).is_ok() {
        let _ = fs::rename(temporary, file);
    }
}

fn safe_extension(path: &Path) -> String {
    path.extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            !extension.is_empty() && extension.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .map(|extension| format!(".{extension}"))
        .unwrap_or_default()
}

fn save_project_bundle(
    path: &Path,
    project_name: &str,
    clips: &[ProjectClipData],
    tracks: &[AudioTrackSettings; 4],
) -> std::io::Result<()> {
    for clip in clips {
        if !Path::new(&clip.source_path).is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Media file is missing: {}", clip.source_path),
            ));
        }
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project.vpproj");
    let save_id = NEXT_PROJECT_SAVE_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_file_name(format!(
        ".{file_name}.videopro-save-{}-{save_id}.tmp",
        std::process::id()
    ));
    let _temporary_cleanup = TemporaryFile(temporary.clone());
    let file = fs::File::create(&temporary)?;
    let mut archive = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut manifest = format!("VIDEOPRO_BUNDLE\t7\nNAME\t{}\n", encode_name(project_name));
    for (index, track) in tracks.iter().enumerate() {
        manifest.push_str(&format!(
            "TRACK\t{index}\t{:.6}\t{}\t{}\n",
            track.gain_db,
            u8::from(track.muted),
            u8::from(track.solo)
        ));
    }

    for (index, clip) in clips.iter().enumerate() {
        let source = Path::new(&clip.source_path);
        let archive_path = format!("media/{index:06}{}", safe_extension(source));
        archive.start_file(&archive_path, stored)?;
        let mut input = fs::File::open(source)?;
        std::io::copy(&mut input, &mut archive)?;
        manifest.push_str(&format!(
            "CLIP\t{}\t{}\t{}\t{:.6}\t{:.6}\t{:.6}\t{:.6}\t{}\t{:.6}\t{:.6}\t{:.6}\t{:.6}\t{:.6}\t{:.6}\t{:.6}\t{:.6}\t{:.6}\t{}\t{:.6}\t{:.6}\t{}\t{:.6}\n",
            encode_name(&clip.name),
            encode_name(&clip.source_path),
            encode_name(&archive_path),
            clip.duration_seconds,
            clip.start_seconds,
            clip.source_in_seconds,
            clip.source_out_seconds,
            clip.track,
            clip.exposure,
            clip.contrast,
            clip.saturation,
            clip.opacity,
            clip.transition_in_seconds,
            clip.transition_out_seconds,
            clip.audio_gain_db,
            clip.blur,
            clip.sharpen,
            encode_name(&clip.title_text),
            clip.scale,
            clip.rotation,
            u8::from(clip.animate_scale),
            clip.scale_end,
        ));
    }

    archive.start_file("project.vpm", stored)?;
    archive.write_all(manifest.as_bytes())?;
    let file = archive.finish()?;
    file.sync_all()?;
    drop(file);
    if path.exists() {
        let backup = path.with_extension("vpproj.bak");
        if backup.exists() {
            fs::remove_file(&backup)?;
        }
        fs::rename(path, &backup)?;
        match fs::rename(&temporary, path) {
            Ok(()) => {
                let _ = fs::remove_file(backup);
            }
            Err(error) => {
                let _ = fs::rename(&backup, path);
                return Err(error);
            }
        }
    } else {
        fs::rename(temporary, path)?;
    }
    Ok(())
}

fn resolve_project_source(path: &Path) -> Result<PathBuf, String> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let backup = path.with_extension("vpproj.bak");
    if backup.is_file() {
        return Ok(backup);
    }
    Err(format!("Project file is missing: {}", path.display()))
}

fn load_project_bundle(
    path: &Path,
) -> Result<(String, Vec<ProjectClipData>, [AudioTrackSettings; 4]), String> {
    let storage_root = project_storage_dir()
        .ok_or_else(|| "Could not determine VideoPro's project storage directory".to_owned())?
        .join("Projects");
    load_project_bundle_into(path, &storage_root)
}

fn load_project_bundle_into(
    path: &Path,
    storage_root: &Path,
) -> Result<(String, Vec<ProjectClipData>, [AudioTrackSettings; 4]), String> {
    let source_path = resolve_project_source(path)?;
    let file = fs::File::open(&source_path).map_err(|error| error.to_string())?;
    let mut archive = ZipArchive::new(file)
        .map_err(|error| format!("Not a valid .vpproj ZIP bundle: {error}"))?;
    const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
    const MAX_MEDIA_ENTRY_BYTES: u64 = 10 * 1024 * 1024 * 1024;
    const MAX_TOTAL_MEDIA_BYTES: u64 = 20 * 1024 * 1024 * 1024;
    const MAX_CLIPS: usize = 10_000;
    if archive.len() > MAX_CLIPS + 1024 {
        return Err("Project bundle contains too many ZIP entries".to_owned());
    }
    let mut manifest = String::new();
    {
        let entry = archive
            .by_name("project.vpm")
            .map_err(|_| "Project bundle has no project.vpm manifest".to_owned())?;
        if entry.size() > MAX_MANIFEST_BYTES {
            return Err("Project manifest exceeds the 16 MiB safety limit".to_owned());
        }
        entry
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_string(&mut manifest)
            .map_err(|error| error.to_string())?;
        if manifest.len() as u64 > MAX_MANIFEST_BYTES {
            return Err("Project manifest exceeds the 16 MiB safety limit".to_owned());
        }
    }

    let mut lines = manifest.lines();
    let version = match lines.next() {
        Some("VIDEOPRO_BUNDLE\t1") => 1,
        Some("VIDEOPRO_BUNDLE\t2") => 2,
        Some("VIDEOPRO_BUNDLE\t3") => 3,
        Some("VIDEOPRO_BUNDLE\t4") => 4,
        Some("VIDEOPRO_BUNDLE\t5") => 5,
        Some("VIDEOPRO_BUNDLE\t6") => 6,
        Some("VIDEOPRO_BUNDLE\t7") => 7,
        _ => return Err("Unsupported VideoPro project version".to_owned()),
    };
    let project_name = lines
        .next()
        .and_then(|line| line.strip_prefix("NAME\t"))
        .and_then(decode_name)
        .ok_or_else(|| "Project manifest has no valid name".to_owned())?;

    let remaining_lines: Vec<&str> = lines.collect();
    let mut track_settings = [AudioTrackSettings::default(); 4];
    if version >= 4 {
        for line in &remaining_lines {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.first() != Some(&"TRACK") {
                continue;
            }
            if fields.len() != 5 {
                return Err("Malformed track audio settings in project manifest".to_owned());
            }
            let index = fields[1]
                .parse::<usize>()
                .ok()
                .filter(|index| *index < track_settings.len())
                .ok_or_else(|| "Invalid audio track index in project manifest".to_owned())?;
            let gain_db = fields[2]
                .parse::<f32>()
                .ok()
                .filter(|value| value.is_finite() && (-24.0..=12.0).contains(value))
                .ok_or_else(|| "Invalid audio track gain in project manifest".to_owned())?;
            let parse_bool = |value: &str| match value {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            };
            track_settings[index] = AudioTrackSettings {
                gain_db,
                muted: parse_bool(fields[3])
                    .ok_or_else(|| "Invalid audio track mute state".to_owned())?,
                solo: parse_bool(fields[4])
                    .ok_or_else(|| "Invalid audio track solo state".to_owned())?,
            };
        }
    }

    fs::create_dir_all(storage_root).map_err(|error| error.to_string())?;
    let mut hasher = DefaultHasher::new();
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .hash(&mut hasher);
    let project_key = format!("{:016x}", hasher.finish());
    let extraction_id = NEXT_EXTRACTION_ID.fetch_add(1, Ordering::Relaxed);
    let extraction_name = format!("{project_key}-{}-{extraction_id}", std::process::id());
    let extraction_root = storage_root.join(format!("{extraction_name}.staging"));
    let final_root = storage_root.join(extraction_name);
    fs::create_dir_all(&extraction_root).map_err(|error| error.to_string())?;

    let mut clips = Vec::new();
    let mut total_media_bytes = 0_u64;
    let load_result = (|| -> Result<(), String> {
        for (index, line) in remaining_lines.iter().enumerate() {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.first() != Some(&"CLIP") {
                continue;
            }
            if fields.len()
                != if version == 1 {
                    6
                } else if version == 2 {
                    15
                } else if version <= 4 {
                    18
                } else if version == 5 {
                    19
                } else if version == 6 {
                    21
                } else {
                    23
                }
            {
                return Err(format!("Malformed clip entry {}", index + 1));
            }
            if clips.len() >= MAX_CLIPS {
                return Err(format!("Project exceeds the {MAX_CLIPS}-clip safety limit"));
            }
            let name =
                decode_name(fields[1]).ok_or_else(|| "Invalid clip name in manifest".to_owned())?;
            let original_path = decode_name(fields[2])
                .ok_or_else(|| "Invalid source path in manifest".to_owned())?;
            let archive_path = decode_name(fields[3])
                .ok_or_else(|| "Invalid media path in manifest".to_owned())?;
            if !archive_path.starts_with("media/")
                || archive_path.split('/').any(|part| part == "..")
            {
                return Err("Unsafe media path in project manifest".to_owned());
            }
            let number = |index: usize| -> Result<f32, String> {
                fields[index]
                    .parse::<f32>()
                    .ok()
                    .filter(|value| value.is_finite())
                    .ok_or_else(|| format!("Invalid numeric value in clip entry {}", index + 1))
            };
            let duration = number(4)?;
            let start = number(5)?;
            let (
                source_in,
                source_out,
                track,
                exposure,
                contrast,
                saturation,
                opacity,
                transition_in,
                transition_out,
            ) = if version == 1 {
                (0.0, duration, 1, 0.0, 1.0, 1.0, 1.0, 0.0, 0.0)
            } else {
                let track = fields[8]
                    .parse::<i32>()
                    .ok()
                    .filter(|value| (0..=3).contains(value))
                    .ok_or_else(|| "Invalid track index in manifest".to_owned())?;
                (
                    number(6)?,
                    number(7)?,
                    track,
                    number(9)?,
                    number(10)?,
                    number(11)?,
                    number(12)?,
                    number(13)?,
                    number(14)?,
                )
            };
            let (audio_gain_db, blur, sharpen) = if version >= 3 {
                (number(15)?, number(16)?, number(17)?)
            } else {
                (0.0, 0.0, 0.0)
            };
            let title_text = if version >= 5 {
                decode_name(fields[18])
                    .ok_or_else(|| "Invalid title text in project manifest".to_owned())?
            } else {
                String::new()
            };
            let (scale, rotation) = if version >= 6 {
                (number(19)?, number(20)?)
            } else {
                (1.0, 0.0)
            };
            let (animate_scale, scale_end) = if version >= 7 {
                let animated = match fields[21] {
                    "0" => false,
                    "1" => true,
                    _ => return Err("Invalid scale animation flag in manifest".to_owned()),
                };
                (animated, number(22)?)
            } else {
                (false, scale)
            };
            if duration <= 0.0
                || start < 0.0
                || source_in < 0.0
                || source_out <= source_in
                || (source_out - source_in - duration).abs() > 0.25
                || transition_in < 0.0
                || transition_out < 0.0
                || transition_in > duration
                || transition_out > duration
                || !(0.0..=2.0).contains(&contrast)
                || !(0.0..=3.0).contains(&saturation)
                || !(0.0..=1.0).contains(&opacity)
                || !(-24.0..=12.0).contains(&audio_gain_db)
                || !(0.0..=10.0).contains(&blur)
                || !(0.0..=5.0).contains(&sharpen)
                || !(0.25..=3.0).contains(&scale)
                || !(-180.0..=180.0).contains(&rotation)
                || !(0.25..=3.0).contains(&scale_end)
            {
                return Err("Invalid clip timing in project manifest".to_owned());
            }

            let extension = safe_extension(Path::new(&archive_path));
            let extracted_path = extraction_root.join(format!("{index:06}{extension}"));
            let mut output =
                fs::File::create(&extracted_path).map_err(|error| error.to_string())?;
            match archive.by_name(&archive_path) {
                Ok(mut input) => {
                    if input.size() > MAX_MEDIA_ENTRY_BYTES {
                        return Err(format!(
                            "Embedded media exceeds the 10 GiB safety limit: {name}"
                        ));
                    }
                    total_media_bytes = total_media_bytes
                        .checked_add(input.size())
                        .filter(|size| *size <= MAX_TOTAL_MEDIA_BYTES)
                        .ok_or_else(|| {
                            "Project exceeds the 20 GiB total embedded media limit".to_owned()
                        })?;
                    let expected_size = input.size();
                    let copied = std::io::copy(&mut input, &mut output)
                        .map_err(|error| error.to_string())?;
                    if copied != expected_size {
                        return Err(format!("Embedded media was truncated: {name}"));
                    }
                }
                Err(_)
                    if Path::new(&original_path).is_file()
                        && Path::new(&original_path).file_name()
                            == Path::new(&name).file_name()
                        && safe_extension(Path::new(&original_path))
                            == safe_extension(Path::new(&archive_path)) =>
                {
                    // If an archive entry was lost during an interrupted/corrupt
                    // copy, recover from the original source when it still exists.
                    let source = Path::new(&original_path);
                    let size = fs::metadata(source)
                        .map_err(|error| error.to_string())?
                        .len();
                    if size > MAX_MEDIA_ENTRY_BYTES {
                        return Err(format!("Relinked media exceeds the 10 GiB limit: {name}"));
                    }
                    total_media_bytes = total_media_bytes
                        .checked_add(size)
                        .filter(|size| *size <= MAX_TOTAL_MEDIA_BYTES)
                        .ok_or_else(|| "Project exceeds the 20 GiB total media limit".to_owned())?;
                    let mut input = fs::File::open(source).map_err(|error| error.to_string())?;
                    let copied = std::io::copy(&mut input, &mut output)
                        .map_err(|error| error.to_string())?;
                    if copied != size {
                        return Err(format!("Relinked media changed while copying: {name}"));
                    }
                }
                Err(_) => {
                    return Err(format!(
                        "Project is missing embedded media '{name}', and its original file is unavailable"
                    ));
                }
            }
            clips.push(ProjectClipData {
                name,
                title_text,
                source_path: extracted_path.to_string_lossy().into_owned(),
                duration_seconds: duration,
                start_seconds: start,
                source_in_seconds: source_in,
                source_out_seconds: source_out,
                track,
                exposure,
                contrast,
                saturation,
                opacity,
                audio_gain_db,
                blur,
                sharpen,
                transition_in_seconds: transition_in,
                transition_out_seconds: transition_out,
                scale,
                rotation,
                animate_scale,
                scale_end,
            });
        }
        Ok(())
    })();
    if let Err(error) = load_result {
        let _ = fs::remove_dir_all(&extraction_root);
        return Err(error);
    }
    fs::rename(&extraction_root, &final_root).map_err(|error| {
        let _ = fs::remove_dir_all(&extraction_root);
        error.to_string()
    })?;
    for clip in &mut clips {
        if let Ok(relative) = Path::new(&clip.source_path).strip_prefix(&extraction_root) {
            clip.source_path = final_root.join(relative).to_string_lossy().into_owned();
        }
    }
    Ok((project_name, clips, track_settings))
}

fn load_workspace_layouts() -> Vec<WorkspaceLayout> {
    let Some(path) = workspace_file() else {
        return Vec::new();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .take(MAX_WORKSPACE_LAYOUTS)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() != 9 && fields.len() != 16 {
                return None;
            }
            let name = decode_name(fields[0])?;
            let boolean = |value: &str| match value {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            };
            let timeline_height = fields[5].parse::<f32>().ok()?;
            let left_panel_width = fields[6].parse::<f32>().ok()?;
            let right_panel_width = fields[7].parse::<f32>().ok()?;
            let timeline_zoom = fields[8].parse::<f32>().ok()?;
            if name.trim().is_empty()
                || ![
                    timeline_height,
                    left_panel_width,
                    right_panel_width,
                    timeline_zoom,
                ]
                .iter()
                .all(|v| v.is_finite())
            {
                return None;
            }
            let (
                active_mode,
                edit_exposure,
                edit_contrast,
                edit_saturation,
                edit_opacity,
                edit_transition_in,
                edit_transition_out,
            ) = if fields.len() == 16 {
                let mode = match fields[9] {
                    "Media" | "Editing" | "Color" | "Audio" | "Effects" => fields[9],
                    _ => "Editing",
                };
                let exposure = fields[10].parse::<f32>().ok()?;
                let contrast = fields[11].parse::<f32>().ok()?;
                let saturation = fields[12].parse::<f32>().ok()?;
                let opacity = fields[13].parse::<f32>().ok()?;
                let transition_in = fields[14].parse::<f32>().ok()?;
                let transition_out = fields[15].parse::<f32>().ok()?;
                if ![
                    exposure,
                    contrast,
                    saturation,
                    opacity,
                    transition_in,
                    transition_out,
                ]
                .iter()
                .all(|v| v.is_finite())
                {
                    return None;
                }
                (
                    mode,
                    exposure.clamp(-100.0, 100.0),
                    contrast.clamp(0.0, 2.0),
                    saturation.clamp(0.0, 2.0),
                    opacity.clamp(0.0, 1.0),
                    transition_in.clamp(0.0, 2.0),
                    transition_out.clamp(0.0, 2.0),
                )
            } else {
                ("Editing", 0.0, 1.0, 1.0, 1.0, 0.0, 0.0)
            };
            Some(WorkspaceLayout {
                name: name.into(),
                show_project_panel: boolean(fields[1])?,
                show_properties_panel: boolean(fields[2])?,
                show_effects_browser: boolean(fields[3])?,
                show_timeline: boolean(fields[4])?,
                timeline_height: timeline_height.clamp(180.0, 420.0),
                left_panel_width: left_panel_width.clamp(180.0, 320.0),
                right_panel_width: right_panel_width.clamp(200.0, 360.0),
                timeline_zoom: timeline_zoom.clamp(0.25, 2.0),
                active_mode: active_mode.into(),
                edit_exposure,
                edit_contrast,
                edit_saturation,
                edit_opacity,
                edit_transition_in,
                edit_transition_out,
            })
        })
        .collect()
}

fn save_workspace_layouts(layouts: &[WorkspaceLayout]) -> std::io::Result<()> {
    if layouts.len() > MAX_WORKSPACE_LAYOUTS {
        return Err(std::io::Error::other(format!(
            "A maximum of {MAX_WORKSPACE_LAYOUTS} workspace layouts can be saved"
        )));
    }
    let path = workspace_file()
        .ok_or_else(|| std::io::Error::other("Could not determine the configuration directory"))?;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("Invalid configuration path"))?;
    fs::create_dir_all(parent)?;
    let mut contents = String::new();
    for layout in layouts {
        contents.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            encode_name(layout.name.as_str()),
            u8::from(layout.show_project_panel),
            u8::from(layout.show_properties_panel),
            u8::from(layout.show_effects_browser),
            u8::from(layout.show_timeline),
            layout.timeline_height,
            layout.left_panel_width,
            layout.right_panel_width,
            layout.timeline_zoom,
            layout.active_mode,
            layout.edit_exposure,
            layout.edit_contrast,
            layout.edit_saturation,
            layout.edit_opacity,
            layout.edit_transition_in,
            layout.edit_transition_out
        ));
    }
    let temporary = path.with_extension("tsv.tmp");
    fs::write(&temporary, contents)?;
    fs::rename(temporary, path)
}

slint::include_modules!();

fn configure_editor(
    editor: &EditorWindow,
    player: Rc<VideoPlayer>,
    pending_timeline_seek: Rc<Cell<Option<f32>>>,
    pending_local_seek: Rc<Cell<Option<f64>>>,
    file_ready: Rc<Cell<bool>>,
    playback_state: TimelinePlaybackState,
) -> EditorRuntimeState {
    let timeline_clock = playback_state.clock;
    let timeline_audio = playback_state.audio;
    let audio_generation = playback_state.generation;
    editor.set_is_macos(cfg!(target_os = "macos"));
    let undo_stack = Rc::new(RefCell::new(Vec::<Vec<Clip>>::new()));
    let redo_stack = Rc::new(RefCell::new(Vec::<Vec<Clip>>::new()));
    let drag_snapshot = Rc::new(RefCell::new(None::<Vec<Clip>>));
    let project_generation = Arc::new(AtomicU64::new(0));
    let project_reset_requested = Arc::new(AtomicBool::new(false));

    editor.on_new_project({
        let weak = editor.as_weak();
        let undo = Rc::clone(&undo_stack);
        let redo = Rc::clone(&redo_stack);
        let generation = Arc::clone(&project_generation);
        let player = Rc::clone(&player);
        let pending_seek = Rc::clone(&pending_timeline_seek);
        let pending_local = Rc::clone(&pending_local_seek);
        let file_ready = Rc::clone(&file_ready);
        let drag = Rc::clone(&drag_snapshot);
        let clock = Arc::clone(&timeline_clock);
        let audio = Arc::clone(&timeline_audio);
        let audio_generation = Arc::clone(&audio_generation);
        move || {
            if let Some(ui) = weak.upgrade() {
                generation.fetch_add(1, Ordering::AcqRel);
                pending_seek.set(None);
                pending_local.set(None);
                file_ready.set(false);
                audio_generation.fetch_add(1, Ordering::AcqRel);
                clear_timeline_clock(&clock);
                clear_timeline_audio(&audio);
                drag.borrow_mut().take();
                let _ = player.engine.stop();
                ui.set_is_playing(false);
                ui.set_clips(Rc::new(slint::VecModel::from(Vec::<Clip>::new())).into());
                ui.set_project_name("Untitled Project".into());
                ui.set_selected_clip("".into());
                ui.set_video_frame(Image::default());
                ui.set_has_video_frame(false);
                ui.set_playhead_seconds(0.0);
                ui.set_playhead_time("00:00:00.000".into());
                ui.set_timeline_playhead_seconds(0.0);
                ui.set_video_duration(0.0);
                ui.set_video_duration_time("--:--".into());
                ui.set_timeline_duration(10.0);
                set_audio_track_settings(&ui, [AudioTrackSettings::default(); 4]);
                undo.borrow_mut().clear();
                redo.borrow_mut().clear();
                ui.set_status_message("New project".into());
            }
        }
    });

    editor.on_save_project({
        let weak = editor.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(path) = rfd::FileDialog::new()
                .set_title("Save VideoPro Project")
                .add_filter("VideoPro Project", &["vpproj"])
                .set_file_name("Untitled.vpproj")
                .save_file()
            else {
                return;
            };
            let project_name = ui.get_project_name().to_string();
            let tracks = audio_track_settings(&ui);
            let clips: Vec<ProjectClipData> = ui
                .get_clips()
                .iter()
                .map(|clip| ProjectClipData {
                    name: clip.name.to_string(),
                    title_text: clip.title_text.to_string(),
                    source_path: clip.path.to_string(),
                    duration_seconds: clip.duration_seconds,
                    start_seconds: clip.start_seconds,
                    source_in_seconds: clip.source_in_seconds,
                    source_out_seconds: clip.source_out_seconds,
                    track: clip.track,
                    exposure: clip.exposure,
                    contrast: clip.contrast,
                    saturation: clip.saturation,
                    opacity: clip.opacity,
                    audio_gain_db: clip.audio_gain_db,
                    blur: clip.blur,
                    sharpen: clip.sharpen,
                    transition_in_seconds: clip.transition_in_seconds,
                    transition_out_seconds: clip.transition_out_seconds,
                    scale: clip.scale,
                    rotation: clip.rotation,
                    animate_scale: clip.animate_scale,
                    scale_end: clip.scale_end,
                })
                .collect();
            ui.set_status_message(format!("Packaging {} media file(s)…", clips.len()).into());
            drop(ui);

            let weak = weak.clone();
            thread::spawn(move || {
                let result = save_project_bundle(&path, &project_name, &clips, &tracks);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        match result {
                            Ok(()) => {
                                remember_recent_project(&path, &project_name);
                                ui.set_status_message(
                                    format!(
                                        "Project bundle saved: {}",
                                        path.file_name()
                                            .and_then(|n| n.to_str())
                                            .unwrap_or("project.vpproj")
                                    )
                                    .into(),
                                )
                            }
                            Err(error) => ui.set_status_message(
                                format!("Could not save project bundle: {error}").into(),
                            ),
                        }
                    }
                });
            });
        }
    });

    editor.on_open_project({
        let weak = editor.as_weak();
        let generation = Arc::clone(&project_generation);
        let reset_requested = Arc::clone(&project_reset_requested);
        move || {
            let path = PENDING_PROJECT_PATH
                .lock()
                .ok()
                .and_then(|mut path| path.take())
                .or_else(|| {
                    rfd::FileDialog::new()
                        .set_title("Open VideoPro Project")
                        .add_filter("VideoPro Project", &["vpproj"])
                        .pick_file()
                });
            let Some(path) = path else { return };
            remember_recent_project(
                &path,
                path.file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Untitled Project"),
            );
            let request_generation = generation.fetch_add(1, Ordering::AcqRel) + 1;
            if let Some(ui) = weak.upgrade() {
                ui.set_status_message("Opening project bundle and unpacking media…".into());
            }
            let weak = weak.clone();
            let generation_for_load = Arc::clone(&generation);
            let reset_for_load = Arc::clone(&reset_requested);
            thread::spawn(move || {
                let result = load_project_bundle(&path);
                let mut load_error = None;
                let prepared = match result {
                    Ok((project_name, entries, tracks)) => {
                        let mut imported = Vec::with_capacity(entries.len());
                        for (index, clip) in entries.into_iter().enumerate() {
                            let media_path = PathBuf::from(&clip.source_path);
                            let probed = probe_duration(&media_path);
                            let source_duration =
                                probed.unwrap_or(clip.source_out_seconds as f64) as f32;
                            let source_in = clip
                                .source_in_seconds
                                .clamp(0.0, (source_duration - 0.05).max(0.0));
                            let source_out = clip
                                .source_out_seconds
                                .min(source_duration)
                                .max((source_in + 0.05).min(source_duration));
                            let duration_seconds = (source_out - source_in).max(0.05);
                            let thumbnails = [
                                make_thumbnail(
                                    &media_path,
                                    duration_seconds as f64 * 0.05,
                                    index * 3,
                                ),
                                make_thumbnail(
                                    &media_path,
                                    duration_seconds as f64 * 0.5,
                                    index * 3 + 1,
                                ),
                                make_thumbnail(
                                    &media_path,
                                    duration_seconds as f64 * 0.9,
                                    index * 3 + 2,
                                ),
                            ];
                            let waveform = render::has_audio(&media_path)
                                .then(|| make_waveform(&media_path))
                                .flatten();
                            imported.push((
                                clip,
                                media_path.to_string_lossy().into_owned(),
                                source_duration,
                                source_in,
                                source_out,
                                duration_seconds,
                                thumbnails,
                                waveform,
                            ));
                        }
                        Some((project_name, imported, tracks))
                    }
                    Err(error) => {
                        load_error = Some(error);
                        None
                    }
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    if generation_for_load.load(Ordering::Acquire) != request_generation {
                        return;
                    }
                    if let Some(error) = load_error {
                        ui.set_status_message(format!("Could not open project: {error}").into());
                        return;
                    }
                    let Some((project_name, imported, tracks)) = prepared else {
                        return;
                    };
                    let mut clips = Vec::with_capacity(imported.len());
                    let mut end = 10.0_f32;
                    for (
                        saved,
                        path,
                        source_duration,
                        source_in,
                        source_out,
                        duration_seconds,
                        thumbnails,
                        waveform,
                    ) in imported
                    {
                        end = end.max(saved.start_seconds + duration_seconds);
                        clips.push(Clip {
                            name: saved.name.into(),
                            title_text: saved.title_text.into(),
                            path: path.into(),
                            duration: format_time(duration_seconds as f64).into(),
                            duration_seconds,
                            start_seconds: saved.start_seconds,
                            source_in_seconds: source_in,
                            source_out_seconds: source_out.min(source_duration),
                            track: saved.track,
                            exposure: saved.exposure,
                            contrast: saved.contrast,
                            saturation: saved.saturation,
                            opacity: saved.opacity,
                            audio_gain_db: saved.audio_gain_db,
                            blur: saved.blur,
                            sharpen: saved.sharpen,
                            transition_in_seconds: saved.transition_in_seconds,
                            transition_out_seconds: saved.transition_out_seconds,
                            scale: saved.scale,
                            rotation: saved.rotation,
                            animate_scale: saved.animate_scale,
                            scale_end: saved.scale_end,
                            thumbnail_a: thumbnails[0]
                                .as_ref()
                                .and_then(|p| Image::load_from_path(p).ok())
                                .unwrap_or_default(),
                            thumbnail_b: thumbnails[1]
                                .as_ref()
                                .and_then(|p| Image::load_from_path(p).ok())
                                .unwrap_or_default(),
                            thumbnail_c: thumbnails[2]
                                .as_ref()
                                .and_then(|p| Image::load_from_path(p).ok())
                                .unwrap_or_default(),
                            waveform: waveform
                                .as_ref()
                                .and_then(|p| Image::load_from_path(p).ok())
                                .unwrap_or_default(),
                            color: slint::Color::from_rgb_u8(55, 91, 135),
                        });
                    }
                    ui.set_project_name(project_name.into());
                    set_audio_track_settings(&ui, tracks);
                    reset_for_load.store(true, Ordering::Release);
                    ui.set_clips(Rc::new(slint::VecModel::from(clips)).into());
                    ui.set_selected_clip("".into());
                    ui.set_video_frame(Image::default());
                    ui.set_has_video_frame(false);
                    ui.set_playhead_seconds(0.0);
                    ui.set_timeline_playhead_seconds(0.0);
                    ui.set_playhead_time("00:00:00.000".into());
                    ui.set_video_duration(0.0);
                    ui.set_video_duration_time("--:--".into());
                    ui.set_timeline_duration(end);
                    ui.set_status_message("Project bundle opened".into());
                });
            });
        }
    });

    editor.on_import_media({
        let weak = editor.as_weak();
        let undo = Rc::clone(&undo_stack);
        let redo = Rc::clone(&redo_stack);
        let generation = Arc::clone(&project_generation);

        move || {
            let Some(files) = pick_media_files() else {
                return;
            };
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut seen_paths: HashSet<String> = ui
                .get_clips()
                .iter()
                .map(|clip| media_path_identity(Path::new(clip.path.as_str())))
                .collect();
            let files: Vec<PathBuf> = files
                .into_iter()
                .filter(|path| seen_paths.insert(media_path_identity(path)))
                .collect();
            if files.is_empty() {
                ui.set_status_message("All selected files are already in the media library".into());
                return;
            }
            let base_index = ui.get_clips().row_count();
            let import_generation = generation.load(Ordering::Acquire);
            ui.set_status_message(format!("Analyzing {} file(s)…", files.len()).into());
            if !files.is_empty() {
                undo.borrow_mut().push(ui.get_clips().iter().collect());
                if undo.borrow().len() > 100 {
                    undo.borrow_mut().remove(0);
                }
                redo.borrow_mut().clear();
            }
            drop(ui);

            let weak = weak.clone();
            let generation_for_import = Arc::clone(&generation);
            thread::spawn(move || {
                let mut imported = Vec::with_capacity(files.len());
                for (index, path) in files.into_iter().enumerate() {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("Untitled")
                        .to_owned();
                    let probed_duration = probe_duration(&path);
                    let duration_seconds = probed_duration.unwrap_or(10.0) as f32;
                    let track = if render::has_video(&path) { 1 } else { 2 };
                    let thumbnail_a = make_thumbnail(
                        &path,
                        duration_seconds as f64 * 0.05,
                        (base_index + index) * 3,
                    );
                    let thumbnail_b = make_thumbnail(
                        &path,
                        duration_seconds as f64 * 0.5,
                        (base_index + index) * 3 + 1,
                    );
                    let thumbnail_c = make_thumbnail(
                        &path,
                        duration_seconds as f64 * 0.9,
                        (base_index + index) * 3 + 2,
                    );
                    let waveform = render::has_audio(&path)
                        .then(|| make_waveform(&path))
                        .flatten();
                    imported.push(ImportedClip {
                        name,
                        path: path.to_string_lossy().into_owned(),
                        duration: probed_duration
                            .map(format_time)
                            .unwrap_or_else(|| "10s estimate".to_owned()),
                        duration_seconds,
                        track,
                        thumbnails: [thumbnail_a, thumbnail_b, thumbnail_c],
                        waveform,
                    });
                }

                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    if generation_for_import.load(Ordering::Acquire) != import_generation {
                        return;
                    }
                    let imported_count = imported.len();
                    let mut clips: Vec<Clip> = ui.get_clips().iter().collect();
                    let mut next_start_by_track = [0.0_f32; 4];
                    for clip in &clips {
                        let track = clip.track.clamp(0, 3) as usize;
                        next_start_by_track[track] = next_start_by_track[track]
                            .max(clip.start_seconds + clip.duration_seconds);
                    }
                    clips.extend(imported.into_iter().map(|clip| {
                        let track = clip.track.clamp(0, 3);
                        let start_seconds = next_start_by_track[track as usize];
                        next_start_by_track[track as usize] += clip.duration_seconds;
                        Clip {
                            name: clip.name.into(),
                            title_text: "".into(),
                            path: clip.path.into(),
                            duration: clip.duration.into(),
                            duration_seconds: clip.duration_seconds,
                            start_seconds,
                            source_in_seconds: 0.0,
                            source_out_seconds: clip.duration_seconds,
                            track,
                            exposure: 0.0,
                            contrast: 1.0,
                            saturation: 1.0,
                            opacity: 1.0,
                            audio_gain_db: 0.0,
                            blur: 0.0,
                            sharpen: 0.0,
                            transition_in_seconds: 0.0,
                            transition_out_seconds: 0.0,
                            scale: 1.0,
                            rotation: 0.0,
                            animate_scale: false,
                            scale_end: 1.0,
                            thumbnail_a: clip.thumbnails[0]
                                .as_ref()
                                .and_then(|path| Image::load_from_path(path).ok())
                                .unwrap_or_default(),
                            thumbnail_b: clip.thumbnails[1]
                                .as_ref()
                                .and_then(|path| Image::load_from_path(path).ok())
                                .unwrap_or_default(),
                            thumbnail_c: clip.thumbnails[2]
                                .as_ref()
                                .and_then(|path| Image::load_from_path(path).ok())
                                .unwrap_or_default(),
                            waveform: clip
                                .waveform
                                .as_ref()
                                .and_then(|path| Image::load_from_path(path).ok())
                                .unwrap_or_default(),
                            color: slint::Color::from_rgb_u8(55, 91, 135),
                        }
                    }));
                    let start = clips
                        .iter()
                        .map(|clip| clip.start_seconds + clip.duration_seconds)
                        .fold(0.0_f32, f32::max);
                    ui.set_timeline_duration(if start > 0.0 { start } else { 10.0 });
                    ui.set_clips(Rc::new(slint::VecModel::from(clips)).into());
                    ui.set_status_message(
                        format!("Added {imported_count} media file(s) to the project").into(),
                    );
                });
            });
        }
    });

    editor.on_relink_media({
        let weak = editor.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let selected_path = ui.get_selected_clip().to_string();
            if selected_path.is_empty() {
                ui.set_status_message("Select a media item to relink first".into());
                return;
            }
            let Some(replacement) = rfd::FileDialog::new()
                .set_title("Relink Missing Media")
                .add_filter(
                    "Video and audio",
                    &[
                        "mp4", "mov", "m4v", "mkv", "webm", "avi", "mpeg", "mpg", "mts", "m2ts",
                        "ts", "flv", "ogv", "3gp", "mp3", "wav", "m4a", "aac", "flac", "ogg",
                        "opus", "aiff", "wma",
                    ],
                )
                .pick_file()
            else {
                return;
            };
            let replacement = fs::canonicalize(&replacement).unwrap_or(replacement);
            let replacement_string = replacement.to_string_lossy().into_owned();
            let replacement_name = replacement
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Media")
                .to_owned();
            let mut clips: Vec<Clip> = ui.get_clips().iter().collect();
            let mut changed = false;
            for clip in &mut clips {
                if clip.path.as_str() == selected_path {
                    clip.path = replacement_string.clone().into();
                    clip.name = replacement_name.clone().into();
                    changed = true;
                }
            }
            if !changed {
                ui.set_status_message("The selected media item is no longer in the project".into());
                return;
            }
            ui.set_clips(Rc::new(slint::VecModel::from(clips)).into());
            ui.set_selected_clip(replacement_string.into());
            ui.set_status_message(format!("Relinked media to {replacement_name}").into());
        }
    });

    editor.on_filter_media({
        let weak = editor.as_weak();
        move |query| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let query = query.to_string().trim().to_lowercase();
            let filtered: Vec<Clip> = ui
                .get_clips()
                .iter()
                .filter(|clip| {
                    query.is_empty()
                        || clip.name.to_string().to_lowercase().contains(&query)
                        || clip.path.to_string().to_lowercase().contains(&query)
                })
                .collect();
            ui.set_filtered_media(Rc::new(slint::VecModel::from(filtered)).into());
        }
    });

    editor.on_select_clip({
        let weak = editor.as_weak();
        let player = Rc::clone(&player);
        let pending_seek = Rc::clone(&pending_timeline_seek);
        let pending_local = Rc::clone(&pending_local_seek);
        let file_ready = Rc::clone(&file_ready);

        move |path| {
            let Some(ui) = weak.upgrade() else {
                return;
            };

            let preview_path = preview_media_path(path.as_str());
            match player.load(preview_path.as_str()) {
                Ok(()) => {
                    ui.set_selected_clip(path);
                    if let Some(clip) = ui
                        .get_clips()
                        .iter()
                        .find(|clip| clip.path == ui.get_selected_clip())
                    {
                        ui.set_video_duration(
                            clip.source_out_seconds.max(clip.source_in_seconds + 0.05),
                        );
                        ui.set_video_duration_time(
                            format_time(clip.source_out_seconds as f64).into(),
                        );
                        ui.set_edit_trim_in(clip.source_in_seconds);
                        ui.set_edit_title_text(clip.title_text.to_string().into());
                        ui.set_edit_trim_out(clip.source_out_seconds);
                        ui.set_edit_track(clip.track);
                        ui.set_edit_exposure(clip.exposure);
                        ui.set_edit_contrast(clip.contrast);
                        ui.set_edit_saturation(clip.saturation);
                        ui.set_edit_opacity(clip.opacity);
                        ui.set_edit_audio_gain_db(clip.audio_gain_db);
                        ui.set_edit_blur(clip.blur);
                        ui.set_edit_sharpen(clip.sharpen);
                        ui.set_edit_transition_in(clip.transition_in_seconds);
                        ui.set_edit_transition_out(clip.transition_out_seconds);
                        ui.set_edit_scale(clip.scale);
                        ui.set_edit_rotation(clip.rotation);
                        ui.set_edit_animate_scale(clip.animate_scale);
                        ui.set_edit_scale_end(clip.scale_end);
                        apply_clip_preview_settings(&player, &clip, &audio_track_settings(&ui));
                        file_ready.set(false);
                        pending_local.set(Some(clip.source_in_seconds as f64));
                        pending_seek.set(Some(clip.start_seconds));
                    }
                    ui.set_is_playing(true);
                    ui.set_status_message("Loading video".into());
                }
                Err(error) => {
                    ui.set_status_message(format!("Could not load video: {error}").into());
                }
            }
        }
    });

    editor.on_generate_proxy({
        let weak = editor.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let selected = ui.get_selected_clip().to_string();
            if selected.is_empty() {
                ui.set_status_message("Select a clip before generating a proxy".into());
                return;
            }
            ui.set_status_message("Generating preview proxy…".into());
            let weak = weak.clone();
            let selected_for_reload = selected.clone();
            thread::spawn(move || {
                let result = proxy::create_proxy(&selected);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        match result {
                            Ok(proxy) => {
                                ui.invoke_select_clip(selected_for_reload.into());
                                ui.set_status_message(
                                    format!(
                                        "Proxy {} · {}",
                                        if proxy.reused { "reused" } else { "ready" },
                                        proxy
                                            .proxy_path
                                            .file_name()
                                            .and_then(|name| name.to_str())
                                            .unwrap_or("proxy.mp4")
                                    )
                                    .into(),
                                );
                            }
                            Err(error) => {
                                ui.set_status_message(format!("Proxy failed: {error}").into())
                            }
                        }
                    }
                });
            });
        }
    });

    editor.on_apply_clip_edits({
        let weak = editor.as_weak();
        let player = Rc::clone(&player);
        let undo = Rc::clone(&undo_stack);
        let redo = Rc::clone(&redo_stack);
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let selected = ui.get_selected_clip().to_string();
            if selected.is_empty() {
                return;
            }
            let mut clips: Vec<Clip> = ui.get_clips().iter().collect();
            let Some(index) = clips.iter().position(|clip| clip.path == selected) else {
                return;
            };
            let old = clips[index].clone();
            let source_duration = ui.get_video_duration().max(0.05);
            let source_in = ui
                .get_edit_trim_in()
                .clamp(0.0, (source_duration - 0.05).max(0.0));
            let source_out = ui
                .get_edit_trim_out()
                .clamp(source_in + 0.05, source_duration);
            let duration = (source_out - source_in).max(0.05);
            let clip = &mut clips[index];
            clip.title_text = ui.get_edit_title_text().to_string().into();
            clip.source_in_seconds = source_in;
            clip.source_out_seconds = source_out;
            clip.duration_seconds = duration;
            clip.duration = format_time(duration as f64).into();
            clip.track = ui.get_edit_track().clamp(0, 3);
            clip.exposure = ui.get_edit_exposure().clamp(-100.0, 100.0);
            clip.contrast = ui.get_edit_contrast().clamp(0.0, 2.0);
            clip.saturation = ui.get_edit_saturation().clamp(0.0, 2.0);
            clip.opacity = ui.get_edit_opacity().clamp(0.0, 1.0);
            clip.audio_gain_db = ui.get_edit_audio_gain_db().clamp(-24.0, 12.0);
            clip.blur = ui.get_edit_blur().clamp(0.0, 10.0);
            clip.sharpen = ui.get_edit_sharpen().clamp(0.0, 5.0);
            clip.transition_in_seconds = ui.get_edit_transition_in().clamp(0.0, duration / 2.0);
            clip.transition_out_seconds = ui.get_edit_transition_out().clamp(0.0, duration / 2.0);
            clip.scale = ui.get_edit_scale().clamp(0.25, 3.0);
            clip.rotation = ui.get_edit_rotation().clamp(-180.0, 180.0);
            clip.animate_scale = ui.get_edit_animate_scale();
            clip.scale_end = ui.get_edit_scale_end().clamp(0.25, 3.0);
            let _ = player
                .engine
                .set_property("brightness", clip.exposure as f64);
            let _ = player
                .engine
                .set_property("contrast", ((clip.contrast - 1.0) * 100.0) as f64);
            let _ = player
                .engine
                .set_property("saturation", ((clip.saturation - 1.0) * 100.0) as f64);
            let changed = old.source_in_seconds != clip.source_in_seconds
                || old.title_text != clip.title_text
                || old.source_out_seconds != clip.source_out_seconds
                || old.track != clip.track
                || old.exposure != clip.exposure
                || old.contrast != clip.contrast
                || old.saturation != clip.saturation
                || old.opacity != clip.opacity
                || old.audio_gain_db != clip.audio_gain_db
                || old.blur != clip.blur
                || old.sharpen != clip.sharpen
                || old.transition_in_seconds != clip.transition_in_seconds
                || old.transition_out_seconds != clip.transition_out_seconds;
            let changed = changed
                || old.scale != clip.scale
                || old.rotation != clip.rotation
                || old.animate_scale != clip.animate_scale
                || old.scale_end != clip.scale_end;
            if !changed {
                return;
            }
            apply_clip_preview_settings(&player, &clips[index], &audio_track_settings(&ui));
            undo.borrow_mut().push(ui.get_clips().iter().collect());
            if undo.borrow().len() > 100 {
                undo.borrow_mut().remove(0);
            }
            redo.borrow_mut().clear();
            let mut start = 0.0_f32;
            for clip in &clips {
                start = start.max(clip.start_seconds + clip.duration_seconds);
            }
            ui.set_timeline_duration(start.max(10.0));
            ui.set_clips(Rc::new(slint::VecModel::from(clips)).into());
            ui.set_status_message("Clip edit applied".into());
        }
    });

    editor.on_apply_track_audio_settings({
        let weak = editor.as_weak();
        let player = Rc::clone(&player);
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let tracks = audio_track_settings(&ui);
            if let Some(clip) = ui
                .get_clips()
                .iter()
                .find(|clip| clip.path == ui.get_selected_clip())
            {
                apply_clip_preview_settings(&player, &clip, &tracks);
            }
            ui.set_status_message("Track mixer updated".into());
        }
    });

    editor.on_undo_edit({
        let weak = editor.as_weak();
        let player = Rc::clone(&player);
        let undo = Rc::clone(&undo_stack);
        let redo = Rc::clone(&redo_stack);
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(previous) = undo.borrow_mut().pop() else {
                ui.set_status_message("Nothing to undo".into());
                return;
            };
            redo.borrow_mut().push(ui.get_clips().iter().collect());
            let end = previous
                .iter()
                .map(|clip| clip.start_seconds + clip.duration_seconds)
                .fold(10.0_f32, f32::max);
            ui.set_timeline_duration(end);
            ui.set_clips(Rc::new(slint::VecModel::from(previous)).into());
            if let Some(clip) = ui
                .get_clips()
                .iter()
                .find(|clip| clip.path == ui.get_selected_clip())
            {
                apply_clip_preview_settings(&player, &clip, &audio_track_settings(&ui));
                ui.set_edit_trim_in(clip.source_in_seconds);
                ui.set_edit_trim_out(clip.source_out_seconds);
                ui.set_edit_track(clip.track);
                ui.set_edit_exposure(clip.exposure);
                ui.set_edit_contrast(clip.contrast);
                ui.set_edit_saturation(clip.saturation);
                ui.set_edit_opacity(clip.opacity);
                ui.set_edit_audio_gain_db(clip.audio_gain_db);
                ui.set_edit_blur(clip.blur);
                ui.set_edit_sharpen(clip.sharpen);
                ui.set_edit_transition_in(clip.transition_in_seconds);
                ui.set_edit_transition_out(clip.transition_out_seconds);
            }
            ui.set_status_message("Undo".into());
        }
    });

    editor.on_redo_edit({
        let weak = editor.as_weak();
        let player = Rc::clone(&player);
        let undo = Rc::clone(&undo_stack);
        let redo = Rc::clone(&redo_stack);
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(next) = redo.borrow_mut().pop() else {
                ui.set_status_message("Nothing to redo".into());
                return;
            };
            undo.borrow_mut().push(ui.get_clips().iter().collect());
            let end = next
                .iter()
                .map(|clip| clip.start_seconds + clip.duration_seconds)
                .fold(10.0_f32, f32::max);
            ui.set_timeline_duration(end);
            ui.set_clips(Rc::new(slint::VecModel::from(next)).into());
            if let Some(clip) = ui
                .get_clips()
                .iter()
                .find(|clip| clip.path == ui.get_selected_clip())
            {
                apply_clip_preview_settings(&player, &clip, &audio_track_settings(&ui));
                ui.set_edit_trim_in(clip.source_in_seconds);
                ui.set_edit_trim_out(clip.source_out_seconds);
                ui.set_edit_track(clip.track);
                ui.set_edit_exposure(clip.exposure);
                ui.set_edit_contrast(clip.contrast);
                ui.set_edit_saturation(clip.saturation);
                ui.set_edit_opacity(clip.opacity);
                ui.set_edit_audio_gain_db(clip.audio_gain_db);
                ui.set_edit_blur(clip.blur);
                ui.set_edit_sharpen(clip.sharpen);
                ui.set_edit_transition_in(clip.transition_in_seconds);
                ui.set_edit_transition_out(clip.transition_out_seconds);
            }
            ui.set_status_message("Redo".into());
        }
    });

    editor.on_begin_move_clip({
        let weak = editor.as_weak();
        let snapshot = Rc::clone(&drag_snapshot);
        move || {
            if let Some(ui) = weak.upgrade() {
                *snapshot.borrow_mut() = Some(ui.get_clips().iter().collect());
            }
        }
    });

    editor.on_move_clip({
        let weak = editor.as_weak();
        let player = Rc::clone(&player);
        let pending_seek = Rc::clone(&pending_timeline_seek);
        let pending_local = Rc::clone(&pending_local_seek);
        let file_ready = Rc::clone(&file_ready);
        move |path, start_seconds, track| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut clips: Vec<Clip> = ui.get_clips().iter().collect();
            let Some(clip) = clips.iter_mut().find(|clip| clip.path == path) else {
                return;
            };
            clip.start_seconds = start_seconds.max(0.0);
            clip.track = track.clamp(0, 3);
            let moved = clip.clone();
            let end = clips
                .iter()
                .map(|clip| clip.start_seconds + clip.duration_seconds)
                .fold(10.0_f32, f32::max);
            ui.set_timeline_duration(end);
            ui.set_clips(Rc::new(slint::VecModel::from(clips)).into());
            ui.set_selected_clip(moved.path.clone());
            file_ready.set(false);
            pending_local.set(Some(moved.source_in_seconds as f64));
            pending_seek.set(Some(moved.start_seconds));
            if player.load(moved.path.as_str()).is_ok() {
                apply_clip_preview_settings(&player, &moved, &audio_track_settings(&ui));
                ui.set_is_playing(true);
                ui.set_status_message("Clip moved and loaded".into());
            }
        }
    });

    editor.on_finish_move_clip({
        let weak = editor.as_weak();
        let snapshot = Rc::clone(&drag_snapshot);
        let undo = Rc::clone(&undo_stack);
        let redo = Rc::clone(&redo_stack);
        move || {
            let Some(previous) = snapshot.borrow_mut().take() else {
                return;
            };
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let current: Vec<Clip> = ui.get_clips().iter().collect();
            let changed = previous.iter().zip(current.iter()).any(|(before, after)| {
                before.start_seconds != after.start_seconds || before.track != after.track
            });
            if changed {
                undo.borrow_mut().push(previous);
                if undo.borrow().len() > 100 {
                    undo.borrow_mut().remove(0);
                }
                redo.borrow_mut().clear();
                ui.set_status_message("Clip moved on timeline".into());
            }
        }
    });

    editor.on_toggle_playback({
        let weak = editor.as_weak();
        let player = Rc::clone(&player);
        let clock = Arc::clone(&timeline_clock);
        let audio = Arc::clone(&timeline_audio);
        let audio_generation = Arc::clone(&audio_generation);

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };

            match player.toggle_playback() {
                Ok(playing) => {
                    ui.set_is_playing(playing);
                    audio_generation.fetch_add(1, Ordering::AcqRel);
                    clear_timeline_audio(&audio);
                    if playing {
                        *clock
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            Some((ui.get_timeline_playhead_seconds(), Instant::now()));
                    } else {
                        clear_timeline_clock(&clock);
                    }
                    ui.set_status_message(if playing { "Playing" } else { "Paused" }.into());
                }
                Err(error) => {
                    ui.set_status_message(format!("Playback error: {error}").into());
                }
            }
        }
    });

    editor.on_seek_relative({
        let weak = editor.as_weak();
        let player = Rc::clone(&player);

        move |seconds| {
            if let Err(error) = player.seek(seconds as f64)
                && let Some(ui) = weak.upgrade()
            {
                ui.set_status_message(format!("Seek failed: {error}").into());
            }
        }
    });

    editor.on_seek_preview({
        let weak = editor.as_weak();
        move |fraction| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let selected = ui.get_selected_clip().to_string();
            let Some(clip) = ui
                .get_clips()
                .iter()
                .find(|clip| clip.path == selected && clip.track <= 1)
            else {
                ui.set_status_message("Select a video clip before seeking".into());
                return;
            };
            let target = clip.start_seconds + clip.duration_seconds * fraction.clamp(0.0, 1.0);
            ui.invoke_seek_to(target);
        }
    });

    editor.on_seek_to({
        let weak = editor.as_weak();
        let player = Rc::clone(&player);
        let pending_seek = Rc::clone(&pending_timeline_seek);
        let pending_local = Rc::clone(&pending_local_seek);
        let file_ready = Rc::clone(&file_ready);
        let clock = Arc::clone(&timeline_clock);
        let audio = Arc::clone(&timeline_audio);
        let audio_generation = Arc::clone(&audio_generation);

        move |seconds| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let clips: Vec<Clip> = ui.get_clips().iter().collect();
            let target = seconds.max(0.0);
            audio_generation.fetch_add(1, Ordering::AcqRel);
            clear_timeline_audio(&audio);
            if ui.get_is_playing() {
                *clock
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some((target, Instant::now()));
            } else {
                clear_timeline_clock(&clock);
            }
            let clip = clips
                .iter()
                .filter(|clip| {
                    clip.track <= 1
                        && target >= clip.start_seconds
                        && target < clip.start_seconds + clip.duration_seconds
                })
                .min_by_key(|clip| clip.track)
                .cloned();

            let Some(clip) = clip else {
                // A timeline gap is not part of the previous clip. Keep the
                // playhead at the requested time instead of seeking beyond a
                // stale source and showing an unrelated frame.
                pending_seek.set(None);
                pending_local.set(None);
                let _ = player.engine.set_paused(true);
                ui.set_is_playing(false);
                ui.set_selected_clip("".into());
                ui.set_video_frame(Image::default());
                ui.set_has_video_frame(false);
                ui.set_timeline_playhead_seconds(target);
                ui.set_playhead_time(format_time(target as f64).into());
                ui.set_status_message("Timeline gap".into());
                return;
            };
            let local_time = (clip.source_in_seconds + target - clip.start_seconds)
                .max(0.0)
                .min((clip.source_out_seconds - 0.01).max(0.0)) as f64;
            let needs_load = ui.get_selected_clip().as_str() != clip.path.as_str();
            let still_loading_selected = !needs_load && !file_ready.get();
            let result = if needs_load {
                file_ready.set(false);
                let preview_path = preview_media_path(clip.path.as_str());
                player.load(preview_path.as_str())
            } else if still_loading_selected {
                // Selecting a clip starts an asynchronous load. A click/seek
                // in the timeline can arrive before mpv's Loaded event; defer
                // the seek so it cannot target the previously loaded file.
                pending_local.set(Some(local_time));
                pending_seek.set(Some(target));
                ui.set_status_message("Waiting for selected clip to load…".into());
                return;
            } else {
                player.seek_absolute(local_time)
            };
            match result {
                Ok(()) => {
                    ui.set_selected_clip(clip.path.clone());
                    ui.set_playhead_seconds(local_time as f32);
                    ui.set_timeline_playhead_seconds(target);
                    pending_seek.set(Some(target));
                    if needs_load {
                        // mpv accepts loadfile asynchronously; seeking immediately
                        // can target the old file. Defer until the new file is loaded.
                        pending_local.set(Some(local_time));
                        ui.set_status_message("Loading clip and seeking…".into());
                    } else {
                        pending_local.set(None);
                        ui.set_status_message(
                            format!("Seeked to {}", format_time(local_time)).into(),
                        );
                    }
                }
                Err(error) => {
                    pending_seek.set(Some(target));
                    pending_local.set(Some(local_time));
                    ui.set_status_message(format!("Waiting to seek ({error})…").into());
                }
            }
        }
    });

    editor.on_export_project({
        let weak = editor.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_export_in_progress() {
                return;
            }
            let Some(output_path) = rfd::FileDialog::new()
                .set_title("Export VideoPro Timeline")
                .add_filter("MP4 Video", &["mp4"])
                .set_file_name("VideoPro-Export.mp4")
                .save_file()
            else {
                return;
            };
            let clips = render_clip_snapshot(&ui);
            let export_width = ui.get_export_width().clamp(1280, 3840) as u32;
            let export_height = ui.get_export_height().clamp(720, 2160) as u32;
            let export_quality = ui.get_export_quality().to_string();
            let export_options = render::ExportRenderOptions {
                frame_rate: ui.get_export_frame_rate().clamp(24, 60) as u32,
                video_codec: match ui.get_export_codec().as_str() {
                    "H.265" => render::ExportVideoCodec::H265,
                    _ => render::ExportVideoCodec::H264,
                },
                bitrate_kbps: match ui.get_export_bitrate().as_str() {
                    "8 Mbps" => Some(8_000),
                    "16 Mbps" => Some(16_000),
                    "32 Mbps" => Some(32_000),
                    _ => None,
                },
            };
            ui.set_export_progress(0.0);
            ui.set_export_in_progress(true);
            ui.set_status_message("Rendering timeline to MP4…".into());
            let weak = ui.as_weak();
            drop(ui);
            thread::spawn(move || {
                let progress_weak = weak.clone();
                let result = render::render_mp4_with_export_options(
                    &clips,
                    &output_path,
                    export_width,
                    export_height,
                    &export_quality,
                    export_options,
                    move |progress| {
                        let weak = progress_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_export_progress(progress);
                                ui.set_status_message(
                                    format!("Rendering timeline · {:.0}%", progress * 100.0).into(),
                                );
                            }
                        });
                    },
                );
                let (message, succeeded) = match result {
                    Ok(()) => (
                        format!(
                            "Export complete: {}",
                            output_path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("output.mp4")
                        ),
                        true,
                    ),
                    Err(error) => (format!("Export failed: {error}"), false),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        if succeeded {
                            ui.set_export_progress(1.0);
                        }
                        ui.set_export_in_progress(false);
                        ui.set_status_message(message.into());
                    }
                });
            });
        }
    });

    editor.on_save_layout({
        let weak = editor.as_weak();
        move |name| {
            if let Some(ui) = weak.upgrade() {
                let name = name.trim().to_string();

                if !name.is_empty() {
                    let mut layouts: Vec<WorkspaceLayout> =
                        ui.get_custom_layouts().iter().collect();
                    let snapshot = WorkspaceLayout {
                        name: name.clone().into(),
                        show_project_panel: ui.get_show_project_panel(),
                        show_properties_panel: ui.get_show_properties_panel(),
                        show_effects_browser: ui.get_show_effects_browser(),
                        show_timeline: ui.get_show_timeline(),
                        timeline_height: ui.get_timeline_height(),
                        left_panel_width: ui.get_left_panel_width(),
                        right_panel_width: ui.get_right_panel_width(),
                        timeline_zoom: ui.get_timeline_zoom(),
                        active_mode: ui.get_active_mode().to_string().into(),
                        edit_exposure: ui.get_edit_exposure(),
                        edit_contrast: ui.get_edit_contrast(),
                        edit_saturation: ui.get_edit_saturation(),
                        edit_opacity: ui.get_edit_opacity(),
                        edit_transition_in: ui.get_edit_transition_in(),
                        edit_transition_out: ui.get_edit_transition_out(),
                    };
                    if let Some(existing) = layouts
                        .iter_mut()
                        .find(|saved| saved.name.eq_ignore_ascii_case(&name))
                    {
                        *existing = snapshot;
                    } else {
                        if layouts.len() >= MAX_WORKSPACE_LAYOUTS {
                            ui.set_status_message(
                                format!(
                                    "Workspace limit reached ({MAX_WORKSPACE_LAYOUTS}); replace an existing layout"
                                )
                                .into(),
                            );
                            return;
                        }
                        layouts.push(snapshot);
                    }
                    ui.set_custom_layouts(Rc::new(slint::VecModel::from(layouts)).into());
                    let layouts: Vec<WorkspaceLayout> = ui.get_custom_layouts().iter().collect();
                    match save_workspace_layouts(&layouts) {
                        Ok(()) => ui.set_status_message(format!("Layout saved: {name}").into()),
                        Err(error) => ui.set_status_message(
                            format!("Layout kept for this session; disk save failed: {error}")
                                .into(),
                        ),
                    }
                }
            }
        }
    });

    editor.on_load_layout({
        let weak = editor.as_weak();
        move |name| {
            if let Some(ui) = weak.upgrade() {
                let layouts: Vec<WorkspaceLayout> = ui.get_custom_layouts().iter().collect();
                if let Some(saved) = layouts.iter().find(|saved| saved.name == name) {
                    ui.set_layout_name(saved.name.clone());
                    ui.set_show_project_panel(saved.show_project_panel);
                    ui.set_show_properties_panel(saved.show_properties_panel);
                    ui.set_show_effects_browser(saved.show_effects_browser);
                    ui.set_show_timeline(saved.show_timeline);
                    ui.set_timeline_height(saved.timeline_height);
                    ui.set_left_panel_width(saved.left_panel_width);
                    ui.set_right_panel_width(saved.right_panel_width);
                    ui.set_timeline_zoom(saved.timeline_zoom);
                    ui.set_active_mode(saved.active_mode.clone());
                    ui.set_edit_exposure(saved.edit_exposure);
                    ui.set_edit_contrast(saved.edit_contrast);
                    ui.set_edit_saturation(saved.edit_saturation);
                    ui.set_edit_opacity(saved.edit_opacity);
                    ui.set_edit_transition_in(saved.edit_transition_in);
                    ui.set_edit_transition_out(saved.edit_transition_out);
                    ui.set_status_message(format!("Workspace loaded: {name}").into());
                }
            }
        }
    });

    editor.on_reset_layout({
        let weak = editor.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_active_mode("Editing".into());
                ui.set_timeline_zoom(1.0);
                ui.set_layout_name("My Layout".into());
                ui.set_show_project_panel(true);
                ui.set_show_properties_panel(true);
                ui.set_show_effects_browser(false);
                ui.set_show_timeline(true);
                ui.set_timeline_height(276.0);
                ui.set_left_panel_width(240.0);
                ui.set_right_panel_width(272.0);
                ui.set_status_message("Layout reset".into());
            }
        }
    });

    editor.on_remove_selected_clip({
        let weak = editor.as_weak();
        let player = Rc::clone(&player);
        let undo = Rc::clone(&undo_stack);
        let redo = Rc::clone(&redo_stack);

        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };

            let selected = ui.get_selected_clip().to_string();

            if selected.is_empty() {
                ui.set_status_message("No clip selected".into());
                return;
            }

            let mut clips: Vec<Clip> = ui.get_clips().iter().collect();
            undo.borrow_mut().push(clips.clone());
            if undo.borrow().len() > 100 {
                undo.borrow_mut().remove(0);
            }
            redo.borrow_mut().clear();
            clips.retain(|clip| clip.path != selected);
            let start = clips
                .iter()
                .map(|clip| clip.start_seconds + clip.duration_seconds)
                .fold(0.0_f32, f32::max);

            ui.set_clips(Rc::new(slint::VecModel::from(clips)).into());
            ui.set_timeline_duration(if start > 0.0 { start } else { 10.0 });
            ui.set_selected_clip("".into());
            ui.set_video_frame(Image::default());
            ui.set_has_video_frame(false);
            ui.set_playhead_seconds(0.0);
            ui.set_playhead_time("00:00:00.000".into());
            ui.set_timeline_playhead_seconds(0.0);
            ui.set_video_duration(0.0);
            ui.set_video_duration_time("--:--".into());
            ui.set_is_playing(false);
            ui.set_status_message("Clip removed".into());

            let _ = player.engine.stop();
        }
    });

    editor.on_close_window({
        let weak = editor.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                let _ = ui.hide();
            }
        }
    });

    editor.on_minimize_window({
        let weak = editor.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().set_minimized(true);
            }
        }
    });

    editor.on_toggle_maximize({
        let weak = editor.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                let window = ui.window();
                window.set_maximized(!window.is_maximized());
            }
        }
    });
    EditorRuntimeState {
        reset_requested: project_reset_requested,
        undo_stack,
        redo_stack,
        drag_snapshot,
    }
}

fn pick_media_files() -> Option<Vec<std::path::PathBuf>> {
    rfd::FileDialog::new()
        .add_filter(
            "Video and audio",
            &[
                "mp4", "mov", "m4v", "mkv", "webm", "avi", "mpeg", "mpg", "mts", "m2ts", "ts",
                "flv", "ogv", "3gp", "mp3", "wav", "m4a", "aac", "flac", "ogg", "opus", "aiff",
                "wma",
            ],
        )
        .add_filter("All files", &["*"])
        .pick_files()
}

fn media_path_identity(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn open_editor(
    welcome_weak: slint::Weak<WelcomeWindow>,
    project_name: &'static str,
    open_existing: bool,
) {
    let editor = match EditorWindow::new() {
        Ok(editor) => editor,
        Err(error) => {
            eprintln!("Could not create editor: {error}");
            return;
        }
    };

    let player = match VideoPlayer::new() {
        Ok(player) => Rc::new(player),
        Err(error) => {
            editor.set_status_message(format!("Player initialization failed: {error}").into());
            eprintln!("Could not initialize player: {error}");
            return;
        }
    };

    editor.set_project_name(project_name.into());
    let missing = missing_runtime_dependencies();
    editor.set_status_message(if missing.is_empty() {
        "Ready".into()
    } else {
        format!(
            "Missing runtime dependencies: {}. Install them and restart VideoPro.",
            missing.join(", ")
        )
        .into()
    });
    editor.set_custom_layouts(Rc::new(slint::VecModel::from(load_workspace_layouts())).into());

    let pending_timeline_seek = Rc::new(Cell::new(None::<f32>));
    let pending_local_seek = Rc::new(Cell::new(None::<f64>));
    let file_ready = Rc::new(Cell::new(false));
    let timeline_clock = Arc::new(Mutex::new(None::<(f32, Instant)>));
    let timeline_audio = Arc::new(Mutex::new(None::<render::TimelineAudioPlayback>));
    let audio_generation = Arc::new(AtomicU64::new(0));
    let timeline_playback = TimelinePlaybackState {
        clock: Arc::clone(&timeline_clock),
        audio: Arc::clone(&timeline_audio),
        generation: Arc::clone(&audio_generation),
    };
    let runtime = configure_editor(
        &editor,
        Rc::clone(&player),
        Rc::clone(&pending_timeline_seek),
        Rc::clone(&pending_local_seek),
        Rc::clone(&file_ready),
        timeline_playback,
    );
    if open_existing {
        editor.invoke_open_project();
    }

    let gpu_preview: Rc<RefCell<Option<GpuPreview>>> = Rc::new(RefCell::new(None));
    let preview_slot = Rc::clone(&gpu_preview);
    if let Err(error) = editor
        .window()
        .set_rendering_notifier(move |state, graphics_api| match (state, graphics_api) {
            (
                slint::RenderingState::RenderingSetup,
                slint::GraphicsAPI::WGPU30 { device, queue, .. },
            ) => {
                *preview_slot.borrow_mut() = Some(GpuPreview::new(device.clone(), queue.clone()));
            }
            (slint::RenderingState::RenderingTeardown, _) => {
                *preview_slot.borrow_mut() = None;
            }
            _ => {}
        })
    {
        editor.set_status_message(format!("GPU preview setup failed: {error}").into());
        return;
    }

    let weak = editor.as_weak();
    let frame_player = Rc::clone(&player);
    let frame_preview = Rc::clone(&gpu_preview);
    let pending_seek = Rc::clone(&pending_timeline_seek);
    let pending_local = Rc::clone(&pending_local_seek);
    let file_ready_for_timer = Rc::clone(&file_ready);
    let timeline_clock_for_timer = Arc::clone(&timeline_clock);
    let timeline_audio_for_timer = Arc::clone(&timeline_audio);
    let audio_generation_for_timer = Arc::clone(&audio_generation);
    let audio_start_busy = Arc::new(AtomicBool::new(false));
    let audio_start_busy_for_timer = Arc::clone(&audio_start_busy);
    let audio_attempted_generation = Arc::new(AtomicU64::new(u64::MAX));
    let audio_attempted_generation_for_timer = Arc::clone(&audio_attempted_generation);
    let reset_requested = Arc::clone(&runtime.reset_requested);
    let undo_for_timer = Rc::clone(&runtime.undo_stack);
    let redo_for_timer = Rc::clone(&runtime.redo_stack);
    let drag_for_timer = Rc::clone(&runtime.drag_snapshot);
    let preview_busy = Arc::new(AtomicBool::new(false));
    let preview_busy_for_timer = Arc::clone(&preview_busy);
    let latest_preview_id = Arc::new(AtomicU64::new(0));
    let latest_preview_id_for_timer = Arc::clone(&latest_preview_id);
    let meter_busy = Arc::new(AtomicBool::new(false));
    let meter_busy_for_timer = Arc::clone(&meter_busy);

    let timer = Timer::default();
    let mut last_duration: Option<f64> = None;
    let mut last_position: Option<f64> = None;
    let mut last_playhead_text = String::new();
    let mut last_selected_path = String::new();
    let mut selected_start_cache = 0.0_f32;
    let mut selected_source_in_cache = 0.0_f32;
    let mut last_preview_time: Option<f32> = None;
    let mut last_preview_signature: Option<u64> = None;
    let mut last_audio_signature: Option<u64> = None;
    let mut last_meter_sample: Option<Instant> = None;
    timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };

        if reset_requested.swap(false, Ordering::AcqRel) {
            let _ = frame_player.engine.stop();
            audio_generation_for_timer.fetch_add(1, Ordering::AcqRel);
            clear_timeline_clock(&timeline_clock_for_timer);
            clear_timeline_audio(&timeline_audio_for_timer);
            last_audio_signature = None;
            pending_seek.set(None);
            pending_local.set(None);
            file_ready_for_timer.set(false);
            drag_for_timer.borrow_mut().take();
            undo_for_timer.borrow_mut().clear();
            redo_for_timer.borrow_mut().clear();
            ui.set_is_playing(false);
            ui.set_selected_clip("".into());
            ui.set_video_frame(Image::default());
            ui.set_has_video_frame(false);
            ui.set_playhead_seconds(0.0);
            ui.set_playhead_time("00:00:00.000".into());
            ui.set_timeline_playhead_seconds(0.0);
            ui.set_video_duration(0.0);
            ui.set_video_duration_time("--:--".into());
            last_duration = None;
            last_position = None;
            last_playhead_text.clear();
            last_selected_path.clear();
            selected_start_cache = 0.0;
            selected_source_in_cache = 0.0;
        }

        // mpv event pumping and property queries are unnecessary while the
        // editor is idle. Keep the lightweight timer alive so loading and
        // deferred seeks resume as soon as a clip is selected.
        if ui.get_selected_clip().is_empty()
            && pending_seek.get().is_none()
            && pending_local.get().is_none()
        {
            return;
        }

        if ui.get_is_playing() {
            if timeline_clock_for_timer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none()
            {
                *timeline_clock_for_timer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((
                    pending_seek
                        .get()
                        .unwrap_or_else(|| ui.get_timeline_playhead_seconds()),
                    Instant::now(),
                ));
                audio_generation_for_timer.fetch_add(1, Ordering::AcqRel);
            }
        } else {
            clear_timeline_clock(&timeline_clock_for_timer);
            clear_timeline_audio(&timeline_audio_for_timer);
            last_audio_signature = None;
        }

        // Drain mpv's pending events before polling time-pos. In particular,
        // load/seek completion can make the position temporarily unavailable;
        // preserve the last known position instead of snapping the playhead
        // back to zero for that tick.
        let events = frame_player.engine.pump_events();
        if events
            .iter()
            .any(|event| matches!(event, mpv_engine::PlaybackEvent::Loaded))
        {
            file_ready_for_timer.set(true);
        }
        if file_ready_for_timer.get()
            && let Some(local_time) = pending_local.get()
        {
            match frame_player.seek_absolute(local_time) {
                Ok(()) => {
                    pending_local.set(None);
                    ui.set_status_message(format!("Seeked to {}", format_time(local_time)).into());
                }
                Err(_) => {
                    // Retry next tick while mpv finishes entering playback.
                }
            }
        }
        let position = frame_player
            .engine
            .position()
            .unwrap_or(ui.get_playhead_seconds() as f64)
            .max(0.0);
        if last_position.is_none_or(|last| (last - position).abs() >= 0.001) {
            ui.set_playhead_seconds(position as f32);
            last_position = Some(position);
        }
        let playhead_text = format_time(position);
        if playhead_text != last_playhead_text {
            ui.set_playhead_time(playhead_text.clone().into());
            last_playhead_text = playhead_text;
        }

        let selected_path = ui.get_selected_clip().to_string();
        if selected_path != last_selected_path {
            last_duration = None;
            selected_start_cache = ui
                .get_clips()
                .iter()
                .find(|clip| clip.path == selected_path)
                .map(|clip| clip.start_seconds)
                .unwrap_or(0.0);
            selected_source_in_cache = ui
                .get_clips()
                .iter()
                .find(|clip| clip.path == selected_path)
                .map(|clip| clip.source_in_seconds)
                .unwrap_or(0.0);
            last_selected_path = selected_path;
        }
        if let Some(selected) = ui
            .get_clips()
            .iter()
            .find(|clip| clip.path == last_selected_path)
        {
            selected_start_cache = selected.start_seconds;
            selected_source_in_cache = selected.source_in_seconds;
        }

        if let Some(duration) = frame_player
            .engine
            .duration()
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            && last_duration.is_none_or(|last| (last - duration).abs() > 0.05)
        {
            ui.set_video_duration(duration as f32);
            ui.set_video_duration_time(format_time(duration).into());
            let selected = ui.get_selected_clip().to_string();
            let selected_clip = ui.get_clips().iter().find(|clip| clip.path == selected);
            if let Some(clip) = selected_clip {
                ui.set_edit_trim_out(clip.source_out_seconds.min(duration as f32));
            }
            last_duration = Some(duration);
        }
        let actual_timeline_position = if ui.get_is_playing() {
            timeline_clock_for_timer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .map(|(start, instant)| *start + instant.elapsed().as_secs_f32())
                .unwrap_or_else(|| {
                    selected_start_cache + (position as f32 - selected_source_in_cache)
                })
        } else {
            ui.get_timeline_playhead_seconds()
        };
        if ui.get_is_playing() {
            let clips: Vec<Clip> = ui.get_clips().iter().collect();
            let active_video = clips
                .iter()
                .filter(|clip| {
                    clip.track <= 1
                        && actual_timeline_position >= clip.start_seconds
                        && actual_timeline_position < clip.start_seconds + clip.duration_seconds
                })
                .min_by_key(|clip| clip.track)
                .cloned();
            if let Some(next_clip) = active_video
                && next_clip.path.as_str() != ui.get_selected_clip().as_str()
            {
                let local_time = next_clip.source_in_seconds
                    + (actual_timeline_position - next_clip.start_seconds).max(0.0);
                file_ready_for_timer.set(false);
                pending_local.set(Some(
                    local_time.min((next_clip.source_out_seconds - 0.01).max(0.0)) as f64,
                ));
                pending_seek.set(Some(actual_timeline_position));
                if frame_player.load(next_clip.path.as_str()).is_ok() {
                    ui.set_selected_clip(next_clip.path.clone());
                    apply_clip_preview_settings(
                        &frame_player,
                        &next_clip,
                        &audio_track_settings(&ui),
                    );
                    ui.set_status_message("Switching active video track".into());
                } else {
                    pending_local.set(None);
                    pending_seek.set(None);
                }
                return;
            }
        }
        let timeline_position = match pending_seek.get() {
            Some(target) if (actual_timeline_position - target).abs() > 0.20 => target,
            Some(_) => {
                pending_seek.set(None);
                actual_timeline_position
            }
            None => actual_timeline_position,
        };
        if (ui.get_timeline_playhead_seconds() - timeline_position).abs() >= 0.01 {
            ui.set_timeline_playhead_seconds(timeline_position);
        }

        // The mpv instance still owns playback audio and its frame pool, but
        // the visible frame comes from the same FFmpeg layer compositor as
        // export. Drain unused mpv frames so the bounded GPU pool keeps moving.
        match frame_player.take_frame() {
            Ok(Some(frame)) if !ui.get_has_video_frame() => {
                if let Some(preview) = frame_preview.borrow().as_ref()
                    && let Ok(image) = preview.import_frame(frame)
                {
                    ui.set_video_frame(image);
                    ui.set_has_video_frame(true);
                }
            }
            Ok(_) => {}
            Err(error) => {
                ui.set_status_message(format!("Playback frame acquisition failed: {error}").into());
            }
        }

        let snapshot = render_clip_snapshot(&ui);
        let preview_snapshot = snapshot.clone();
        let meter_snapshot = snapshot.clone();
        let signature = render_snapshot_signature(&snapshot);

        if ui.get_is_playing() && last_audio_signature != Some(signature) {
            if last_audio_signature.is_some() {
                // Any timeline or mixer edit invalidates the old stream. Restart
                // at the current clock position rather than jumping to clip zero.
                clear_timeline_audio(&timeline_audio_for_timer);
                audio_generation_for_timer.fetch_add(1, Ordering::AcqRel);
                *timeline_clock_for_timer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some((actual_timeline_position, Instant::now()));
            }
            last_audio_signature = Some(signature);
        }

        if ui.get_is_playing() && !snapshot.is_empty() {
            let generation = audio_generation_for_timer.load(Ordering::Acquire);
            if audio_attempted_generation_for_timer.load(Ordering::Acquire) != generation
                && !audio_start_busy_for_timer.swap(true, Ordering::AcqRel)
            {
                audio_attempted_generation_for_timer.store(generation, Ordering::Release);
                let audio_snapshot = snapshot.clone();
                let audio_position = actual_timeline_position;
                let weak = ui.as_weak();
                let audio_slot = Arc::clone(&timeline_audio_for_timer);
                let clock_slot = Arc::clone(&timeline_clock_for_timer);
                let generation_state = Arc::clone(&audio_generation_for_timer);
                let busy = Arc::clone(&audio_start_busy_for_timer);
                thread::spawn(move || {
                    let result =
                        render::TimelineAudioPlayback::start(&audio_snapshot, audio_position);
                    let clock_anchor = result
                        .as_ref()
                        .ok()
                        .and_then(Option::as_ref)
                        .map(render::TimelineAudioPlayback::clock_anchor);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade()
                            && ui.get_is_playing()
                            && generation_state.load(Ordering::Acquire) == generation
                        {
                            match result {
                                Ok(playback) => {
                                    if let Some(anchor) = clock_anchor {
                                        *clock_slot
                                            .lock()
                                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                            Some(anchor);
                                    }
                                    *audio_slot
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                        playback;
                                }
                                Err(error) => ui.set_status_message(
                                    format!("Live timeline audio unavailable: {error}").into(),
                                ),
                            }
                        }
                        busy.store(false, Ordering::Release);
                    });
                });
            }
            let audio_ended = timeline_audio_for_timer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_mut()
                .is_some_and(|playback| playback.position().is_none());
            if audio_ended {
                clear_timeline_audio(&timeline_audio_for_timer);
            }
        }

        if ui.get_is_playing() && actual_timeline_position >= ui.get_timeline_duration() {
            ui.set_timeline_playhead_seconds(ui.get_timeline_duration());
            ui.set_is_playing(false);
            let _ = frame_player.engine.set_paused(true);
            clear_timeline_clock(&timeline_clock_for_timer);
            clear_timeline_audio(&timeline_audio_for_timer);
            audio_generation_for_timer.fetch_add(1, Ordering::AcqRel);
            last_audio_signature = None;
            return;
        }
        let time_changed = last_preview_time.is_none_or(|last| {
            (last - timeline_position).abs() >= if ui.get_is_playing() { 0.12 } else { 0.02 }
        });
        let edits_changed = last_preview_signature != Some(signature);
        if (time_changed || edits_changed) && !preview_busy_for_timer.swap(true, Ordering::AcqRel) {
            let request_id = NEXT_PREVIEW_FRAME_ID.fetch_add(1, Ordering::Relaxed);
            latest_preview_id_for_timer.store(request_id, Ordering::Release);
            last_preview_time = Some(timeline_position);
            last_preview_signature = Some(signature);
            let output = std::env::temp_dir().join(format!(
                "videopro-preview-{}-{request_id}.png",
                std::process::id()
            ));
            let weak = ui.as_weak();
            let busy = Arc::clone(&preview_busy_for_timer);
            let latest = Arc::clone(&latest_preview_id_for_timer);
            thread::spawn(move || {
                let result =
                    render::render_preview_frame(&preview_snapshot, timeline_position, &output);
                let _ = slint::invoke_from_event_loop(move || {
                    if latest.load(Ordering::Acquire) == request_id
                        && let Some(ui) = weak.upgrade()
                    {
                        match result {
                            Ok(()) => match Image::load_from_path(&output) {
                                Ok(image) => {
                                    ui.set_video_frame(image);
                                    ui.set_has_video_frame(true);
                                }
                                Err(error) => ui.set_status_message(
                                    format!("Could not load composited preview frame: {error}")
                                        .into(),
                                ),
                            },
                            Err(error) => ui.set_status_message(
                                format!("Composited preview failed: {error}").into(),
                            ),
                        }
                    }
                    let _ = fs::remove_file(output);
                    busy.store(false, Ordering::Release);
                });
            });
        }

        if ui.get_is_playing()
            && last_meter_sample.is_none_or(|last| last.elapsed() >= Duration::from_millis(250))
            && !meter_busy_for_timer.swap(true, Ordering::AcqRel)
        {
            last_meter_sample = Some(Instant::now());
            let weak = ui.as_weak();
            let busy = Arc::clone(&meter_busy_for_timer);
            thread::spawn(move || {
                let result = render::measure_track_levels(&meter_snapshot, timeline_position);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade()
                        && let Ok(levels) = result
                    {
                        ui.set_track_v2_meter_rms(levels.tracks[0].rms);
                        ui.set_track_v2_meter_peak(levels.tracks[0].peak);
                        ui.set_track_v1_meter_rms(levels.tracks[1].rms);
                        ui.set_track_v1_meter_peak(levels.tracks[1].peak);
                        ui.set_track_a1_meter_rms(levels.tracks[2].rms);
                        ui.set_track_a1_meter_peak(levels.tracks[2].peak);
                        ui.set_track_a2_meter_rms(levels.tracks[3].rms);
                        ui.set_track_a2_meter_peak(levels.tracks[3].peak);
                        ui.set_master_meter_rms(levels.master.rms);
                        ui.set_master_meter_peak(levels.master.peak);
                    }
                    busy.store(false, Ordering::Release);
                });
            });
        } else if !ui.get_is_playing() {
            last_meter_sample = None;
            ui.set_track_v2_meter_rms(0.0);
            ui.set_track_v2_meter_peak(0.0);
            ui.set_track_v1_meter_rms(0.0);
            ui.set_track_v1_meter_peak(0.0);
            ui.set_track_a1_meter_rms(0.0);
            ui.set_track_a1_meter_peak(0.0);
            ui.set_track_a2_meter_rms(0.0);
            ui.set_track_a2_meter_peak(0.0);
            ui.set_master_meter_rms(0.0);
            ui.set_master_meter_peak(0.0);
        }
    });

    match editor.show() {
        Ok(()) => {
            if let Some(welcome) = welcome_weak.upgrade() {
                let _ = welcome.hide();
            }
        }
        Err(error) => eprintln!("Could not show editor: {error}"),
    }

    // Keep the timer alive while the editor is open.
    std::mem::forget(timer);
}

fn format_time(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    let hours = (seconds / 3600.0).floor() as u64;
    let minutes = ((seconds % 3600.0) / 60.0).floor() as u64;
    let whole_seconds = (seconds % 60.0).floor() as u64;
    let millis = ((seconds.fract() * 1000.0).round() as u64).min(999);
    format!("{hours:02}:{minutes:02}:{whole_seconds:02}.{millis:03}")
}

fn probe_duration(path: &Path) -> Option<f64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let duration = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .ok()?;
    (duration.is_finite() && duration > 0.0).then_some(duration)
}

fn missing_runtime_dependencies() -> Vec<&'static str> {
    ["ffmpeg", "ffprobe", "ffplay"]
        .into_iter()
        .filter(|program| {
            !Command::new(program)
                .arg("-version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
        .collect()
}

fn make_thumbnail(path: &Path, seconds: f64, index: usize) -> Option<std::path::PathBuf> {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let directory = std::env::temp_dir().join("videopro-thumbnails");
    if std::fs::create_dir_all(&directory).is_err() {
        return None;
    }
    let output_path = directory.join(format!("{:x}-{index}.jpg", hasher.finish()));
    if output_path.is_file() {
        return Some(output_path);
    }
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-ss"])
        .arg(format!("{seconds:.3}"))
        .arg("-i")
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            "scale=160:90:force_original_aspect_ratio=increase,crop=160:90",
        ])
        .arg(&output_path)
        .output();

    if output.is_ok_and(|result| result.status.success()) {
        Some(output_path)
    } else {
        None
    }
}

fn make_waveform(path: &Path) -> Option<PathBuf> {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let directory = std::env::temp_dir().join("videopro-waveforms");
    if fs::create_dir_all(&directory).is_err() {
        return None;
    }
    let output_path = directory.join(format!("{:x}.png", hasher.finish()));
    if output_path.is_file() {
        return Some(output_path);
    }
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(path)
        .args([
            "-filter_complex",
            "[0:a:0]showwavespic=s=640x80:colors=0x90c8ff,format=rgba,colorkey=black:0.03:0.0[w]",
            "-map",
            "[w]",
            "-frames:v",
            "1",
        ])
        .arg(&output_path)
        .output();
    if output.is_ok_and(|result| result.status.success()) {
        Some(output_path)
    } else {
        None
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut wgpu_settings = wgpu_30::WGPUSettings::default();
    wgpu_settings.backends = mpv_engine::WGPU_BACKEND;
    wgpu_settings.device_required_features |= mpv_engine::REQUIRED_WGPU_FEATURES;
    let backend = slint::BackendSelector::new()
        .require_wgpu_30(wgpu_30::WGPUConfiguration::Automatic(wgpu_settings));

    // Keep AppKit's native traffic lights while letting the custom Slint title
    // bar render underneath the transparent macOS titlebar.
    #[cfg(target_os = "macos")]
    let backend = backend.with_winit_window_attributes_hook(|attributes| {
        use slint::winit_030::winit::platform::macos::WindowAttributesExtMacOS;

        attributes
            .with_fullsize_content_view(true)
            .with_titlebar_transparent(true)
            .with_title_hidden(true)
    });

    backend.select()?;

    let welcome = WelcomeWindow::new()?;
    welcome.set_is_macos(cfg!(target_os = "macos"));
    welcome.set_recent_projects(Rc::new(slint::VecModel::from(load_recent_projects())).into());

    let welcome_weak = welcome.as_weak();

    welcome.on_create_project({
        let weak = welcome_weak.clone();
        move || open_editor(weak.clone(), "Untitled Project", false)
    });

    welcome.on_open_project({
        let weak = welcome_weak.clone();
        move || open_editor(weak.clone(), "Untitled Project", true)
    });

    welcome.on_open_recent_project({
        let weak = welcome_weak.clone();
        move |path: slint::SharedString| {
            if let Ok(mut pending) = PENDING_PROJECT_PATH.lock() {
                *pending = Some(PathBuf::from(path.as_str()));
            }
            open_editor(weak.clone(), "Untitled Project", true);
        }
    });

    welcome.on_close_window({
        let weak = welcome_weak.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                let _ = ui.hide();
            }
        }
    });
    welcome.on_minimize_window({
        let weak = welcome_weak.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().set_minimized(true);
            }
        }
    });
    welcome.on_toggle_maximize({
        let weak = welcome_weak.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                let window = ui.window();
                window.set_maximized(!window.is_maximized());
            }
        }
    });

    welcome.show()?;
    slint::run_event_loop()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_project_uses_interrupted_save_backup() {
        let directory =
            std::env::temp_dir().join(format!("videopro-recovery-test-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create recovery test directory");
        let project = directory.join("edit.vpproj");
        let backup = project.with_extension("vpproj.bak");
        fs::write(&backup, b"previous valid bundle").expect("write recovery backup");
        assert_eq!(resolve_project_source(&project).unwrap(), backup);

        fs::write(&project, b"new bundle").expect("write main project");
        assert_eq!(resolve_project_source(&project).unwrap(), project);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn audio_media_generates_a_waveform_image() {
        if Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("Skipping waveform integration test: ffmpeg is not installed");
            return;
        }
        let directory =
            std::env::temp_dir().join(format!("videopro-waveform-test-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create waveform test directory");
        let audio = directory.join("tone.wav");
        let result = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.25",
            ])
            .arg(&audio)
            .output()
            .expect("launch ffmpeg to create waveform fixture");
        assert!(result.status.success(), "could not create audio fixture");
        let waveform = make_waveform(&audio).expect("create waveform image");
        assert!(waveform.is_file());
        let _ = fs::remove_file(waveform);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn project_bundle_is_a_zip_with_manifest_and_embedded_media() {
        let unique = format!("videopro-test-{}", std::process::id());
        let directory = std::env::temp_dir().join(&unique);
        fs::create_dir_all(&directory).expect("create test directory");
        let media_path = directory.join("sample.mp4");
        let second_media_path = directory.join("second.mov");
        let bundle_path = directory.join("sample.vpproj");
        fs::write(&media_path, b"fake-media-payload").expect("write sample media");
        fs::write(&second_media_path, b"second-media-payload").expect("write second media");

        let clips = [
            ProjectClipData {
                name: "sample.mp4".to_owned(),
                title_text: "Opening title".to_owned(),
                source_path: media_path.to_string_lossy().into_owned(),
                duration_seconds: 12.5,
                start_seconds: 3.0,
                source_in_seconds: 0.5,
                source_out_seconds: 13.0,
                track: 1,
                exposure: 12.0,
                contrast: 1.1,
                saturation: 0.9,
                opacity: 0.8,
                audio_gain_db: -3.0,
                blur: 2.0,
                sharpen: 0.0,
                transition_in_seconds: 0.25,
                transition_out_seconds: 0.75,
                scale: 1.25,
                rotation: 12.0,
                animate_scale: true,
                scale_end: 1.75,
            },
            ProjectClipData {
                name: "second.mov".to_owned(),
                title_text: String::new(),
                source_path: second_media_path.to_string_lossy().into_owned(),
                duration_seconds: 5.0,
                start_seconds: 15.5,
                source_in_seconds: 0.0,
                source_out_seconds: 5.0,
                track: 0,
                exposure: 0.0,
                contrast: 1.0,
                saturation: 1.0,
                opacity: 1.0,
                audio_gain_db: 0.0,
                blur: 0.0,
                sharpen: 0.0,
                transition_in_seconds: 0.0,
                transition_out_seconds: 0.0,
                scale: 1.0,
                rotation: 0.0,
                animate_scale: false,
                scale_end: 1.0,
            },
        ];
        let tracks = [
            AudioTrackSettings::default(),
            AudioTrackSettings {
                gain_db: -4.5,
                muted: true,
                solo: false,
            },
            AudioTrackSettings {
                gain_db: 3.0,
                muted: false,
                solo: true,
            },
            AudioTrackSettings::default(),
        ];
        save_project_bundle(&bundle_path, "Test Project", &clips, &tracks).expect("save bundle");

        let file = fs::File::open(&bundle_path).expect("open saved bundle");
        let mut archive = ZipArchive::new(file).expect("bundle is a ZIP archive");
        let mut manifest = String::new();
        archive
            .by_name("project.vpm")
            .expect("manifest exists")
            .read_to_string(&mut manifest)
            .expect("read manifest");
        assert!(manifest.starts_with("VIDEOPRO_BUNDLE\t7\nNAME\t"));
        assert!(manifest.contains("TRACK\t1\t-4.500000\t1\t0\n"));
        assert!(manifest.contains("TRACK\t2\t3.000000\t0\t1\n"));
        assert!(manifest.contains("\t12.500000\t3.000000\t0.500000\t13.000000\t1\t12.000000\t1.100000\t0.900000\t0.800000\t0.250000\t0.750000\t-3.000000\t2.000000\t0.000000\t4f70656e696e67207469746c65\t1.250000\t12.000000\t1\t1.750000\n"));
        assert!(manifest.contains("\t5.000000\t15.500000\t0.000000\t5.000000\t0\t0.000000\t1.000000\t1.000000\t1.000000\t0.000000\t0.000000\t0.000000\t0.000000\t0.000000\t\t1.000000\t0.000000\t0\t1.000000\n"));
        assert_eq!(manifest.matches("\nCLIP\t").count(), 2);

        let mut media = Vec::new();
        archive
            .by_name("media/000000.mp4")
            .expect("media is embedded")
            .read_to_end(&mut media)
            .expect("read embedded media");
        assert_eq!(media, b"fake-media-payload");

        let mut second_media = Vec::new();
        archive
            .by_name("media/000001.mov")
            .expect("second media is embedded")
            .read_to_end(&mut second_media)
            .expect("read second embedded media");
        assert_eq!(second_media, b"second-media-payload");
        drop(archive);

        let extraction_root = directory.join("extracted");
        let (loaded_name, loaded_clips, loaded_tracks) =
            load_project_bundle_into(&bundle_path, &extraction_root).expect("load bundle");
        assert_eq!(loaded_name, "Test Project");
        assert_eq!(loaded_clips.len(), 2);
        assert_eq!(loaded_clips[0].title_text, "Opening title");
        assert_eq!(loaded_clips[1].title_text, "");
        assert_eq!(loaded_clips[0].audio_gain_db, -3.0);
        assert_eq!(loaded_clips[0].blur, 2.0);
        assert_eq!(loaded_tracks[1].gain_db, -4.5);
        assert!(loaded_tracks[1].muted);
        assert_eq!(loaded_tracks[2].gain_db, 3.0);
        assert!(loaded_tracks[2].solo);
        assert_eq!(
            fs::read(&loaded_clips[0].source_path).expect("read extracted media"),
            b"fake-media-payload"
        );

        let _ = fs::remove_dir_all(directory);
    }
}
