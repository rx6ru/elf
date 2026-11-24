//! Voice analysis and voice-activity detection. The analyser is the one from the HUD work
//! (`elf-hud/src/mic.rs`): speech against silence and steady noise, loudness with auto-gain,
//! 8 spectrum bands and syllable onsets. `Vad` is the seam where another detector can replace it.

use std::f32::consts::PI;

use crate::audio::RATE;

/// 20 ms frames: 320 samples, 50 Hz per DFT bin.
const N: usize = (RATE as usize) / 50;
/// Speech-like frames in a row to switch on (80 ms), and frames to stay on afterwards (360 ms).
const ATTACK_FRAMES: u32 = 4;
const HANGOVER_FRAMES: u32 = 18;
/// A frame must sit this far above the noise floor, in dB, to count as speech.
const GATE_DB: f32 = 7.0;
/// Share of the energy that must lie in the speech band (about 250 Hz to 4.5 kHz).
const SPEECH_SHARE: f32 = 0.62;
/// Over the last half second, a loudness that moves less than this (dB, std. deviation) is a
/// steady sound such as a fan and cannot switch voice on.
const STEADY_DB: f32 = 1.2;
/// While the floor is still being learned (first second), it may not rise above this. A
/// dictation that starts at once would otherwise teach the floor its own voice and be ignored.
const LEARNING_FLOOR_CAP_DB: f32 = -45.0;
const LEARNING_FRAMES: u32 = 50;
/// Band edges in DFT bins (x50 Hz): 100, 200, 400, 700, 1100, 1800, 2800, 4500, 8000 Hz.
const EDGES: [usize; 9] = [2, 4, 8, 14, 22, 36, 56, 90, 160];
pub const BANDS: usize = 8;

const FRAME_MS: u32 = 20;

#[derive(Clone, Copy, Default, Debug)]
pub struct Voice {
    /// Speech is going on: speech-like frames for ~80 ms switch it on, and it stays on for
    /// ~360 ms after the last one. Silence, hum, hiss and clicks never switch it on.
    pub active: bool,
    /// `active`, eased: rises in ~150 ms, falls in ~500 ms. 0..1.
    pub activity: f32,
    /// 0 at the room's noise floor, 1 at the recent speaking peak.
    pub level: f32,
    /// Spectrum shape x level.
    pub bands: [f32; BANDS],
    /// Counts up by one on each syllable onset.
    pub onsets: u32,
    /// Strength of the latest onset, 0..1.
    pub onset_strength: f32,
}

pub trait Vad: Send {
    /// Feeds one 20 ms frame of s16le audio.
    fn push(&mut self, pcm: &[u8]) -> Voice;
    /// Total time with voice so far.
    fn voiced_ms(&self) -> u32;
}

pub fn new_level_vad() -> Box<dyn Vad> {
    Box::new(AnalyserVad { analyser: Analyser::new(), active_frames: 0 })
}

/// Voice heard = the analyser's voice activity was on. Switching it on takes 80 ms of speech.
struct AnalyserVad {
    analyser: Analyser,
    active_frames: u32,
}

impl Vad for AnalyserVad {
    fn push(&mut self, pcm: &[u8]) -> Voice {
        let v = self.analyser.push(pcm);
        if v.active {
            self.active_frames += 1;
        }
        v
    }

    fn voiced_ms(&self) -> u32 {
        self.active_frames * FRAME_MS
    }
}

struct Analyser {
    window: Vec<f32>,
    cos: Vec<f32>,
    sin: Vec<f32>,
    floor: f32,
    peak: f32,
    prev_log: [f32; BANDS],
    flux_mean: f32,
    since_onset: u32,
    frames: u32,
    voice: Voice,
    recent_db: [f32; 25],
    streak: u32,
    since_speech: u32,
}

impl Analyser {
    fn new() -> Self {
        let window = (0..N).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / N as f32).cos()).collect();
        let angle = |i: usize| 2.0 * PI * i as f32 / N as f32;
        Self {
            window,
            cos: (0..N).map(|i| angle(i).cos()).collect(),
            sin: (0..N).map(|i| angle(i).sin()).collect(),
            floor: -50.0,
            peak: -20.0,
            prev_log: [0.0; BANDS],
            flux_mean: 0.5,
            since_onset: 0,
            frames: 0,
            voice: Voice::default(),
            recent_db: [-100.0; 25],
            streak: 0,
            since_speech: u32::MAX,
        }
    }

    fn push(&mut self, pcm: &[u8]) -> Voice {
        let x: Vec<f32> = pcm
            .chunks_exact(2)
            .zip(&self.window)
            .map(|(s, w)| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0 * w)
            .collect();

        // Loudness with auto-gain: the floor follows quiet fast and loud slowly (≈1 dB/s);
        // the peak jumps up at once and decays ≈7 dB/s, but stays ≥ 18 dB over the floor.
        let rms = (x.iter().map(|v| v * v).sum::<f32>() / N as f32).sqrt();
        let db = 20.0 * rms.max(1e-7).log10();
        // The first second learns the room both ways; after that the floor rises slowly (2.5 dB/s),
        // and not at all while someone speaks.
        self.frames += 1;
        self.floor = if self.frames <= LEARNING_FRAMES {
            let learned = if self.frames == 1 { db } else { self.floor + (db - self.floor) * 0.15 };
            learned.min(LEARNING_FLOOR_CAP_DB)
        } else if db < self.floor {
            self.floor + (db - self.floor) * 0.3
        } else if self.voice.active {
            self.floor
        } else {
            self.floor + 0.05
        };
        // Band energies from a direct DFT (160 bins × 320 samples, cheap at 50 frames/s).
        let mut energy = [0.0f32; BANDS];
        for (b, e) in energy.iter_mut().enumerate() {
            for k in EDGES[b]..EDGES[b + 1] {
                let (mut re, mut im) = (0.0f32, 0.0f32);
                for (n, v) in x.iter().enumerate() {
                    let idx = (k * n) % N;
                    re += v * self.cos[idx];
                    im -= v * self.sin[idx];
                }
                *e += re * re + im * im;
            }
            *e /= (EDGES[b + 1] - EDGES[b]) as f32;
        }
        // Voice activity. Where the energy lies: 250 Hz to 4.5 kHz is speech (half of the
        // 200-400 Hz band counts); the rest is hum below it or hiss above it.
        let power: Vec<f32> = energy.iter().enumerate().map(|(b, e)| e * (EDGES[b + 1] - EDGES[b]) as f32).collect();
        let sub = {
            let (mut re, mut im) = (0.0f32, 0.0f32);
            for (n, v) in x.iter().enumerate() {
                re += v * self.cos[n];
                im -= v * self.sin[n];
            }
            re * re + im * im
        };
        let total = power.iter().sum::<f32>() + sub + 1e-12;
        let speech = 0.5 * power[1] + power[2..7].iter().sum::<f32>();
        self.recent_db.rotate_left(1);
        self.recent_db[24] = db;
        let mean = self.recent_db.iter().sum::<f32>() / 25.0;
        let spread = (self.recent_db.iter().map(|d| (d - mean).powi(2)).sum::<f32>() / 25.0).sqrt();
        let steady = self.frames > 25 && spread < STEADY_DB;
        let speechlike = db > self.floor + GATE_DB && speech / total > SPEECH_SHARE;
        // Switching on needs a moving sound; staying on does not, so a held vowel is kept.
        let counts = speechlike && (self.voice.active || !steady);
        self.streak = if counts { self.streak + 1 } else { 0 };
        self.since_speech = if counts { 0 } else { self.since_speech.saturating_add(1) };
        self.voice.active = if self.voice.active {
            self.since_speech < HANGOVER_FRAMES
        } else {
            self.streak >= ATTACK_FRAMES
        };
        let target = if self.voice.active { 1.0 } else { 0.0 };
        let rate = if target > self.voice.activity { 0.13 } else { 0.04 };
        self.voice.activity += (target - self.voice.activity) * rate;

        // The peak only learns from speech-like sound, so a slammed door does not shrink your voice.
        let gate = self.floor + 6.0;
        let heard = self.voice.active || speechlike;
        self.peak = (if heard { db } else { f32::MIN }).max(self.peak - 0.14).max(self.floor + 18.0);
        let level = ((db - gate) / (self.peak - gate)).clamp(0.0, 1.0);

        let max = energy.iter().cloned().fold(1e-12, f32::max);
        let mut bands = [0.0f32; BANDS];
        for (b, e) in energy.iter().enumerate() {
            bands[b] = (e / max).sqrt() * level;
        }

        // Onsets: positive spectral flux well above its running mean, at most one per 100 ms.
        let mut flux = 0.0f32;
        for (b, e) in energy.iter().enumerate() {
            let l = (e + 1e-10).ln();
            flux += (l - self.prev_log[b]).max(0.0);
            self.prev_log[b] = l;
        }
        self.since_onset += 1;
        let onset = level > 0.12 && self.since_onset >= 5 && flux > self.flux_mean * 2.0 + 1.5;
        self.flux_mean += (flux - self.flux_mean) * 0.05;
        if onset {
            self.since_onset = 0;
            self.voice.onsets = self.voice.onsets.wrapping_add(1);
            self.voice.onset_strength = (flux / (self.flux_mean * 4.0 + 3.0)).clamp(0.3, 1.0);
        }
        self.voice.level = level;
        self.voice.bands = bands;
        self.voice
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, amp: f32) -> Vec<u8> {
        (0..N).map(|i| ((amp * (2.0 * PI * hz * i as f32 / RATE as f32).sin()) * 32767.0) as i16).flat_map(i16::to_le_bytes).collect()
    }

    fn voice_like(amp: f32) -> Vec<u8> {
        (0..N).map(|i| {
            let t = i as f32 / RATE as f32;
            let sum: f32 = [300.0, 650.0, 1000.0, 1500.0, 2100.0].iter().map(|f| (2.0 * PI * f * t).sin()).sum();
            ((amp * sum / 5.0) * 32767.0) as i16
        }).flat_map(i16::to_le_bytes).collect()
    }

    fn noise(amp: f32, seed: &mut u32) -> Vec<u8> {
        (0..N).map(|_| {
            *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            ((*seed >> 16) as f32 / 32768.0 - 1.0) * amp
        }).map(|v| (v * 32767.0) as i16).flat_map(i16::to_le_bytes).collect()
    }

    /// An analyser that has learned a quiet room.
    fn in_quiet_room() -> Analyser {
        let mut a = Analyser::new();
        let mut seed = 1;
        for _ in 0..80 { a.push(&noise(0.001, &mut seed)); }
        a
    }

    #[test]
    fn silence_is_never_active() {
        let mut a = in_quiet_room();
        assert!((0..100).all(|_| !a.push(&vec![0u8; N * 2]).active));
    }

    #[test]
    fn a_steady_hum_is_never_active() {
        let mut a = in_quiet_room();
        assert!((0..150).all(|_| !a.push(&tone(100.0, 0.3)).active));
        assert!((0..150).all(|_| !a.push(&tone(50.0, 0.3)).active));
    }

    #[test]
    fn loud_steady_noise_and_hiss_are_never_active() {
        let mut a = in_quiet_room();
        let mut seed = 7;
        assert!((0..150).all(|_| !a.push(&noise(0.4, &mut seed)).active));
        assert!((0..150).all(|_| !a.push(&tone(6000.0, 0.3)).active));
    }

    #[test]
    fn a_short_click_is_never_active() {
        let mut a = in_quiet_room();
        let mut seed = 3;
        for _ in 0..5 {
            assert!(!a.push(&noise(0.9, &mut seed)).active);
            assert!(!a.push(&noise(0.9, &mut seed)).active);
            for _ in 0..30 { assert!(!a.push(&vec![0u8; N * 2]).active); }
        }
    }

    #[test]
    fn speech_like_bursts_switch_on_and_the_hangover_bridges_a_gap() {
        let mut a = in_quiet_room();
        let on_after = (1..=10).find(|_| a.push(&voice_like(0.2)).active).expect("switches on");
        assert!((4..=5).contains(&on_after), "switches on after {on_after} frames");
        for _ in 0..30 { a.push(&voice_like(0.2)); }
        // A 150 ms pause between words does not drop it.
        assert!((0..7).all(|_| a.push(&vec![0u8; N * 2]).active));
        assert!(a.push(&voice_like(0.2)).active);
        // Real silence does, after about 360 ms.
        let off_after = (1..=60).find(|_| !a.push(&vec![0u8; N * 2]).active).expect("switches off");
        assert!((15..=20).contains(&off_after), "switches off after {off_after} frames");
    }

    #[test]
    fn activity_eases_up_and_down() {
        let mut a = in_quiet_room();
        let first = a.push(&voice_like(0.2)).activity;
        for _ in 0..40 { a.push(&voice_like(0.2)); }
        let loud = a.push(&voice_like(0.2)).activity;
        assert!(first < 0.2 && loud > 0.95, "{first} {loud}");
        for _ in 0..30 { a.push(&vec![0u8; N * 2]); }
        let mid = a.push(&vec![0u8; N * 2]).activity;
        assert!(mid > 0.05 && mid < 0.9, "decays, does not snap: {mid}");
    }

    #[test]
    fn silence_stays_at_zero() {
        let mut a = Analyser::new();
        for _ in 0..100 { a.push(&vec![0u8; N * 2]); }
        assert_eq!(a.push(&vec![0u8; N * 2]).level, 0.0);
    }

    #[test]
    fn a_low_tone_lights_a_low_band_and_a_high_tone_a_high_band() {
        let mut a = Analyser::new();
        for _ in 0..50 { a.push(&tone(300.0, 0.001)); }
        let low = a.push(&tone(300.0, 0.3));
        assert!(low.level > 0.5, "speech-loud tone over a quiet floor: {low:?}");
        // 300 Hz sits in band 1 (200–400 Hz); 5 kHz in band 7 (4.5–8 kHz).
        assert!(low.bands[1] > 0.9 * low.level && low.bands[7] < 0.2, "{low:?}");
        let high = a.push(&tone(5000.0, 0.3));
        assert!(high.bands[7] > 0.9 * high.level && high.bands[1] < 0.2, "{high:?}");
    }

    #[test]
    fn a_burst_after_quiet_is_an_onset() {
        let mut a = Analyser::new();
        for _ in 0..50 { a.push(&tone(300.0, 0.001)); }
        let before = a.push(&tone(300.0, 0.001)).onsets;
        let after = a.push(&tone(300.0, 0.4)).onsets;
        assert_eq!(after, before + 1);
    }

    #[test]
    fn voiced_ms_counts_speech_but_not_hum_or_noise() {
        let mut v = new_level_vad();
        let mut seed = 1;
        for _ in 0..80 { v.push(&noise(0.001, &mut seed)); }
        for _ in 0..150 { v.push(&tone(100.0, 0.3)); }
        for _ in 0..150 { v.push(&noise(0.4, &mut seed)); }
        assert_eq!(v.voiced_ms(), 0);
        for _ in 0..20 { v.push(&voice_like(0.2)); }
        assert!(v.voiced_ms() >= 150, "{}", v.voiced_ms());
    }

    #[test]
    fn speech_from_the_very_first_frame_is_still_heard() {
        let mut v = new_level_vad();
        for _ in 0..60 { v.push(&voice_like(0.2)); }
        assert!(v.voiced_ms() >= 150, "{}", v.voiced_ms());
    }

    #[test]
    fn speech_after_a_pause_in_the_first_second_is_heard() {
        let mut v = new_level_vad();
        for _ in 0..5 { v.push(&voice_like(0.2)); }
        for _ in 0..10 { v.push(&vec![0u8; N * 2]); }
        for _ in 0..40 { v.push(&voice_like(0.2)); }
        assert!(v.voiced_ms() >= 300, "{}", v.voiced_ms());
    }
}
