//! Capture: a child process that writes 16 kHz mono s16le to stdout, read in 20 ms frames.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use super::FRAME_BYTES;
use crate::machine::{Capture, Event, SessionId};

type CommandFactory = Box<dyn Fn() -> Command + Send + Sync>;

pub struct ProcessCapture {
    events: Sender<Event>,
    command: CommandFactory,
    /// Pids of running children. The reader thread removes its entry before it reaps the child,
    /// so a pid in here is never reused.
    pids: Arc<Mutex<HashMap<SessionId, u32>>>,
}

impl ProcessCapture {
    /// The microphone through PipeWire. Test hook: with `ELF_MIC_FILE=<file>` set, that file is
    /// decoded in real time (once, no loop) instead, and the recording ends when the file does.
    pub fn microphone(events: Sender<Event>) -> Self {
        if let Some(file) = std::env::var_os("ELF_MIC_FILE") {
            crate::elog!("ELF_MIC_FILE set: capture plays {file:?} instead of the microphone");
            return Self::file(events, Path::new(&file));
        }
        Self::with_command(events, || {
            let mut c = Command::new("pw-record");
            // --raw: without it pw-record wraps stdout in a Sun .snd container with
            // big-endian samples, which read as little-endian PCM is loud noise.
            c.args(["--raw", "--rate", "16000", "--channels", "1", "--format", "s16", "-"]);
            c
        })
    }

    /// A recording played at real-time speed, for `elfd --simulate`.
    pub fn file(events: Sender<Event>, path: &Path) -> Self {
        let path = path.to_path_buf();
        Self::with_command(events, move || {
            let mut c = Command::new("ffmpeg");
            c.args(["-loglevel", "error", "-re", "-i"]).arg(&path).args(["-f", "s16le", "-ar", "16000", "-ac", "1", "-"]);
            c
        })
    }

    fn with_command(events: Sender<Event>, make: impl Fn() -> Command + Send + Sync + 'static) -> Self {
        Self { events, command: Box::new(make), pids: Arc::default() }
    }
}

impl Capture for ProcessCapture {
    fn start(&self, id: SessionId) -> Result<(), String> {
        let mut child = (self.command)()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start the recorder: {e}"))?;
        let stdout = child.stdout.take().expect("piped stdout");
        self.pids.lock().unwrap().insert(id, child.id());
        let (events, pids) = (self.events.clone(), self.pids.clone());
        std::thread::spawn(move || read_frames(id, child, stdout, &events, &pids));
        Ok(())
    }

    fn stop(&self, id: SessionId) {
        if let Some(pid) = self.pids.lock().unwrap().get(&id) {
            // SAFETY: the pid belongs to our unreaped child (see `pids`).
            unsafe { libc::kill(*pid as libc::pid_t, libc::SIGTERM) };
        }
    }
}

fn read_frames(
    id: SessionId,
    mut child: Child,
    mut stdout: impl Read,
    events: &Sender<Event>,
    pids: &Mutex<HashMap<SessionId, u32>>,
) {
    let mut frame = vec![0u8; FRAME_BYTES];
    while stdout.read_exact(&mut frame).is_ok() {
        if events.send(Event::Frame(id, frame.clone())).is_err() {
            break;
        }
    }
    pids.lock().unwrap().remove(&id);
    let _ = child.kill();
    let status = child.wait();
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    let failed = status.is_ok_and(|s| !s.success()) && !stderr.trim().is_empty();
    let reason = failed.then(|| stderr.trim().lines().next().unwrap_or_default().chars().take(120).collect());
    let _ = events.send(Event::CaptureEnded(id, reason));
}
