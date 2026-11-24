//! The desktop around the daemon: focused window, Esc submap, notifications, sounds.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::hypr::{self, Window};
use crate::machine::Desktop;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    Start,
    Stop,
}

pub struct RealDesktop;

impl Desktop for RealDesktop {
    fn active_window(&self) -> Option<Window> {
        hypr::active_window()
    }

    fn set_submap(&self, on: bool) {
        hypr::set_submap(on);
    }

    fn notify(&self, message: &str) {
        let message = message.to_string();
        std::thread::spawn(move || {
            let _ = Command::new("notify-send")
                .args(["--app-name=elf", "--expire-time=4000", "ELF", &message])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
    }

    fn sound(&self, sound: Sound) {
        let pcm = beep(if sound == Sound::Start { 880.0 } else { 660.0 }, 70);
        std::thread::spawn(move || {
            let child = Command::new("pw-play")
                .args(["--raw", "--rate", "16000", "--channels", "1", "--format", "s16", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(mut child) = child {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(&pcm);
                }
                let _ = child.wait();
            }
        });
    }
}

/// A short sine beep as 16 kHz s16le, with a fade in and out so it does not click.
fn beep(hz: f32, ms: usize) -> Vec<u8> {
    let n = 16 * ms;
    (0..n)
        .flat_map(|i| {
            let t = i as f32 / 16_000.0;
            let fade = (i.min(n - i) as f32 / 160.0).min(1.0);
            let s = (2.0 * std::f32::consts::PI * hz * t).sin() * fade * 0.25;
            ((s * 32767.0) as i16).to_le_bytes()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beep_length_and_fade() {
        let b = beep(880.0, 70);
        assert_eq!(b.len(), 16 * 70 * 2);
        assert_eq!(&b[0..2], &[0, 0]);
    }
}
