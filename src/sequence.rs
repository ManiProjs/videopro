//! Timeline sequence graph used to resolve nested timelines before rendering.
//!
//! The editor's current timeline remains a flat list for compatibility. This
//! model gives save/load and UI work a validated graph contract to build on.
#![allow(dead_code)] // Nested sequence and multicamera editing are being staged behind this model.

use std::collections::{HashMap, HashSet};

use crate::render::RenderClip;

const MAX_NESTING_DEPTH: usize = 16;
const MAX_FLATTENED_CLIPS: usize = 10_000;

#[derive(Clone, Debug)]
pub(crate) struct Sequence {
    pub id: String,
    pub items: Vec<SequenceItem>,
}

#[derive(Clone, Debug)]
pub(crate) enum SequenceItem {
    Clip(RenderClip),
    Nested { sequence_id: String, start: f32 },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AngleCut {
    pub start: f32,
    pub end: f32,
    pub angle_sequence_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct MulticamEdit {
    pub id: String,
    pub angles: Vec<String>,
    pub cuts: Vec<AngleCut>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SequenceError {
    MissingSequence(String),
    Cycle(String),
    InvalidPlacement(String),
    InvalidMulticam(String),
    TooDeep,
    TooManyClips,
}

impl std::fmt::Display for SequenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSequence(id) => write!(f, "sequence '{id}' does not exist"),
            Self::Cycle(id) => write!(f, "nested sequence cycle reaches '{id}'"),
            Self::InvalidPlacement(id) => write!(f, "invalid nested sequence placement '{id}'"),
            Self::InvalidMulticam(id) => write!(f, "invalid multicamera edit '{id}'"),
            Self::TooDeep => write!(f, "nested sequences exceed the depth limit"),
            Self::TooManyClips => write!(f, "flattened sequence exceeds the clip limit"),
        }
    }
}

impl std::error::Error for SequenceError {}

/// Resolve nested timelines to the existing renderer contract, accumulating
/// placement offsets and rejecting cycles before they can recurse indefinitely.
pub(crate) fn flatten(
    root_id: &str,
    sequences: &HashMap<String, Sequence>,
) -> Result<Vec<RenderClip>, SequenceError> {
    let mut result = Vec::new();
    let mut active = HashSet::new();
    flatten_into(root_id, 0.0, sequences, &mut active, 0, &mut result)?;
    result.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.track.cmp(&b.track)));
    Ok(result)
}

fn flatten_into(
    id: &str,
    offset: f32,
    sequences: &HashMap<String, Sequence>,
    active: &mut HashSet<String>,
    depth: usize,
    result: &mut Vec<RenderClip>,
) -> Result<(), SequenceError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(SequenceError::TooDeep);
    }
    let sequence = sequences
        .get(id)
        .ok_or_else(|| SequenceError::MissingSequence(id.to_owned()))?;
    if !active.insert(id.to_owned()) {
        return Err(SequenceError::Cycle(id.to_owned()));
    }
    for item in &sequence.items {
        match item {
            SequenceItem::Clip(clip) => {
                if !clip.start.is_finite()
                    || clip.start < 0.0
                    || !clip.duration.is_finite()
                    || clip.duration <= 0.0
                {
                    active.remove(id);
                    return Err(SequenceError::InvalidPlacement(sequence.id.clone()));
                }
                if result.len() >= MAX_FLATTENED_CLIPS {
                    active.remove(id);
                    return Err(SequenceError::TooManyClips);
                }
                let mut clip = clip.clone();
                clip.start += offset;
                result.push(clip);
            }
            SequenceItem::Nested { sequence_id, start } => {
                if !start.is_finite() || *start < 0.0 {
                    active.remove(id);
                    return Err(SequenceError::InvalidPlacement(sequence_id.clone()));
                }
                flatten_into(
                    sequence_id,
                    offset + start,
                    sequences,
                    active,
                    depth + 1,
                    result,
                )?;
            }
        }
    }
    active.remove(id);
    Ok(())
}

impl MulticamEdit {
    /// Validate ordered, non-overlapping angle cuts against the available angles.
    pub(crate) fn validate(&self) -> Result<(), SequenceError> {
        if self.id.trim().is_empty() || self.angles.is_empty() {
            return Err(SequenceError::InvalidMulticam(self.id.clone()));
        }
        let angles: HashSet<&str> = self.angles.iter().map(String::as_str).collect();
        let mut previous_end = 0.0;
        for cut in &self.cuts {
            if !cut.start.is_finite()
                || !cut.end.is_finite()
                || cut.start < previous_end
                || cut.end <= cut.start
                || !angles.contains(cut.angle_sequence_id.as_str())
            {
                return Err(SequenceError::InvalidMulticam(self.id.clone()));
            }
            previous_end = cut.end;
        }
        Ok(())
    }

    /// Return the active angle at a timeline time; gaps have no selected angle.
    pub(crate) fn angle_at(&self, time: f32) -> Option<&str> {
        self.cuts
            .iter()
            .find(|cut| time >= cut.start && time < cut.end)
            .map(|cut| cut.angle_sequence_id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_clip(start: f32) -> RenderClip {
        RenderClip {
            path: "media.mp4".into(),
            title_text: String::new(),
            duration: 2.0,
            start,
            source_in: 0.0,
            source_out: 2.0,
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
            scale: 1.0,
            rotation: 0.0,
            animate_scale: false,
            scale_end: 1.0,
        }
    }

    #[test]
    fn nested_sequence_offsets_flatten_into_render_clips() {
        let sequences = HashMap::from([
            (
                "root".to_owned(),
                Sequence {
                    id: "root".to_owned(),
                    items: vec![SequenceItem::Nested {
                        sequence_id: "scene".to_owned(),
                        start: 10.0,
                    }],
                },
            ),
            (
                "scene".to_owned(),
                Sequence {
                    id: "scene".to_owned(),
                    items: vec![SequenceItem::Clip(sample_clip(1.5))],
                },
            ),
        ]);
        let clips = flatten("root", &sequences).unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].start, 11.5);
    }

    #[test]
    fn nested_sequence_cycles_are_rejected() {
        let sequences = HashMap::from([
            (
                "a".to_owned(),
                Sequence {
                    id: "a".to_owned(),
                    items: vec![SequenceItem::Nested {
                        sequence_id: "b".to_owned(),
                        start: 0.0,
                    }],
                },
            ),
            (
                "b".to_owned(),
                Sequence {
                    id: "b".to_owned(),
                    items: vec![SequenceItem::Nested {
                        sequence_id: "a".to_owned(),
                        start: 0.0,
                    }],
                },
            ),
        ]);
        assert!(matches!(
            flatten("a", &sequences),
            Err(SequenceError::Cycle(id)) if id == "a"
        ));
    }

    #[test]
    fn multicam_cuts_resolve_boundaries_and_validate_angles() {
        let edit = MulticamEdit {
            id: "interview".to_owned(),
            angles: vec!["wide".to_owned(), "close".to_owned()],
            cuts: vec![
                AngleCut {
                    start: 0.0,
                    end: 3.0,
                    angle_sequence_id: "wide".to_owned(),
                },
                AngleCut {
                    start: 3.0,
                    end: 5.0,
                    angle_sequence_id: "close".to_owned(),
                },
            ],
        };
        edit.validate().unwrap();
        assert_eq!(edit.angle_at(2.999), Some("wide"));
        assert_eq!(edit.angle_at(3.0), Some("close"));
        assert_eq!(edit.angle_at(5.0), None);
    }
}
