//! Offline timeline rendering through FFmpeg.
//!
//! The UI/player is intentionally not used as the export renderer: the timeline is
//! rendered from an immutable snapshot so exports don't depend on preview state.

use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Instant,
};

static NEXT_RENDER_ID: AtomicU64 = AtomicU64::new(1);

/// A real-time audio-only timeline mix. FFmpeg decodes and mixes every active
/// audio clip, while ffplay sends the resulting PCM stream to the system audio
/// device. The UI can use the monotonic clock here as its playback clock rather
/// than following whichever source clip happens to be loaded in mpv.
pub(crate) struct TimelineAudioPlayback {
    ffmpeg: Child,
    ffplay: Child,
    started_at: Instant,
    start_position: f32,
    copier: Option<thread::JoinHandle<()>>,
}

impl TimelineAudioPlayback {
    pub(crate) fn start(
        clips: &[RenderClip],
        timeline_position: f32,
    ) -> Result<Option<Self>, RenderError> {
        let request_started_at = Instant::now();
        let end = validate(clips)?;
        if !timeline_position.is_finite() || timeline_position < 0.0 || timeline_position >= end {
            return Ok(None);
        }

        // Probe stream types off the UI thread (the caller launches this
        // function on a worker). Account for that probing time before building
        // the stream so playback starts at the clock's current position rather
        // than replaying the audio from when the worker was queued.
        let audio_capable: Vec<usize> = clips
            .iter()
            .enumerate()
            .filter(|(_, clip)| {
                clip.start + clip.duration > timeline_position
                    && clip.path.is_file()
                    && has_audio(&clip.path)
            })
            .map(|(index, _)| index)
            .collect();
        let timeline_position = timeline_position + request_started_at.elapsed().as_secs_f32();
        if timeline_position >= end {
            return Ok(None);
        }
        let candidates: Vec<usize> = audio_capable
            .into_iter()
            .filter(|index| clips[*index].start + clips[*index].duration > timeline_position)
            .collect();
        let selected = active_audio_inputs(clips, &candidates);
        if selected.is_empty() {
            return Ok(None);
        }

        let remaining = end - timeline_position;
        let mut graph = String::new();
        let mut labels = Vec::new();
        let mut inputs = Vec::new();
        for (input_index, clip_index) in selected.iter().enumerate() {
            let clip = &clips[*clip_index];
            let elapsed = (timeline_position - clip.start).max(0.0);
            let source_time = (clip.source_in + elapsed).clamp(clip.source_in, clip.source_out);
            let clip_remaining = (clip.start + clip.duration - timeline_position).max(0.0);
            let timeline_delay = (clip.start - timeline_position).max(0.0);
            let label = format!("live{input_index}");
            let mut filters = format!(
                "[{input_index}:a:0]atrim=duration={},asetpts=PTS-STARTPTS,volume={:.7}",
                fmt_seconds(clip_remaining),
                10_f32.powf((clip.audio_gain_db + clip.track_gain_db).clamp(-48.0, 24.0) / 20.0),
            );
            if elapsed < clip.fade_in && clip.fade_in > 0.001 {
                let current_gain = (elapsed / clip.fade_in).clamp(0.0001, 1.0);
                let fade_remaining = clip.fade_in - elapsed;
                if fade_remaining > 0.001 {
                    filters.push_str(&format!(
                        ",volume='if(lt(t,{}),{}+(1-{})*t/{},1)'",
                        fmt_seconds(fade_remaining),
                        current_gain,
                        current_gain,
                        fmt_seconds(fade_remaining)
                    ));
                } else {
                    filters.push_str(&format!(",volume={current_gain:.7}"));
                }
            }
            if clip.fade_out > 0.001 {
                let fade_start = (clip.duration - clip.fade_out - elapsed).max(0.0);
                if elapsed >= clip.duration - clip.fade_out {
                    let current_gain = (clip_remaining / clip.fade_out).clamp(0.0, 1.0);
                    if clip_remaining > 0.001 {
                        filters.push_str(&format!(
                            ",volume='if(lt(t,{}),{}*(1-t/{}),0)'",
                            fmt_seconds(clip_remaining),
                            current_gain,
                            fmt_seconds(clip_remaining)
                        ));
                    }
                } else if fade_start > 0.001 {
                    filters.push_str(&format!(
                        ",afade=t=out:st={}:d={}",
                        fmt_seconds(fade_start),
                        fmt_seconds(clip.fade_out),
                    ));
                }
            }
            if timeline_delay > 0.001 {
                filters.push_str(&format!(
                    ",adelay=delays={}:all=1",
                    (timeline_delay * 1000.0).round() as u64,
                ));
            }
            graph.push_str(&format!("{filters}[{label}];"));
            labels.push(format!("[{label}]"));
            inputs.push((clip.path.as_path(), source_time));
        }

        if labels.len() == 1 {
            graph.push_str(&format!(
                "{}apad,atrim=duration={}[aout]",
                labels[0],
                fmt_seconds(remaining)
            ));
        } else {
            graph.push_str(&format!(
                "{}amix=inputs={}:duration=longest:normalize=0,apad,atrim=duration={}[aout]",
                labels.join(""),
                labels.len(),
                fmt_seconds(remaining),
            ));
        }

        let mut ffplay = Command::new("ffplay")
            .args([
                "-hide_banner", "-loglevel", "warning", "-nodisp", "-autoexit", "-volume", "100",
                "-f", "f32le", "-ar", "48000", "-ch_layout", "stereo", "-i", "pipe:0",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            // Do not swallow SDL/audio-device failures. They are otherwise
            // indistinguishable from a successful but silent playback session.
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| RenderError(format!("Could not start live audio playback (ffplay): {error}. Install an FFmpeg build that includes ffplay.")))?;
        let audio_stdin = ffplay
            .stdin
            .take()
            .ok_or_else(|| RenderError("Could not open live audio output pipe".into()))?;

        let mut command = Command::new("ffmpeg");
        command.args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-nostdin",
            "-filter_complex_threads",
            "1",
        ]);
        for (path, source_time) in inputs {
            command
                .arg("-ss")
                .arg(fmt_seconds(source_time))
                .arg("-i")
                .arg(path);
        }
        let mut ffmpeg = match command
            .args([
                "-filter_complex",
                &graph,
                "-map",
                "[aout]",
                "-t",
                &fmt_seconds(remaining),
                "-vn",
                "-sn",
                "-dn",
                "-ac",
                "2",
                "-ar",
                "48000",
                "-c:a",
                "pcm_f32le",
                "-f",
                "f32le",
                "pipe:1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = ffplay.kill();
                let _ = ffplay.wait();
                return Err(RenderError(format!(
                    "Could not start live audio mixer (ffmpeg): {error}"
                )));
            }
        };
        let audio_stdout = ffmpeg
            .stdout
            .take()
            .ok_or_else(|| RenderError("Could not open live audio mixer pipe".into()))?;
        let copier = thread::spawn(move || {
            let mut source = audio_stdout;
            let mut destination = audio_stdin;
            let _ = std::io::copy(&mut source, &mut destination);
        });

        Ok(Some(Self {
            ffmpeg,
            ffplay,
            started_at: Instant::now(),
            start_position: timeline_position,
            copier: Some(copier),
        }))
    }

    pub(crate) fn position(&mut self) -> Option<f32> {
        if self.ffplay.try_wait().ok().flatten().is_some() {
            return None;
        }
        Some(self.start_position + self.started_at.elapsed().as_secs_f32())
    }

    pub(crate) fn clock_anchor(&self) -> (f32, Instant) {
        (self.start_position, self.started_at)
    }

    pub(crate) fn stop(&mut self) {
        let _ = self.ffmpeg.kill();
        let _ = self.ffplay.kill();
        let _ = self.ffmpeg.wait();
        let _ = self.ffplay.wait();
        if let Some(copier) = self.copier.take() {
            let _ = copier.join();
        }
    }
}

impl Drop for TimelineAudioPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RenderClip {
    pub path: PathBuf,
    pub duration: f32,
    pub start: f32,
    pub source_in: f32,
    pub source_out: f32,
    /// 0 and 1 are video tracks (0 is composited above 1); 2 and 3 are audio lanes.
    pub track: i32,
    pub exposure: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub opacity: f32,
    pub audio_gain_db: f32,
    /// Track-level gain, applied in addition to the clip's own gain.
    pub track_gain_db: f32,
    pub track_muted: bool,
    pub track_solo: bool,
    pub solo_mode: bool,
    pub blur: f32,
    pub sharpen: f32,
    pub fade_in: f32,
    pub fade_out: f32,
}

#[derive(Debug)]
pub(crate) struct RenderError(pub String);

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AudioLevel {
    pub rms: f32,
    pub peak: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AudioMeterSnapshot {
    pub tracks: [AudioLevel; 4],
    pub master: AudioLevel,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RenderError {}

fn fmt_seconds(value: f32) -> String {
    format!("{value:.6}")
}

pub(crate) fn has_audio(path: &Path) -> bool {
    Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| output.status.success() && !output.stdout.is_empty())
}

pub(crate) fn has_video(path: &Path) -> bool {
    Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| output.status.success() && !output.stdout.is_empty())
}

fn fit_video_size(width: u32, height: u32) -> Option<(u32, u32)> {
    if width < 2 || height < 2 {
        return None;
    }
    let scale = (3840.0 / width as f64).min(2160.0 / height as f64).min(1.0);
    let scaled_width = ((width as f64 * scale).round() as u32).max(2) & !1;
    let scaled_height = ((height as f64 * scale).round() as u32).max(2) & !1;
    Some((scaled_width, scaled_height))
}

fn validate(clips: &[RenderClip]) -> Result<f32, RenderError> {
    if clips.is_empty() {
        return Err(RenderError(
            "Add media to the timeline before exporting.".into(),
        ));
    }
    let mut end = 0.0_f32;
    for (index, clip) in clips.iter().enumerate() {
        if !clip.path.is_file() {
            return Err(RenderError(format!(
                "Media is missing: {}",
                clip.path.display()
            )));
        }
        let values = [
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
        ];
        if !values.iter().all(|v| v.is_finite())
            || clip.duration <= 0.0
            || clip.start < 0.0
            || clip.source_in < 0.0
            || clip.source_out <= clip.source_in
            || !(0..=3).contains(&clip.track)
            || !(-24.0..=12.0).contains(&clip.audio_gain_db)
            || !(-24.0..=12.0).contains(&clip.track_gain_db)
            || !(0.0..=10.0).contains(&clip.blur)
            || !(0.0..=5.0).contains(&clip.sharpen)
        {
            return Err(RenderError(format!(
                "Clip {} has invalid timeline values.",
                index + 1
            )));
        }
        if (clip.source_out - clip.source_in - clip.duration).abs() > 0.25 {
            return Err(RenderError(format!(
                "Clip {} has inconsistent trim duration.",
                index + 1
            )));
        }
        end = end.max(clip.start + clip.duration);
    }
    Ok(end.max(0.05))
}

fn active_audio_inputs(clips: &[RenderClip], candidates: &[usize]) -> Vec<usize> {
    let any_solo = clips.iter().any(|clip| clip.solo_mode);
    candidates
        .iter()
        .copied()
        .filter(|index| {
            let clip = &clips[*index];
            !clip.track_muted && (!any_solo || clip.track_solo)
        })
        .collect()
}

fn build_filter_graph(
    clips: &[RenderClip],
    video_inputs: &[usize],
    audio_inputs: &[usize],
    duration: f32,
    width: u32,
    height: u32,
) -> String {
    let mut graph = format!(
        "color=c=black:s={width}x{height}:r=30:d={}[base0]",
        fmt_seconds(duration),
    );
    let mut video_clips: Vec<(usize, &RenderClip)> = clips
        .iter()
        .enumerate()
        .filter(|(index, clip)| clip.track <= 1 && video_inputs.contains(index))
        .collect();
    // Paint the lower video lane first, then the upper lane. Within a lane,
    // later clips in the project list are placed above earlier overlapping clips.
    video_clips.sort_by_key(|(index, clip)| (std::cmp::Reverse(clip.track), *index));
    let mut base_index = 0;
    for (layer_position, (input_index, clip)) in video_clips.iter().enumerate() {
        let label = format!("v{input_index}");
        let mut filters = format!(
            "[{input_index}:v:0]trim=start={}:duration={},setpts=PTS-STARTPTS,scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1,eq=brightness={}:contrast={}:saturation={}",
            fmt_seconds(clip.source_in),
            fmt_seconds(clip.duration),
            (clip.exposure / 100.0).clamp(-1.0, 1.0),
            clip.contrast.clamp(0.0, 2.0),
            clip.saturation.clamp(0.0, 3.0),
        );
        if clip.blur > 0.001 {
            filters.push_str(&format!(",boxblur={:.3}:1", clip.blur.clamp(0.0, 10.0)));
        }
        if clip.sharpen > 0.001 {
            filters.push_str(&format!(
                ",unsharp=5:5:{:.3}:5:5:0",
                clip.sharpen.clamp(0.0, 5.0)
            ));
        }
        filters.push_str(&format!(
            ",format=rgba,colorchannelmixer=aa={}",
            clip.opacity.clamp(0.0, 1.0)
        ));
        if clip.fade_in > 0.001 {
            filters.push_str(&format!(
                ",fade=t=in:st=0:d={}:alpha=1",
                fmt_seconds(clip.fade_in.min(clip.duration / 2.0))
            ));
        }
        // When a higher/later layer overlaps the outgoing clip's tail, keep the
        // outgoing image opaque and fade the incoming layer over it. That produces
        // an actual dissolve instead of fading both pictures toward black.
        let dissolves_into_next = video_clips
            .iter()
            .skip(layer_position + 1)
            .any(|(_, next)| {
                next.fade_in > 0.001
                    && next.start < clip.start + clip.duration
                    && next.start + next.duration > clip.start + clip.duration - clip.fade_out
            });
        if clip.fade_out > 0.001 && !dissolves_into_next {
            filters.push_str(&format!(
                ",fade=t=out:st={}:d={}:alpha=1",
                fmt_seconds((clip.duration - clip.fade_out).max(0.0)),
                fmt_seconds(clip.fade_out.min(clip.duration / 2.0))
            ));
        }
        filters.push_str(&format!(
            ",setpts=PTS-STARTPTS+{}/TB[{label}]",
            fmt_seconds(clip.start)
        ));
        graph.push(';');
        graph.push_str(&filters);
        let next_base = format!("base{}", base_index + 1);
        graph.push_str(&format!(
            ";[base{base_index}][{label}]overlay=0:0:eof_action=pass:shortest=0:format=auto:enable='between(t,{},{})'[{next_base}]",
            fmt_seconds(clip.start), fmt_seconds(clip.start + clip.duration)
        ));
        base_index += 1;
    }
    let final_video = format!("[base{base_index}]");
    graph.push_str(&format!(
        ";{final_video}trim=duration={},setpts=PTS-STARTPTS,format=yuv420p[vout]",
        fmt_seconds(duration)
    ));

    let mut audio_labels = Vec::new();
    for (audio_number, input_index) in audio_inputs.iter().enumerate() {
        let clip = &clips[*input_index];
        let label = format!("a{audio_number}");
        let mut filters = format!(
            "[{}:a:0]atrim=start={}:duration={},asetpts=PTS-STARTPTS,volume={:.6},adelay=delays={}:all=1",
            input_index,
            fmt_seconds(clip.source_in),
            fmt_seconds(clip.duration),
            10_f32.powf((clip.audio_gain_db + clip.track_gain_db).clamp(-48.0, 24.0) / 20.0),
            (clip.start * 1000.0).round().max(0.0) as u64,
        );
        if clip.fade_in > 0.001 {
            filters.push_str(&format!(
                ",afade=t=in:st=0:d={}",
                fmt_seconds(clip.fade_in.min(clip.duration / 2.0))
            ));
        }
        if clip.fade_out > 0.001 {
            filters.push_str(&format!(
                ",afade=t=out:st={}:d={}",
                fmt_seconds((clip.duration - clip.fade_out).max(0.0)),
                fmt_seconds(clip.fade_out.min(clip.duration / 2.0))
            ));
        }
        filters.push_str(&format!("[{label}]"));
        graph.push(';');
        graph.push_str(&filters);
        audio_labels.push(format!("[{label}]"));
    }
    if audio_labels.len() == 1 {
        graph.push_str(&format!(
            ";{}apad,atrim=duration={}[aout]",
            audio_labels[0],
            fmt_seconds(duration)
        ));
    } else if audio_labels.len() > 1 {
        graph.push(';');
        graph.push_str(&audio_labels.join(""));
        graph.push_str(&format!(
            "amix=inputs={}:duration=longest:normalize=0,apad,atrim=duration={}[aout]",
            audio_labels.len(),
            fmt_seconds(duration)
        ));
    }
    graph
}

/// Render one low-resolution frame from the same FFmpeg compositing graph used
/// by export. Only clips visible at `timeline_time` are opened, and each input
/// is seeked close to its source frame so scrubbing does not render the whole
/// project from its beginning. This is intentionally an offline preview path:
/// it is throttled by the UI and always runs on a worker thread.
pub(crate) fn render_preview_frame(
    clips: &[RenderClip],
    timeline_time: f32,
    output: &Path,
) -> Result<(), RenderError> {
    if !timeline_time.is_finite() || timeline_time < 0.0 {
        return Err(RenderError(
            "Preview time must be a finite, non-negative value.".into(),
        ));
    }

    const FRAME_WINDOW: f32 = 0.05;
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 360;
    let mut active: Vec<(usize, RenderClip)> = clips
        .iter()
        .enumerate()
        .filter(|(_, clip)| {
            clip.track <= 1
                && timeline_time >= clip.start
                && timeline_time < clip.start + clip.duration
                && clip.path.is_file()
                && has_video(&clip.path)
        })
        .map(|(index, clip)| (index, clip.clone()))
        .collect();

    // The export graph paints lower tracks first and later clips over earlier
    // clips on the same track. Keep that original ordering in this snapshot.
    active.sort_by_key(|(index, clip)| (std::cmp::Reverse(clip.track), *index));
    let mut preview_clips = Vec::with_capacity(active.len());
    let mut source_times = Vec::with_capacity(active.len());
    for (clip_index, mut clip) in active {
        let elapsed = (timeline_time - clip.start).max(0.0);
        let source_time = (clip.source_in + elapsed).clamp(clip.source_in, clip.source_out);
        let mut alpha = clip.opacity.clamp(0.0, 1.0);
        if clip.fade_in > 0.001 && elapsed < clip.fade_in {
            alpha *= (elapsed / clip.fade_in).clamp(0.0, 1.0);
        }
        let dissolves_into_next = clips.iter().enumerate().any(|(next_index, next)| {
            next.track <= 1
                && (next.track < clip.track
                    || (next.track == clip.track && next_index > clip_index))
                && next.fade_in > 0.001
                && next.start < clip.start + clip.duration
                && next.start + next.duration > clip.start + clip.duration - clip.fade_out
                && timeline_time >= next.start
                && timeline_time < next.start + next.duration
        });
        let remaining = clip.start + clip.duration - timeline_time;
        if clip.fade_out > 0.001 && !dissolves_into_next && remaining < clip.fade_out {
            alpha *= (remaining / clip.fade_out).clamp(0.0, 1.0);
        }
        source_times.push(source_time);
        clip.start = 0.0;
        clip.duration = FRAME_WINDOW;
        clip.source_in = 0.0;
        clip.source_out = FRAME_WINDOW;
        clip.fade_in = 0.0;
        clip.fade_out = 0.0;
        clip.opacity = alpha;
        preview_clips.push(clip);
    }

    let video_inputs: Vec<usize> = (0..preview_clips.len()).collect();
    let graph = build_filter_graph(
        &preview_clips,
        &video_inputs,
        &[],
        FRAME_WINDOW,
        WIDTH,
        HEIGHT,
    );
    let mut command = Command::new("ffmpeg");
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-nostdin",
        "-filter_complex_threads",
        "1",
    ]);
    for (clip, source_time) in preview_clips.iter().zip(source_times) {
        command
            .arg("-ss")
            .arg(fmt_seconds(source_time))
            .arg("-i")
            .arg(&clip.path);
    }
    command
        .args([
            "-filter_complex",
            &graph,
            "-map",
            "[vout]",
            "-frames:v",
            "1",
        ])
        .arg(output);
    let result = command
        .output()
        .map_err(|error| RenderError(format!("Could not start FFmpeg preview: {error}")))?;
    if !result.status.success() {
        let _ = std::fs::remove_file(output);
        return Err(RenderError(format!(
            "FFmpeg preview failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        )));
    }
    Ok(())
}

/// Analyze a short, real decoded audio window around the playhead. This is
/// deliberately separate from waveform generation: levels are measured after
/// clip and track gain and mute/solo routing, not inferred from a static image.
pub(crate) fn measure_track_levels(
    clips: &[RenderClip],
    timeline_time: f32,
) -> Result<AudioMeterSnapshot, RenderError> {
    const WINDOW: f32 = 0.12;
    let candidates: Vec<usize> = clips
        .iter()
        .enumerate()
        .filter(|(_, clip)| {
            timeline_time >= clip.start
                && timeline_time < clip.start + clip.duration
                && clip.path.is_file()
                && has_audio(&clip.path)
        })
        .map(|(index, _)| index)
        .collect();
    let selected = active_audio_inputs(clips, &candidates);
    if selected.is_empty() {
        return Ok(AudioMeterSnapshot::default());
    }

    let mut graph = String::new();
    let mut track_labels: [Vec<String>; 4] = std::array::from_fn(|_| Vec::new());
    let mut clip_labels = Vec::new();
    let mut source_times = Vec::new();
    for (input_index, clip_index) in selected.iter().enumerate() {
        let clip = &clips[*clip_index];
        let elapsed = (timeline_time - clip.start).max(0.0);
        let source_time = (clip.source_in + elapsed).clamp(clip.source_in, clip.source_out);
        let mut gain_db = clip.audio_gain_db + clip.track_gain_db;
        if clip.fade_in > 0.001 && elapsed < clip.fade_in {
            gain_db += 20.0 * (elapsed / clip.fade_in).clamp(0.0001, 1.0).log10();
        }
        let remaining = clip.start + clip.duration - timeline_time;
        if clip.fade_out > 0.001 && remaining < clip.fade_out {
            gain_db += 20.0 * (remaining / clip.fade_out).clamp(0.0001, 1.0).log10();
        }
        let label = format!("aud{input_index}");
        graph.push_str(&format!(
            "[{}:a:0]atrim=duration={},asetpts=PTS-STARTPTS,volume={:.6}[{}];",
            input_index,
            fmt_seconds(WINDOW),
            10_f32.powf(gain_db.clamp(-96.0, 24.0) / 20.0),
            label
        ));
        track_labels[clip.track.clamp(0, 3) as usize].push(format!("[{label}]"));
        clip_labels.push(format!("[{label}]"));
        source_times.push(source_time);
    }

    let mut active_tracks = Vec::new();
    for (track, labels) in track_labels.iter().enumerate() {
        if labels.is_empty() {
            continue;
        }
        let mixed = format!("trackmix{track}");
        if labels.len() == 1 {
            graph.push_str(&format!("{}anull[{mixed}];", labels[0]));
        } else {
            graph.push_str(&format!(
                "{}amix=inputs={}:duration=longest:normalize=0[{mixed}];",
                labels.join(""),
                labels.len()
            ));
        }
        graph.push_str(&format!(
            "[{mixed}]ametadata=mode=add:key=videopro.track:value={track},astats=metadata=1:reset=1,ametadata=print:file=-[meter{track}];"
        ));
        active_tracks.push(track);
    }
    let master_input = if clip_labels.len() == 1 {
        clip_labels[0].clone()
    } else {
        graph.push_str(&format!(
            "{}amix=inputs={}:duration=longest:normalize=0[mastermix];",
            clip_labels.join(""),
            clip_labels.len()
        ));
        "[mastermix]".to_owned()
    };
    graph.push_str(&format!(
        "{master_input}ametadata=mode=add:key=videopro.track:value=4,astats=metadata=1:reset=1,ametadata=print:file=-[meter4]"
    ));

    let mut command = Command::new("ffmpeg");
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-filter_complex_threads",
        "1",
    ]);
    for (clip_index, source_time) in selected.iter().zip(source_times) {
        command
            .arg("-ss")
            .arg(fmt_seconds(source_time))
            .arg("-i")
            .arg(&clips[*clip_index].path);
    }
    command.arg("-filter_complex").arg(&graph);
    for track in active_tracks {
        command.args(["-map", &format!("[meter{track}]"), "-f", "null", "-"]);
    }
    command.args(["-map", "[meter4]", "-f", "null", "-"]);
    let output = command
        .output()
        .map_err(|error| RenderError(format!("Could not start audio meter analysis: {error}")))?;
    if !output.status.success() {
        return Err(RenderError(format!(
            "Audio meter analysis failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let mut snapshot = AudioMeterSnapshot::default();
    let mut current_track = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(value) = line.strip_prefix("videopro.track=") {
            current_track = value.parse::<usize>().ok().filter(|track| *track <= 4);
            continue;
        }
        let Some(track) = current_track else { continue };
        let target = if track == 4 {
            &mut snapshot.master
        } else {
            &mut snapshot.tracks[track]
        };
        if let Some(value) = line.strip_prefix("lavfi.astats.Overall.RMS_level=") {
            if let Ok(db) = value.parse::<f32>() {
                target.rms = ((db.clamp(-60.0, 0.0) + 60.0) / 60.0).clamp(0.0, 1.0);
            }
        } else if let Some(value) = line.strip_prefix("lavfi.astats.Overall.Peak_level=")
            && let Ok(db) = value.parse::<f32>()
        {
            target.peak = ((db.clamp(-60.0, 0.0) + 60.0) / 60.0).clamp(0.0, 1.0);
        }
    }
    Ok(snapshot)
}

/// Render the immutable timeline snapshot to an MP4 file. FFmpeg runs off the UI thread.
#[cfg(test)]
pub(crate) fn render_mp4_with_progress(
    clips: &[RenderClip],
    output: &Path,
    on_progress: impl FnMut(f32),
) -> Result<(), RenderError> {
    render_mp4_with_options(clips, output, 1920, 1080, "High", on_progress)
}

pub(crate) fn render_mp4_with_options(
    clips: &[RenderClip],
    output: &Path,
    requested_width: u32,
    requested_height: u32,
    quality: &str,
    mut on_progress: impl FnMut(f32),
) -> Result<(), RenderError> {
    let duration = validate(clips)?;
    let output_parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !output_parent.is_dir() {
        return Err(RenderError(format!(
            "The export folder does not exist: {}",
            output_parent.display()
        )));
    }
    let file_name = output
        .file_name()
        .ok_or_else(|| RenderError("Choose a valid filename for the exported video.".into()))?;
    let render_id = NEXT_RENDER_ID.fetch_add(1, Ordering::Relaxed);
    let temporary_output = output_parent.join(format!(
        ".{}.videopro-{}-{render_id}.mp4",
        file_name.to_string_lossy(),
        std::process::id(),
    ));
    let video_inputs: Vec<usize> = clips
        .iter()
        .enumerate()
        .filter(|(_, clip)| clip.track <= 1)
        .filter_map(|(index, clip)| has_video(&clip.path).then_some(index))
        .collect();
    let (width, height) = fit_video_size(requested_width, requested_height)
        .ok_or_else(|| RenderError("Choose a valid output resolution.".into()))?;
    let audio_candidates: Vec<usize> = clips
        .iter()
        .enumerate()
        .filter(|(_, clip)| has_audio(&clip.path))
        .map(|(index, _)| index)
        .collect();
    let audio_inputs = active_audio_inputs(clips, &audio_candidates);
    let graph = build_filter_graph(clips, &video_inputs, &audio_inputs, duration, width, height);
    let mut command = Command::new("ffmpeg");
    let (preset, crf) = match quality {
        "Draft" => ("ultrafast", "28"),
        "Standard" => ("fast", "23"),
        _ => ("medium", "18"),
    };
    command.args([
        "-hide_banner",
        "-y",
        "-nostdin",
        "-loglevel",
        "error",
        "-filter_complex_threads",
        "1",
        "-stats_period",
        "0.5",
        "-progress",
        "pipe:1",
        "-nostats",
    ]);
    for clip in clips {
        command.arg("-i").arg(&clip.path);
    }
    command.args(["-filter_complex", &graph, "-map", "[vout]"]);
    if !audio_inputs.is_empty() {
        command.args(["-map", "[aout]"]);
    }
    command.args([
        "-t",
        &fmt_seconds(duration),
        "-frames:v",
        &((duration * 30.0).ceil().max(1.0) as u64).to_string(),
        "-c:v",
        "libx264",
        "-preset",
        preset,
        "-crf",
        crf,
        "-pix_fmt",
        "yuv420p",
        "-movflags",
        "+faststart",
    ]);
    if !audio_inputs.is_empty() {
        command.args(["-c:a", "aac", "-b:a", "192k"]);
    }
    let mut child = command
        .arg(&temporary_output)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            RenderError(format!(
                "Could not launch FFmpeg: {error}. Install FFmpeg and ensure it is on PATH."
            ))
        })?;
    let stderr_reader = child.stderr.take().map(|mut stderr| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).map(|_| bytes)
        })
    });
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(value) = line.strip_prefix("out_time_ms=")
                && let Ok(microseconds) = value.parse::<f64>()
            {
                on_progress(
                    (microseconds / (duration as f64 * 1_000_000.0)).clamp(0.0, 1.0) as f32,
                );
            }
            if line == "progress=end" {
                on_progress(1.0);
            }
        }
    }
    let output_result = child
        .wait_with_output()
        .map_err(|error| RenderError(error.to_string()))?;
    if !output_result.status.success() {
        let _ = std::fs::remove_file(&temporary_output);
        let stderr_bytes = stderr_reader
            .and_then(|reader| reader.join().ok())
            .and_then(Result::ok)
            .unwrap_or(output_result.stderr);
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        return Err(RenderError(format!(
            "FFmpeg export failed: {}",
            stderr
                .lines()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        )));
    }

    // Never render directly over the user's existing export. Only publish the
    // new file after FFmpeg has completed successfully; restore the old file if
    // the final rename fails (notably on platforms that cannot replace files).
    if output.exists() {
        let backup = output_parent.join(format!(
            ".{}.videopro-backup-{}-{render_id}",
            file_name.to_string_lossy(),
            std::process::id(),
        ));
        if let Err(error) = std::fs::rename(output, &backup) {
            let _ = std::fs::remove_file(&temporary_output);
            return Err(RenderError(format!(
                "The video rendered, but the existing export could not be replaced: {error}"
            )));
        }
        if let Err(error) = std::fs::rename(&temporary_output, output) {
            let _ = std::fs::rename(&backup, output);
            let _ = std::fs::remove_file(&temporary_output);
            return Err(RenderError(format!(
                "The video rendered, but could not be saved to the destination: {error}"
            )));
        }
        let _ = std::fs::remove_file(backup);
    } else if let Err(error) = std::fs::rename(&temporary_output, output) {
        let _ = std::fs::remove_file(&temporary_output);
        return Err(RenderError(format!(
            "The video rendered, but could not be saved to the destination: {error}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(track: i32, start: f32) -> RenderClip {
        RenderClip {
            path: PathBuf::from("media.mp4"),
            duration: 4.0,
            start,
            source_in: 1.0,
            source_out: 5.0,
            track,
            exposure: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            opacity: 1.0,
            audio_gain_db: 0.0,
            track_gain_db: 0.0,
            track_muted: false,
            track_solo: false,
            solo_mode: false,
            blur: 0.0,
            sharpen: 0.0,
            fade_in: 0.5,
            fade_out: 0.5,
        }
    }

    #[test]
    fn output_dimensions_preserve_aspect_ratio_within_limits() {
        assert_eq!(fit_video_size(7680, 4320), Some((3840, 2160)));
        assert_eq!(fit_video_size(1080, 1920), Some((1080, 1920)));
        assert_eq!(fit_video_size(1920, 1080), Some((1920, 1080)));
        assert_eq!(fit_video_size(1, 1080), None);
    }

    #[test]
    fn graph_composites_video_tracks_and_mixes_audio() {
        let clips = vec![clip(1, 0.0), clip(0, 2.0), clip(2, 1.0), clip(3, 3.0)];
        let graph = build_filter_graph(&clips, &[0, 1], &[0, 1, 2, 3], 7.0, 1920, 1080);
        assert!(graph.contains("overlay=0:0"));
        assert!(graph.contains("between(t,2.000000,6.000000)"));
        assert!(graph.contains("amix=inputs=4"));
        assert!(graph.contains("[vout]"));
        assert!(graph.contains("[aout]"));
    }

    #[test]
    fn graph_preserves_trim_and_effect_parameters() {
        let mut video = clip(1, 0.0);
        video.exposure = 25.0;
        video.contrast = 1.25;
        video.saturation = 0.75;
        video.opacity = 0.8;
        video.blur = 2.0;
        video.sharpen = 1.5;
        let graph = build_filter_graph(&[video], &[0], &[], 4.0, 1920, 1080);
        assert!(graph.contains("trim=start=1.000000:duration=4.000000"));
        assert!(graph.contains("brightness=0.25:contrast=1.25:saturation=0.75"));
        assert!(graph.contains("colorchannelmixer=aa=0.8"));
        assert!(graph.contains("boxblur=2.000:1"));
        assert!(graph.contains("unsharp=5:5:1.500:5:5:0"));
        assert!(graph.contains("fade=t=in"));
        assert!(graph.contains("fade=t=out"));
    }

    #[test]
    fn overlapping_clip_without_fade_in_does_not_suppress_outgoing_fade() {
        let outgoing = clip(1, 0.0);
        let mut incoming = clip(0, 2.0);
        incoming.fade_in = 0.0;
        let graph = build_filter_graph(&[outgoing, incoming], &[0, 1], &[], 6.0, 1920, 1080);
        assert!(graph.contains("fade=t=out:st=3.500000:d=0.500000:alpha=1"));
    }

    #[test]
    fn audio_gain_is_converted_from_decibels_to_linear_volume() {
        let mut audio = clip(2, 0.0);
        audio.audio_gain_db = -6.0;
        let graph = build_filter_graph(&[audio], &[], &[0], 4.0, 1920, 1080);
        assert!(graph.contains("volume=0.501187"));
    }

    #[test]
    fn track_mute_and_solo_select_only_audible_tracks() {
        let mut clips = vec![clip(1, 0.0), clip(2, 0.0), clip(3, 0.0)];
        clips[1].track_muted = true;
        assert_eq!(active_audio_inputs(&clips, &[0, 1, 2]), vec![0, 2]);

        clips[2].track_solo = true;
        clips[0].solo_mode = true;
        clips[1].solo_mode = true;
        clips[2].solo_mode = true;
        assert_eq!(active_audio_inputs(&clips, &[0, 1, 2]), vec![2]);
    }

    #[test]
    fn track_gain_is_combined_with_clip_gain() {
        let mut audio = clip(2, 0.0);
        audio.audio_gain_db = -6.0;
        audio.track_gain_db = 6.0;
        let graph = build_filter_graph(&[audio], &[], &[0], 4.0, 1920, 1080);
        assert!(graph.contains("volume=1.000000"));
    }

    #[test]
    fn graph_does_not_treat_audio_only_media_as_video() {
        let clips = vec![clip(1, 0.0), clip(1, 1.0)];
        let graph = build_filter_graph(&clips, &[0], &[1], 5.0, 1920, 1080);
        assert!(graph.contains("[0:v:0]"));
        assert!(!graph.contains("[1:v:0]"));
        assert!(graph.contains("[1:a:0]"));
    }

    #[test]
    fn ffmpeg_can_render_overlapping_video_and_mixed_audio() {
        if Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("Skipping FFmpeg integration test: ffmpeg is not installed");
            return;
        }
        let root =
            std::env::temp_dir().join(format!("videopro-render-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create renderer test directory");
        let red = root.join("red.mp4");
        let blue = root.join("blue.mp4");
        let silent = root.join("silent.mp4");
        let output = root.join("output.mp4");
        for (path, color, frequency) in [(&red, "red", "440"), (&blue, "blue", "660")] {
            let result = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                ])
                .arg(format!("color=c={color}:s=16x16:r=30:d=1"))
                .args(["-f", "lavfi", "-i"])
                .arg(format!("sine=frequency={frequency}:duration=1"))
                .args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-crf",
                    "35",
                    "-b:a",
                    "16k",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-shortest",
                ])
                .arg(path)
                .output()
                .expect("launch FFmpeg to create test clip");
            assert!(
                result.status.success(),
                "test media generation failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let silent_result = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("color=c=green:s=16x16:r=30:d=1")
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-crf",
                "35",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&silent)
            .output()
            .expect("launch FFmpeg to create silent test clip");
        assert!(
            silent_result.status.success(),
            "silent test media generation failed: {}",
            String::from_utf8_lossy(&silent_result.stderr)
        );
        assert!(
            !has_audio(&silent),
            "video-only media must not be mapped as audio"
        );
        assert!(
            has_audio(&root.join("red.mp4")),
            "audio stream should be detected"
        );
        let clips = vec![
            RenderClip {
                path: red,
                duration: 1.0,
                start: 0.0,
                source_in: 0.0,
                source_out: 1.0,
                track: 1,
                exposure: 0.0,
                contrast: 1.0,
                saturation: 1.0,
                opacity: 1.0,
                audio_gain_db: 0.0,
                track_gain_db: 0.0,
                track_muted: false,
                track_solo: false,
                solo_mode: false,
                blur: 0.0,
                sharpen: 0.0,
                fade_in: 0.0,
                fade_out: 0.0,
            },
            RenderClip {
                path: blue,
                duration: 1.0,
                start: 0.5,
                source_in: 0.0,
                source_out: 1.0,
                track: 0,
                exposure: 0.0,
                contrast: 1.0,
                saturation: 1.0,
                opacity: 0.8,
                audio_gain_db: -3.0,
                track_gain_db: 0.0,
                track_muted: false,
                track_solo: false,
                solo_mode: false,
                blur: 1.0,
                sharpen: 0.5,
                fade_in: 0.1,
                fade_out: 0.1,
            },
        ];
        let preview_frame = root.join("preview.png");
        render_preview_frame(&clips, 0.75, &preview_frame)
            .expect("render a composited preview frame");
        let center_pixel = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(&preview_frame)
            .args([
                "-vf",
                "crop=1:1:320:180,format=rgb24",
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-",
            ])
            .output()
            .expect("read a preview pixel");
        assert!(center_pixel.status.success());
        assert_eq!(center_pixel.stdout.len(), 3);
        assert!(
            center_pixel.stdout[2] > center_pixel.stdout[0],
            "the higher V2 blue clip should composite over the lower red clip"
        );

        let levels = measure_track_levels(&clips, 0.75).expect("measure live audio levels");
        assert!(
            levels.tracks[0].peak > 0.0,
            "V2 peak meter should see audio"
        );
        assert!(
            levels.tracks[1].peak > 0.0,
            "V1 peak meter should see audio"
        );
        assert!(
            levels.master.peak > 0.0,
            "master meter should see the mixed signal"
        );
        let mut muted_clips = clips.clone();
        muted_clips[1].track_muted = true;
        let muted_levels =
            measure_track_levels(&muted_clips, 0.75).expect("measure levels with V2 muted");
        assert_eq!(muted_levels.tracks[0].peak, 0.0);
        assert!(muted_levels.tracks[1].peak > 0.0);

        render_mp4_with_progress(&clips, &output, |_| {})
            .expect("render overlapping video clips and mixed audio");
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ])
            .arg(&output)
            .output()
            .expect("launch ffprobe");
        assert!(
            probe.status.success(),
            "ffprobe could not read rendered MP4"
        );
        let duration: f32 = String::from_utf8_lossy(&probe.stdout)
            .trim()
            .parse()
            .expect("parse rendered duration");
        assert!(
            (duration - 1.5).abs() < 0.15,
            "unexpected export duration: {duration}"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
