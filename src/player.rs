use mpv_engine::{Engine, ExportOptions, ExportedFrame};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub struct VideoPlayer {
    pub engine: Arc<Engine>,
    dirty: Arc<AtomicBool>,
}

impl VideoPlayer {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let engine = Arc::new(Engine::video().build()?);
        // TimelineAudioPlayback is the sole audible output; mute MPV before a
        // file can begin playback to prevent a brief un-mixed audio leak.
        let _ = engine.set_property("volume", 0.0_f64);
        let dirty = Arc::new(AtomicBool::new(true));

        let signal = Arc::clone(&dirty);
        engine.attach_exported_render(ExportOptions::new(1280, 720), move || {
            signal.store(true, Ordering::Release);
        })?;

        Ok(Self { engine, dirty })
    }

    pub fn load(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.engine.load_when_ready(path)?;
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    pub fn toggle_playback(&self) -> Result<bool, Box<dyn std::error::Error>> {
        let paused = !self.engine.is_paused();
        self.engine.set_paused(paused)?;
        Ok(!paused)
    }

    pub fn seek(&self, seconds: f64) -> Result<(), Box<dyn std::error::Error>> {
        self.engine.seek_relative(seconds)?;
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    pub fn seek_absolute(&self, seconds: f64) -> Result<(), Box<dyn std::error::Error>> {
        self.engine.seek_absolute(seconds.max(0.0))?;
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    pub fn take_frame(&self) -> Result<Option<ExportedFrame>, Box<dyn std::error::Error>> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(None);
        }
        Ok(self.engine.acquire_frame()?)
    }
}
