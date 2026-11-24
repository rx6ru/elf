//! The state machine. One thread owns every dictation; capture, stream, batch, delivery and
//! the control socket only send it `Event`s. It talks to the outside through the port traits.
//!
//! A dictation (`Session`) goes Listening -> Finishing -> Delivering. Only one is Listening at a
//! time, but older ones may still be Finishing or Delivering (scenario 5).
//!
//! At stop, pass 2 starts at once and the live stream flushes in parallel. Pass 2 text is
//! delivered as soon as it arrives; pass 1 only matters when pass 2 is empty or failed.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::audio::{self, wav};
use crate::config::{ApiKey, Config, PasteInto};
use crate::control::Command;
use crate::deliver::{DeliverRequest, Outcome, Reason};
use crate::desktop::Sound;
use crate::history::{Entry, Store};
use crate::keys::{KeyPool, Trouble};
use crate::hud::{Hud, Phase};
use crate::hypr::Window;
use crate::stt::batch::{BatchError, is_retryable, timeout_for};
use crate::stt::stream::StreamEvent;
use crate::stt::{Failure, Params};
use crate::text;
use crate::vad::Vad;
use crate::{clock, elog};

/// The unix time in ms when the dictation began. Also the spool file name.
pub type SessionId = u64;

pub enum Event {
    Command(Command, Sender<String>),
    Frame(SessionId, Vec<u8>),
    /// The recorder ended. `Some` carries its error message.
    CaptureEnded(SessionId, Option<String>),
    Stream(SessionId, StreamEvent),
    Batch(SessionId, Result<String, BatchError>),
    /// `None` for `paste-again`, which has no session.
    Delivered(Option<SessionId>, Outcome),
}

pub trait Capture: Send {
    fn start(&self, id: SessionId) -> Result<(), String>;
    fn stop(&self, id: SessionId);
}

pub trait Stream: Send {
    /// Connects and sends `replay` first. Frames follow through `audio`.
    fn open(&self, id: SessionId, key: &ApiKey, params: &Params, replay: Vec<u8>);
    fn audio(&self, id: SessionId, pcm: Vec<u8>);
    /// Flush the last words and close.
    fn finish(&self, id: SessionId);
    fn abort(&self, id: SessionId);
}

/// The audio pass 2 uploads.
#[derive(Clone)]
pub struct Upload {
    pub bytes: Arc<Vec<u8>>,
    pub content_type: &'static str,
}

pub trait Batch: Send {
    fn submit(&self, id: SessionId, key: &ApiKey, upload: &Upload, params: &Params, timeout: Duration);
    /// Opens the HTTPS connection ahead of time so pass 2 does not pay for the handshake.
    fn warm_up(&self);
}

/// Compresses the recording while it is made, so pass 2 uploads a small file at once.
pub trait Encoder: Send {
    fn start(&self, id: SessionId);
    fn push(&self, id: SessionId, pcm: Vec<u8>);
    /// The finished file, or `None` when encoding failed (the caller then uploads the WAV).
    fn finish(&self, id: SessionId) -> Option<Vec<u8>>;
    fn abort(&self, id: SessionId);
}

pub trait Deliver: Send {
    fn deliver(&self, id: Option<SessionId>, req: DeliverRequest);
}

pub trait Desktop: Send {
    fn active_window(&self) -> Option<Window>;
    /// The `elf` submap binds Esc to `elf cancel` while listening.
    fn set_submap(&self, on: bool);
    fn notify(&self, message: &str);
    fn sound(&self, sound: Sound);
}

pub struct Ports {
    pub capture: Box<dyn Capture>,
    pub stream: Box<dyn Stream>,
    pub batch: Box<dyn Batch>,
    pub encoder: Box<dyn Encoder>,
    pub deliver: Box<dyn Deliver>,
    pub desktop: Box<dyn Desktop>,
    pub store: Box<dyn Store>,
    pub hud: Box<dyn Hud>,
    pub new_vad: fn() -> Box<dyn Vad>,
}

/// A press shorter than this is a tap.
const TAP: Duration = Duration::from_millis(250);
const MIN_AUDIO_MS: u64 = 400;
/// How long a stopped stream may take to flush before pass 1 is given up.
const FLUSH_WAIT: Duration = Duration::from_millis(1500);
const STREAM_RETRY_AFTER: Duration = Duration::from_millis(250);
const BATCH_RETRY_AFTER: Duration = Duration::from_millis(500);
/// Deepgram closes an idle HTTPS connection after a few seconds (reuse works at 2 s
/// idle, fails past about 4 s), so the connection is touched this often while recording.
const KEEP_WARM_EVERY: Duration = Duration::from_secs(2);
const HIDE_AFTER_DONE: Duration = Duration::from_millis(450);
const HIDE_AFTER_NOTHING: Duration = Duration::from_millis(1000);
const HIDE_AFTER_FAILED: Duration = Duration::from_millis(1600);
const NO_KEY: &str = "No Deepgram key. Put DEEPGRAM_API_KEY=... in ~/.config/elf/.env and restart elfd";
const PASTE_AGAIN_HINT: &str = "Copied. Paste with Super+Ctrl+M";

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Flow {
    Continue,
    Quit,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum Stage {
    Listening,
    Finishing,
    Delivering,
}

#[derive(Clone, Copy)]
enum Mode {
    Open,
    /// Started by a key press at this time; release decides tap or hold.
    Pressed(Instant),
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum StreamState {
    Up,
    /// Down, and a retry is scheduled.
    Waiting,
    Dead,
    Closed,
}

enum Pass2 {
    Pending,
    Done(Result<String, BatchError>),
}

#[derive(PartialEq, Eq)]
enum Kind {
    Dictation,
    /// An old spooled recording sent again by `elf retry`.
    Retry,
}

/// The live transcript: confirmed segments plus the current guess.
#[derive(Default)]
struct Live {
    finals: Vec<String>,
    interim: String,
    /// A reconnect replays the whole recording, so its first result replaces the old text.
    replaying: bool,
}

impl Live {
    fn update(&mut self, text: &str, is_final: bool) {
        if std::mem::take(&mut self.replaying) {
            self.finals.clear();
        }
        if is_final {
            if !text.trim().is_empty() {
                self.finals.push(text.trim().to_string());
            }
            self.interim.clear();
        } else {
            self.interim = text.trim().to_string();
        }
    }

    fn text(&self) -> String {
        let mut parts: Vec<&str> = self.finals.iter().map(String::as_str).collect();
        if !self.interim.is_empty() {
            parts.push(&self.interim);
        }
        parts.join(" ")
    }
}

struct Session {
    kind: Kind,
    stage: Stage,
    mode: Mode,
    window: Option<Window>,
    class: String,
    params: Params,
    started: Instant,
    stopped: Option<Instant>,
    pcm: Vec<u8>,
    vad: Box<dyn Vad>,
    audio_ms: u64,
    live: Live,
    stream: StreamState,
    stream_retries: u8,
    first_interim_logged: bool,
    upload: Option<Upload>,
    wav_path: Option<PathBuf>,
    pass2: Pass2,
    batch_attempt: u8,
    /// Index of the Deepgram key this dictation uses for its stream, pass 2 and retries. It
    /// changes only when that key is turned away and another one takes over.
    key: usize,
    /// No key is left that can work: pass 2 is not tried, and the user has been told.
    auth_failed: bool,
}

enum Timer {
    MaxLength(SessionId),
    StreamRetry(SessionId),
    FlushWait(SessionId),
    BatchRetry(SessionId),
    KeepWarm(SessionId),
    HideHud(u64),
}

pub struct Machine {
    cfg: Config,
    keys: KeyPool,
    ports: Ports,
    clock_ms: fn() -> u64,
    last_id: u64,
    sessions: BTreeMap<SessionId, Session>,
    /// The session that is recording now.
    active: Option<SessionId>,
    timers: Vec<(Instant, Timer)>,
    /// The HUD follows the newest dictation.
    hud_owner: Option<SessionId>,
    hud_seq: u64,
}

impl Machine {
    pub fn new(cfg: Config, keys: KeyPool, ports: Ports, clock_ms: fn() -> u64) -> Self {
        Self {
            cfg,
            keys,
            ports,
            clock_ms,
            last_id: 0,
            sessions: BTreeMap::new(),
            active: None,
            timers: Vec::new(),
            hud_owner: None,
            hud_seq: 0,
        }
    }

    pub fn handle(&mut self, event: Event, now: Instant) -> Flow {
        match event {
            Event::Command(cmd, reply) => {
                let (text, flow) = self.command(cmd, now);
                let _ = reply.send(text);
                return flow;
            }
            Event::Frame(id, pcm) => self.frame(id, pcm),
            Event::CaptureEnded(id, err) => self.capture_ended(id, err, now),
            Event::Stream(id, ev) => self.stream_event(id, ev, now),
            Event::Batch(id, result) => self.batch_result(id, result, now),
            Event::Delivered(id, outcome) => self.delivered(id, outcome),
        }
        Flow::Continue
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.timers.iter().map(|(at, _)| *at).min()
    }

    pub fn tick(&mut self, now: Instant) {
        while let Some(i) = self.timers.iter().position(|(at, _)| *at <= now) {
            let (_, timer) = self.timers.remove(i);
            self.timer(timer, now);
        }
    }

    /// Leaves the Esc submap and drops a recording in progress. Spooled audio stays for `elf retry`.
    pub fn shutdown(&mut self) {
        if let Some(id) = self.active.take() {
            self.ports.capture.stop(id);
            self.ports.stream.abort(id);
            self.ports.encoder.abort(id);
            self.ports.desktop.set_submap(false);
            self.sessions.remove(&id);
        }
        self.ports.hud.phase(Phase::Hidden);
    }

    // ---- commands ----

    fn command(&mut self, cmd: Command, now: Instant) -> (String, Flow) {
        let reply = match cmd {
            Command::Start => match self.active {
                Some(_) => self.status(),
                None => self.start(now, Mode::Open).map_or_else(|e| format!("err {e}"), |()| self.status()),
            },
            Command::Toggle => match self.active {
                Some(_) => {
                    self.stop(now);
                    self.status()
                }
                None => self.start(now, Mode::Open).map_or_else(|e| format!("err {e}"), |()| self.status()),
            },
            Command::Press => match self.active {
                Some(_) => {
                    self.stop(now);
                    self.status()
                }
                None => self.start(now, Mode::Pressed(now)).map_or_else(|e| format!("err {e}"), |()| self.status()),
            },
            Command::Release => {
                if let Some(id) = self.active {
                    let mode = self.sessions[&id].mode;
                    match mode {
                        Mode::Pressed(at) if now.duration_since(at) >= TAP => self.stop(now),
                        Mode::Pressed(_) => self.sessions.get_mut(&id).expect("active session").mode = Mode::Open,
                        Mode::Open => {}
                    }
                }
                self.status()
            }
            Command::Stop => {
                self.stop(now);
                self.status()
            }
            Command::Cancel => {
                self.cancel();
                self.status()
            }
            Command::PasteAgain => match self.ports.store.last_text() {
                Some(text) => {
                    self.ports.deliver.deliver(None, DeliverRequest { text, window: None });
                    "ok pasting".into()
                }
                None => {
                    self.ports.desktop.notify("Nothing to paste yet");
                    "err nothing to paste".into()
                }
            },
            Command::Retry => self.retry(now),
            Command::History(n) => self.history(n),
            Command::Status => format!("{}\n{}", self.status(), self.keys.summary(now)),
            Command::Quit => return ("ok bye".into(), Flow::Quit),
        };
        (reply, Flow::Continue)
    }

    fn status(&self) -> String {
        if self.active.is_some() {
            "ok listening".into()
        } else if self.sessions.is_empty() {
            "ok idle".into()
        } else {
            format!("ok processing {}", self.sessions.len())
        }
    }

    fn history(&self, n: usize) -> String {
        let entries = self.ports.store.recent(n);
        let mut out = format!("ok {}", entries.len());
        for e in entries {
            let text = match &e.text {
                Some(t) => t.trim().replace('\n', "\\n"),
                None => format!("(no text: {})", e.error.as_deref().unwrap_or("unknown")),
            };
            out.push_str(&format!("\n{} {} {}", e.ts, e.app.as_deref().unwrap_or("-"), text));
        }
        out
    }

    // ---- starting and stopping ----

    fn next_id(&mut self) -> SessionId {
        self.last_id = (self.clock_ms)().max(self.last_id + 1);
        self.last_id
    }

    fn new_session(&mut self, kind: Kind, window: Option<Window>, now: Instant) -> Session {
        let key = self.keys.assign(now);
        let class = window.as_ref().map(|w| w.class.clone()).unwrap_or_default();
        Session {
            kind,
            stage: Stage::Listening,
            mode: Mode::Open,
            params: Params {
                language: self.cfg.language.clone(),
                keyterms: self.cfg.keyterms_for(&class),
                dictation: self.cfg.spoken_punctuation,
                mip_opt_out: self.cfg.mip_opt_out,
            },
            window,
            class,
            started: now,
            stopped: None,
            pcm: Vec::new(),
            vad: (self.ports.new_vad)(),
            audio_ms: 0,
            live: Live::default(),
            stream: StreamState::Dead,
            stream_retries: 1,
            first_interim_logged: false,
            upload: None,
            wav_path: None,
            pass2: Pass2::Pending,
            batch_attempt: 0,
            key,
            auth_failed: false,
        }
    }

    fn start(&mut self, now: Instant, mode: Mode) -> Result<(), String> {
        if self.keys.is_empty() {
            self.ports.desktop.notify(NO_KEY);
            return Err("no deepgram key".into());
        }
        let window = self.ports.desktop.active_window();
        let id = self.next_id();
        self.hud_owner = Some(id);
        if let Err(e) = self.ports.capture.start(id) {
            elog!("{id} capture failed: {e}");
            self.ports.desktop.notify(&format!("Microphone error: {e}"));
            self.set_hud(id, Phase::Failed, now);
            return Err("microphone unavailable".into());
        }
        self.ports.desktop.set_submap(true);
        if self.cfg.sounds {
            self.ports.desktop.sound(Sound::Start);
        }
        self.ports.encoder.start(id);
        self.ports.batch.warm_up();
        let mut s = self.new_session(Kind::Dictation, window, now);
        s.mode = mode;
        // Without the live stream the session stays `Dead` on that side; pass 2 alone gives the text.
        if self.cfg.live_stream {
            s.stream = StreamState::Up;
            self.ports.stream.open(id, self.keys.key(s.key), &s.params, Vec::new());
        }
        elog!("{id} start app={:?} target={:?} {}", s.class, s.window.as_ref().map(|w| &w.address), self.keys.label(s.key));
        self.sessions.insert(id, s);
        self.active = Some(id);
        self.set_hud(id, Phase::Listening, now);
        self.schedule(now + Duration::from_secs(self.cfg.max_seconds), Timer::MaxLength(id));
        self.schedule(now + KEEP_WARM_EVERY, Timer::KeepWarm(id));
        Ok(())
    }

    fn end_listening(&mut self, id: SessionId) {
        self.active = None;
        self.ports.capture.stop(id);
        self.ports.desktop.set_submap(false);
        if self.cfg.sounds {
            self.ports.desktop.sound(Sound::Stop);
        }
    }

    fn cancel(&mut self) {
        let Some(id) = self.active else { return };
        self.end_listening(id);
        self.ports.stream.abort(id);
        self.ports.encoder.abort(id);
        self.sessions.remove(&id);
        self.hud_seq += 1;
        self.ports.hud.phase(Phase::Hidden);
        elog!("{id} cancelled");
    }

    fn stop(&mut self, now: Instant) {
        let Some(id) = self.active else { return };
        self.end_listening(id);
        let s = self.sessions.get_mut(&id).expect("active session");
        s.stage = Stage::Finishing;
        s.stopped = Some(now);
        s.audio_ms = audio::duration_ms(s.pcm.len());
        elog!("{id} stop audio_ms={} voiced_ms={}", s.audio_ms, s.vad.voiced_ms());

        // Only a slip of the key is dropped here. A recording with no voice activity still goes
        // to pass 2: on a noisy microphone the voice detector can miss real speech that
        // Deepgram hears, and dropping it would lose the dictation.
        if s.audio_ms < MIN_AUDIO_MS {
            self.ports.stream.abort(id);
            self.ports.encoder.abort(id);
            self.sessions.remove(&id);
            self.set_hud(id, Phase::NothingHeard, now);
            return;
        }

        match s.stream {
            StreamState::Up => self.ports.stream.finish(id),
            StreamState::Waiting => s.stream = StreamState::Dead,
            StreamState::Dead | StreamState::Closed => {}
        }
        let wav = Arc::new(wav::encode(&std::mem::take(&mut s.pcm)));
        match self.ports.store.save_wav(id, &wav) {
            Ok(path) => s.wav_path = Some(path),
            Err(e) => elog!("{id} spool write failed: {e}"),
        }
        let encoded = self.ports.encoder.finish(id);
        s.upload = Some(match encoded {
            Some(opus) => Upload { bytes: Arc::new(opus), content_type: "audio/ogg" },
            None => Upload { bytes: wav, content_type: "audio/wav" },
        });
        if let Some(u) = &s.upload {
            elog!("{id} upload {} bytes {}", u.bytes.len(), u.content_type);
        }
        if s.auth_failed {
            s.pass2 = Pass2::Done(Err(BatchError::Auth));
        } else {
            self.submit_batch(id);
        }
        self.schedule(now + FLUSH_WAIT, Timer::FlushWait(id));
        self.set_hud(id, Phase::Processing, now);
        self.conclude_if_ready(id, now);
    }

    fn submit_batch(&mut self, id: SessionId) {
        let s = &self.sessions[&id];
        if let Some(upload) = &s.upload {
            self.ports.batch.submit(id, self.keys.key(s.key), upload, &s.params, timeout_for(s.audio_ms));
        }
    }

    fn retry(&mut self, now: Instant) -> String {
        if self.keys.is_empty() {
            self.ports.desktop.notify(NO_KEY);
            return "err no deepgram key".into();
        }
        let mut sent = 0;
        for (id, path) in self.ports.store.spooled() {
            if self.sessions.contains_key(&id) {
                continue;
            }
            let Ok(bytes) = self.ports.store.load_wav(&path) else { continue };
            let mut s = self.new_session(Kind::Retry, None, now);
            s.stage = Stage::Finishing;
            s.stopped = Some(now);
            s.audio_ms = wav::duration_ms(bytes.len());
            s.upload = Some(Upload { bytes: Arc::new(bytes), content_type: "audio/wav" });
            s.wav_path = Some(path);
            self.sessions.insert(id, s);
            self.submit_batch(id);
            elog!("{id} retry");
            sent += 1;
        }
        if sent == 0 { "ok nothing to retry".into() } else { format!("ok retrying {sent}") }
    }

    // ---- events from the parts ----

    fn frame(&mut self, id: SessionId, pcm: Vec<u8>) {
        let Some(s) = self.sessions.get_mut(&id).filter(|s| s.stage == Stage::Listening) else { return };
        let voice = s.vad.push(&pcm);
        self.ports.hud.voice(&voice);
        if s.stream == StreamState::Up {
            self.ports.stream.audio(id, pcm.clone());
        }
        self.ports.encoder.push(id, pcm.clone());
        s.pcm.extend_from_slice(&pcm);
    }

    fn capture_ended(&mut self, id: SessionId, err: Option<String>, now: Instant) {
        let Some(s) = self.sessions.get(&id).filter(|s| s.stage == Stage::Listening) else { return };
        if s.pcm.is_empty() {
            // Scenario 16: the recorder gave no audio at all.
            let reason = err.unwrap_or_else(|| "no audio".into());
            elog!("{id} capture failed: {reason}");
            self.end_listening(id);
            self.ports.stream.abort(id);
            self.ports.encoder.abort(id);
            self.sessions.remove(&id);
            self.ports.desktop.notify(&format!("Microphone error: {reason}"));
            self.set_hud(id, Phase::Failed, now);
        } else {
            // Scenario 17: the device went away mid-recording. Process what we have.
            elog!("{id} capture ended early");
            self.stop(now);
        }
    }

    fn stream_event(&mut self, id: SessionId, event: StreamEvent, now: Instant) {
        let Some(s) = self.sessions.get_mut(&id) else { return };
        match event {
            StreamEvent::Open => elog!("{id} stream open +{}ms", now.duration_since(s.started).as_millis()),
            StreamEvent::Transcript { text, is_final } => {
                if !s.first_interim_logged && !text.trim().is_empty() {
                    s.first_interim_logged = true;
                    elog!("{id} first_text_ms={} final={is_final}", now.duration_since(s.started).as_millis());
                }
                s.live.update(&text, is_final);
            }
            StreamEvent::Failed(failure) if s.stream == StreamState::Up => {
                elog!("{id} stream failed: {failure:?}");
                self.stream_failed(id, failure, now);
            }
            StreamEvent::Closed if s.stream == StreamState::Up => {
                s.stream = StreamState::Closed;
                if let Some(stopped) = s.stopped {
                    elog!("{id} pass1_ms={} text={:?}", now.duration_since(stopped).as_millis(), s.live.text());
                }
                self.conclude_if_ready(id, now);
            }
            StreamEvent::Failed(_) | StreamEvent::Closed => {}
        }
    }

    /// The stream broke or was turned away. A key problem moves the dictation to the next key;
    /// otherwise the stream is retried once, then given up (pass 2 still runs).
    fn stream_failed(&mut self, id: SessionId, failure: Failure, now: Instant) {
        let trouble = match failure {
            Failure::Auth => Some(Trouble::Rejected),
            Failure::Credit => Some(Trouble::OutOfCredit),
            Failure::RateLimited => Some(Trouble::RateLimited),
            Failure::Other(_) => None,
        };
        if let Some(t) = trouble
            && self.fail_over(id, t, now)
        {
            let Some(s) = self.sessions.get_mut(&id) else { return };
            if s.stage == Stage::Listening {
                s.live.replaying = true;
                self.ports.stream.open(id, self.keys.key(s.key), &s.params, s.pcm.clone());
            } else {
                s.stream = StreamState::Dead;
                self.conclude_if_ready(id, now);
            }
            return;
        }
        let Some(s) = self.sessions.get_mut(&id) else { return };
        match failure {
            Failure::Auth | Failure::Credit => {
                s.stream = StreamState::Dead;
                self.out_of_keys(id, trouble.expect("a key problem"));
            }
            Failure::RateLimited | Failure::Other(_) if s.stage == Stage::Listening && s.stream_retries > 0 => {
                s.stream_retries -= 1;
                s.stream = StreamState::Waiting;
                self.schedule(now + STREAM_RETRY_AFTER, Timer::StreamRetry(id));
            }
            Failure::RateLimited | Failure::Other(_) => s.stream = StreamState::Dead,
        }
        self.conclude_if_ready(id, now);
    }

    /// Takes the dictation's key out for `trouble` and gives the dictation the next key that is
    /// not out. False when there is none (the key stays out all the same).
    fn fail_over(&mut self, id: SessionId, trouble: Trouble, now: Instant) -> bool {
        let Some(s) = self.sessions.get_mut(&id) else { return false };
        let from = s.key;
        let Some(to) = self.keys.report(from, trouble, now) else {
            elog!("{id} {} {trouble:?}, no other key to use", self.keys.label(from));
            return false;
        };
        s.key = to;
        elog!("{id} {} {trouble:?}, switching to {}", self.keys.label(from), self.keys.label(to));
        true
    }

    /// Scenario 14: no key can work. Tell the user once; keep the audio; do not retry.
    fn out_of_keys(&mut self, id: SessionId, trouble: Trouble) {
        let Some(s) = self.sessions.get_mut(&id) else { return };
        if !std::mem::replace(&mut s.auth_failed, true) {
            let message = match (trouble, self.keys.len()) {
                (Trouble::OutOfCredit, _) => "Deepgram credit is used up on every key. Your audio is kept; run `elf retry` later",
                (_, 1) => "Deepgram key rejected. Check DEEPGRAM_API_KEY",
                _ => "Every Deepgram key was rejected. Check your keys",
            };
            self.ports.desktop.notify(message);
        }
    }

    fn batch_result(&mut self, id: SessionId, result: Result<String, BatchError>, now: Instant) {
        if !self.sessions.get(&id).is_some_and(|s| matches!(s.pass2, Pass2::Pending)) {
            return;
        }
        // A turned-away key hands the same request to the next key at once. This does not use
        // up the retry below; every key can fail only once, so it ends.
        if let Err(e) = &result
            && let Some(t) = trouble_of(e)
            && self.fail_over(id, t, now)
        {
            self.submit_batch(id);
            return;
        }
        let Some(s) = self.sessions.get_mut(&id) else { return };
        let stopped = s.stopped.unwrap_or(now);
        match result {
            Ok(text) => {
                elog!("{id} pass2_ms={} words={}", now.duration_since(stopped).as_millis(), text.split_whitespace().count());
                s.pass2 = Pass2::Done(Ok(text));
            }
            Err(e) if trouble_of(&e).is_some_and(|t| t != Trouble::RateLimited) => {
                let trouble = trouble_of(&e).expect("checked");
                s.pass2 = Pass2::Done(Err(e));
                self.out_of_keys(id, trouble);
            }
            Err(e) if is_retryable(&e) && s.batch_attempt == 0 => {
                elog!("{id} pass2 failed ({e}), retrying");
                s.batch_attempt = 1;
                self.schedule(now + BATCH_RETRY_AFTER, Timer::BatchRetry(id));
                return;
            }
            Err(e) => {
                elog!("{id} pass2 failed ({e})");
                s.pass2 = Pass2::Done(Err(e));
            }
        }
        self.conclude_if_ready(id, now);
    }

    fn timer(&mut self, timer: Timer, now: Instant) {
        match timer {
            Timer::MaxLength(id) => {
                if self.active == Some(id) {
                    elog!("{id} max length reached");
                    self.stop(now);
                }
            }
            Timer::StreamRetry(id) => {
                let Some(s) = self.sessions.get_mut(&id).filter(|s| s.stream == StreamState::Waiting) else { return };
                s.stream = StreamState::Up;
                s.live.replaying = true;
                self.ports.stream.open(id, self.keys.key(s.key), &s.params, s.pcm.clone());
            }
            Timer::FlushWait(id) => {
                let Some(s) = self.sessions.get_mut(&id).filter(|s| s.stage == Stage::Finishing) else { return };
                if s.stream == StreamState::Up {
                    self.ports.stream.abort(id);
                }
                if matches!(s.stream, StreamState::Up | StreamState::Waiting) {
                    s.stream = StreamState::Dead;
                }
                self.conclude_if_ready(id, now);
            }
            Timer::BatchRetry(id) => {
                if self.sessions.get(&id).is_some_and(|s| s.stage == Stage::Finishing && matches!(s.pass2, Pass2::Pending)) {
                    self.submit_batch(id);
                }
            }
            Timer::KeepWarm(id) => {
                if self.active == Some(id) {
                    self.ports.batch.warm_up();
                    self.schedule(now + KEEP_WARM_EVERY, Timer::KeepWarm(id));
                }
            }
            Timer::HideHud(seq) => {
                if seq == self.hud_seq {
                    self.ports.hud.phase(Phase::Hidden);
                }
            }
        }
    }

    fn delivered(&mut self, id: Option<SessionId>, outcome: Outcome) {
        let now = Instant::now();
        let phase = match &outcome {
            Outcome::Pasted => Phase::Done,
            Outcome::CopiedOnly(Reason::TargetClosed | Reason::NothingFocused) => {
                self.ports.desktop.notify(PASTE_AGAIN_HINT);
                Phase::Done
            }
            Outcome::Failed(e) => {
                self.ports.desktop.notify(&format!("Paste failed: {e}. The text is in `elf history`"));
                Phase::Failed
            }
        };
        elog!("{id:?} delivered {outcome:?}");
        if let Some(id) = id {
            self.sessions.remove(&id);
            self.set_hud(id, phase, now);
        }
    }

    // ---- finishing a dictation ----

    /// Decides the final text once everything it depends on is known.
    fn conclude_if_ready(&mut self, id: SessionId, now: Instant) {
        let Some(s) = self.sessions.get(&id).filter(|s| s.stage == Stage::Finishing) else { return };
        let pass2_text = match &s.pass2 {
            Pass2::Pending => return,
            Pass2::Done(Ok(t)) if !t.trim().is_empty() => Some(t.clone()),
            Pass2::Done(_) => None,
        };
        let chosen = match pass2_text {
            Some(t) => Some((t, "pass2")),
            None if !matches!(s.stream, StreamState::Dead | StreamState::Closed) => return,
            None => Some(s.live.text()).filter(|t| !t.trim().is_empty()).map(|t| (t, "pass1")),
        };
        let text = chosen.as_ref().map(|(raw, _)| text::finalize(raw, &self.cfg)).filter(|t| !t.is_empty());
        match (text, chosen) {
            (Some(text), Some((_, which))) => self.deliver(id, text, which, now),
            _ if matches!(s.pass2, Pass2::Done(Ok(_))) => self.nothing_heard(id, now),
            _ => self.hold(id, now),
        }
    }

    /// Pass 2 heard the whole recording and found no words: nothing to keep or retry.
    fn nothing_heard(&mut self, id: SessionId, now: Instant) {
        elog!("{id} nothing heard");
        self.ports.store.delete_wav(id);
        self.sessions.remove(&id);
        self.set_hud(id, Phase::NothingHeard, now);
    }

    fn entry(&self, id: SessionId, text: Option<String>, chosen: Option<&str>, now: Instant) -> Entry {
        let s = &self.sessions[&id];
        let non_empty = |t: String| Some(t).filter(|t| !t.trim().is_empty());
        let pass2 = match &s.pass2 {
            Pass2::Done(Ok(t)) => non_empty(t.clone()),
            _ => None,
        };
        let error = match (&s.pass2, &text) {
            (_, Some(_)) => None,
            (Pass2::Done(Err(e)), None) => Some(format!("pass2: {e}")),
            _ => Some("no speech recognised".to_string()),
        };
        Entry {
            id: id.to_string(),
            ts: clock::iso_local(id),
            app: non_empty(s.class.clone()),
            target: s.window.as_ref().map(|w| w.address.clone()),
            ms_audio: s.audio_ms,
            ms_to_text: now.duration_since(s.stopped.unwrap_or(now)).as_millis() as u64,
            pass1: non_empty(s.live.text()),
            pass2,
            chosen: chosen.map(String::from),
            wav: text.is_none().then(|| s.wav_path.as_ref().map(|p| p.display().to_string())).flatten(),
            text,
            error,
        }
    }

    fn deliver(&mut self, id: SessionId, text: String, chosen: &str, now: Instant) {
        let entry = self.entry(id, Some(text.clone()), Some(chosen), now);
        elog!("{id} text_ms={} chosen={chosen}", entry.ms_to_text);
        self.ports.store.append(&entry);
        self.ports.store.delete_wav(id);
        let s = self.sessions.get_mut(&id).expect("finishing session");
        s.stage = Stage::Delivering;
        let window = match (self.cfg.paste_into, s.kind == Kind::Dictation) {
            (PasteInto::Original, true) => s.window.clone(),
            _ => None,
        };
        self.ports.deliver.deliver(Some(id), DeliverRequest { text, window });
    }

    /// No text: keep the audio and say how to get it back.
    fn hold(&mut self, id: SessionId, now: Instant) {
        let entry = self.entry(id, None, None, now);
        elog!("{id} held: {}", entry.error.as_deref().unwrap_or(""));
        self.ports.store.append(&entry);
        if !self.sessions[&id].auth_failed {
            self.ports.desktop.notify("Kept your audio. Run `elf retry`");
        }
        self.sessions.remove(&id);
        self.set_hud(id, Phase::Failed, now);
    }

    // ---- HUD and timers ----

    fn set_hud(&mut self, id: SessionId, phase: Phase, now: Instant) {
        if self.hud_owner != Some(id) {
            return;
        }
        self.hud_seq += 1;
        self.ports.hud.phase(phase);
        let hide_after = match phase {
            Phase::Done => HIDE_AFTER_DONE,
            Phase::NothingHeard => HIDE_AFTER_NOTHING,
            Phase::Failed => HIDE_AFTER_FAILED,
            Phase::Hidden | Phase::Listening | Phase::Processing => return,
        };
        self.schedule(now + hide_after, Timer::HideHud(self.hud_seq));
    }

    fn schedule(&mut self, at: Instant, timer: Timer) {
        self.timers.push((at, timer));
    }
}

/// What a failed request says about the key it used.
fn trouble_of(e: &BatchError) -> Option<Trouble> {
    match e {
        BatchError::Auth => Some(Trouble::Rejected),
        BatchError::Status(402, _) => Some(Trouble::OutOfCredit),
        BatchError::Status(429, _) => Some(Trouble::RateLimited),
        _ => None,
    }
}

/// Runs the machine until `Quit`, a stop request or a closed channel.
pub fn run(mut machine: Machine, events: Receiver<Event>, stop: &AtomicBool) {
    const TICK: Duration = Duration::from_millis(250);
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let wait = machine.next_deadline().map_or(TICK, |d| d.saturating_duration_since(Instant::now()).min(TICK));
        match events.recv_timeout(wait) {
            Ok(ev) => {
                if machine.handle(ev, Instant::now()) == Flow::Quit {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        machine.tick(Instant::now());
    }
    machine.shutdown();
}

#[cfg(test)]
mod tests;
