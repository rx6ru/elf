//! The HUD seam: `Hud` is what the state machine drives. `OverlayHud` shows the overlay, `LogHud` only logs.

use std::sync::{Arc, OnceLock};

use crate::vad::Voice;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Hidden,
    Listening,
    /// Stopped; waiting for text.
    Processing,
    Done,
    /// Recording had no voice, or was too short.
    NothingHeard,
    /// Error, or audio kept for `elf retry`.
    Failed,
}

pub trait Hud: Send {
    fn phase(&self, phase: Phase);
    /// The latest 20 ms of voice analysis, while listening.
    fn voice(&self, voice: &Voice);
}

/// Writes phase changes to the log and ignores voice.
pub struct LogHud;

impl Hud for LogHud {
    fn phase(&self, phase: Phase) {
        crate::elog!("hud {phase:?}");
    }

    fn voice(&self, _voice: &Voice) {}
}

/// Where the overlay's handle appears once the event loop on the main thread has built it.
pub type HudSlot = Arc<OnceLock<elf_hud::Handle>>;

/// Drives the Rune overlay from the state machine thread. Until the overlay exists (and
/// forever when there is no display) every call does nothing.
pub struct OverlayHud(pub HudSlot);

impl Hud for OverlayHud {
    fn phase(&self, phase: Phase) {
        let Some(hud) = self.0.get() else { return };
        match phase {
            Phase::Hidden => hud.hide(),
            Phase::Listening => hud.show(elf_hud::Phase::Listening),
            Phase::Processing => hud.show(elf_hud::Phase::Processing),
            // "Nothing heard" is not a failure: the shape just draws in and fades.
            Phase::Done | Phase::NothingHeard => hud.show(elf_hud::Phase::Done),
            Phase::Failed => hud.show(elf_hud::Phase::Error),
        }
    }

    fn voice(&self, voice: &Voice) {
        if let Some(hud) = self.0.get() {
            hud.voice(to_overlay(voice));
        }
    }
}

fn to_overlay(v: &Voice) -> elf_hud::Voice {
    elf_hud::Voice {
        active: v.active,
        activity: v.activity,
        level: v.level,
        bands: v.bands,
        onsets: v.onsets,
        onset_strength: v.onset_strength,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_fields_carry_over() {
        let v = Voice { active: true, activity: 0.5, level: 0.7, bands: [0.1; 8], onsets: 3, onset_strength: 0.9 };
        let o = to_overlay(&v);
        assert!(o.active && o.activity == 0.5 && o.level == 0.7 && o.bands == [0.1; 8]);
        assert_eq!((o.onsets, o.onset_strength), (3, 0.9));
    }

    #[test]
    fn without_an_overlay_calls_do_nothing() {
        let hud = OverlayHud(HudSlot::default());
        hud.phase(Phase::Listening);
        hud.voice(&Voice::default());
    }
}
