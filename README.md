# VideoPro

VideoPro is a native Rust + Slint desktop video editor prototype with an MPV-based preview and an FFmpeg timeline renderer. It is under active development; the current build is not yet a production release.

## Development

Requirements:

- Rust toolchain compatible with the `edition = "2024"` manifest.
- `ffmpeg`, `ffprobe`, and `ffplay` available on `PATH` for media probing, thumbnails, timeline audio mixing, waveforms, and export.
- A desktop graphics environment supported by Slint's WGPU renderer and the MPV engine.

Run locally:

```sh
cargo run --locked
```

Build an unsigned macOS application bundle (requires FFmpeg/ffprobe/ffplay on `PATH`):

```sh
./scripts/package-macos.sh
```

The script creates `dist/VideoPro.app` and deliberately refuses to overwrite an existing bundle. It does not sign or notarize the app, and it does not embed FFmpeg/ffprobe/ffplay. Set `VIDEOPRO_PACKAGE_DIR` to choose another output directory or `VIDEOPRO_BUNDLE_ID` to set the bundle identifier.

Run the repeatable checks:

```sh
cargo fmt --check
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
git diff --check
```

The FFmpeg integration test generates tiny fixtures and renders a short overlapping multi-track timeline. It needs FFmpeg/ffprobe and writable temporary storage. If the machine is nearly out of disk space, free space before running it; do not delete unrelated project data to make the test pass.

## Current capabilities and known gaps

- Timeline clip placement, trimming, clip properties, fades, two video lanes, two audio lanes, project bundles, and MP4 export.
- Timeline editing actions: split at the playhead (`B`), duplicate the selected clip (`Ctrl/Cmd+Shift+D`), ripple-delete on the same track, one-frame nudging, and undo/redo. Dragging uses incremental pointer movement; short clip hit areas no longer distort their seek mapping.
- Export jobs are captured as immutable timeline/settings snapshots and rendered serially in a queue. Reusing an output path already queued or rendering is rejected. Each job retains its selected resolution, quality, frame rate, codec, and bitrate.
- A lightweight recovery snapshot is written periodically under the VideoPro application-support directory. It stores edit metadata and source paths rather than duplicating media; use **Recover** in the editor toolbar to restore it. The normal Save command still creates a portable bundle with embedded media.
- Clip audio gain plus per-track gain, mute, and solo, persisted in project bundle manifest version 4. Versions 1–3 remain readable.
- The live timeline mix and export mix now use a master limiter to reduce clipping from overlapping tracks. Color-adjustment preview brightness uses the same normalized exposure scale as the FFmpeg preview/export pipeline.
- Audio-track waveform images are generated off the UI thread for media with audio. Playback now builds a continuous FFmpeg PCM stream from all active timeline audio clips and sends it to `ffplay`; the timeline clock is anchored to the stream startup, and seek/pause/resume or mixer edits restart the stream at the current timeline position. Clip/track gain, mute/solo, trim offsets, timeline offsets, and fades are applied in the live mix.
- The visible preview frame now uses the FFmpeg timeline compositor shared with export, including simultaneous video lanes, ordering, opacity, color adjustments, blur/sharpen, and transition fades. Preview frames are low-resolution and throttled on a background worker to avoid blocking Slint's event loop; this improves visual consistency but is not a real-time native compositor.
- Project saves use a same-directory temporary bundle and backup fallback; if the main project path is missing after an interrupted replacement, loading can recover from its `.vpproj.bak` file. Missing embedded media can be recovered from its recorded original path when that file still exists.
- Audio meter values remain separate short FFmpeg source-window analysis, not a tap of the exact samples emitted by the live output stream. The audio stream is regenerated after seeks and timeline/mixer edits rather than dynamically reconfiguring filters in-place; frequent edits during playback can briefly interrupt audio. Video preview frames remain low-resolution and throttled rather than native real-time compositing. Waveform cache invalidation for changed-in-place source files, a manual missing-media relink dialog, and exhaustive end-to-end UI automation still need work.
- Undo/redo is still clip-state history rather than a complete command history for media bins, project metadata, and track-mixer settings. Color controls currently provide exposure, contrast, saturation, and effect adjustments; professional scopes, LUT management, and advanced per-channel grading remain future work. Export jobs cannot yet be cancelled or retried from the queue UI.
- Cross-platform CI is a build/test signal, not a substitute for manual packaging, GUI, audio-device, GPU, and export testing on each operating system.

## Release checklist

Before shipping, validate fresh install and upgrade paths on each supported OS; verify FFmpeg/ffprobe/ffplay discovery; open, edit, save, close, and reopen real projects; test missing/corrupt media recovery; compare preview and export frame-by-frame for representative timelines; test audio clipping, mute/solo/gain, fades, seeking, pause/resume and mixed tracks on real audio devices; resize the UI; and run a complete export on each target platform.
