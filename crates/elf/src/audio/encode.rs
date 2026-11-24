//! Ogg/Opus encoding while recording, through an `ffmpeg` child process.
//! 32 kbps Opus is about a tenth of the WAV size and Deepgram reads it as well as WAV.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use crate::machine::{Encoder, SessionId};

/// ffmpeg ends within milliseconds of its input closing; this only guards against a hang.
const FINISH_WAIT: Duration = Duration::from_secs(2);

struct Job {
    audio: Sender<Vec<u8>>,
    result: Receiver<Option<Vec<u8>>>,
}

#[derive(Default)]
pub struct OpusEncoder {
    jobs: Mutex<HashMap<SessionId, Job>>,
}

impl Encoder for OpusEncoder {
    fn start(&self, id: SessionId) {
        let child = Command::new("ffmpeg")
            .args(["-loglevel", "quiet", "-f", "s16le", "-ar", "16000", "-ac", "1", "-i", "-"])
            .args(["-c:a", "libopus", "-b:a", "32k", "-application", "voip", "-f", "ogg", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                crate::elog!("{id} opus encoder unavailable: {e}");
                return;
            }
        };
        let mut stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = child.stdout.take().expect("piped stdout");
        let (audio, audio_rx) = mpsc::channel::<Vec<u8>>();
        let (result_tx, result) = mpsc::channel();
        // Writing runs on its own thread so a stuck ffmpeg can never block the state machine.
        std::thread::spawn(move || {
            for chunk in audio_rx {
                if stdin.write_all(&chunk).is_err() {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let read = stdout.read_to_end(&mut out);
            let ok = child.wait().is_ok_and(|s| s.success());
            let _ = result_tx.send((read.is_ok() && ok && !out.is_empty()).then_some(out));
        });
        self.jobs.lock().unwrap().insert(id, Job { audio, result });
    }

    fn push(&self, id: SessionId, pcm: Vec<u8>) {
        if let Some(job) = self.jobs.lock().unwrap().get(&id) {
            let _ = job.audio.send(pcm);
        }
    }

    fn finish(&self, id: SessionId) -> Option<Vec<u8>> {
        let Job { audio, result } = self.jobs.lock().unwrap().remove(&id)?;
        drop(audio);
        result.recv_timeout(FINISH_WAIT).ok().flatten()
    }

    fn abort(&self, id: SessionId) {
        self.jobs.lock().unwrap().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_one_second_of_audio_to_an_ogg_file() {
        let e = OpusEncoder::default();
        e.start(1);
        for i in 0..50 {
            let frame: Vec<u8> = (0..320)
                .map(|n| (((n + i * 320) as f32 * 0.12).sin() * 8000.0) as i16)
                .flat_map(i16::to_le_bytes)
                .collect();
            e.push(1, frame);
        }
        let ogg = e.finish(1).expect("ffmpeg with libopus is installed");
        assert_eq!(&ogg[0..4], b"OggS");
        assert!(ogg.len() < 8_000, "{} bytes", ogg.len());
    }

    #[test]
    fn finish_without_start_is_none() {
        assert_eq!(OpusEncoder::default().finish(9), None);
    }
}
