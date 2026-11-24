//! State machine tests with fake ports. Scenario numbers refer to the table in docs/behaviour.md.

use super::*;
use std::collections::BTreeMap as Map;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc;

use crate::vad::Voice;

#[derive(Default)]
struct Shared {
    calls: Mutex<Vec<String>>,
    entries: Mutex<Vec<Entry>>,
    wavs: Mutex<Map<u64, Vec<u8>>>,
    active: Mutex<Option<Window>>,
    mic_fails: Mutex<bool>,
    delivered: Mutex<Vec<(Option<SessionId>, String, Option<Window>)>>,
    batch_submits: Mutex<Vec<(SessionId, Params)>>,
    uploads: Mutex<Vec<(&'static str, usize)>>,
    encoder_fails: Mutex<bool>,
    stream_replays: Mutex<Vec<usize>>,
    /// Which key each stream open and pass 2 request used: (kind, session, key).
    key_uses: Mutex<Vec<(&'static str, SessionId, String)>>,
}

#[derive(Clone, Default)]
struct Fake(Arc<Shared>);

impl Fake {
    fn log(&self, s: impl Into<String>) {
        self.0.calls.lock().unwrap().push(s.into());
    }
    fn count(&self, prefix: &str) -> usize {
        self.0.calls.lock().unwrap().iter().filter(|c| c.starts_with(prefix)).count()
    }
    fn has(&self, call: &str) -> bool {
        self.0.calls.lock().unwrap().iter().any(|c| c == call)
    }
    fn notifications(&self) -> Vec<String> {
        self.0.calls.lock().unwrap().iter().filter_map(|c| c.strip_prefix("notify ").map(String::from)).collect()
    }
    fn focus(&self, class: &str, addr: &str) {
        *self.0.active.lock().unwrap() = Some(Window { address: addr.into(), class: class.into() });
    }
}

impl Capture for Fake {
    fn start(&self, id: SessionId) -> Result<(), String> {
        if *self.0.mic_fails.lock().unwrap() {
            return Err("no such device".into());
        }
        self.log(format!("capture.start {id}"));
        Ok(())
    }
    fn stop(&self, id: SessionId) {
        self.log(format!("capture.stop {id}"));
    }
}

impl Stream for Fake {
    fn open(&self, id: SessionId, key: &ApiKey, _params: &Params, replay: Vec<u8>) {
        self.0.stream_replays.lock().unwrap().push(replay.len());
        self.0.key_uses.lock().unwrap().push(("open", id, key.expose().to_string()));
        self.log(format!("stream.open {id}"));
    }
    fn audio(&self, _id: SessionId, _pcm: Vec<u8>) {
        self.log("stream.audio");
    }
    fn finish(&self, id: SessionId) {
        self.log(format!("stream.finish {id}"));
    }
    fn abort(&self, id: SessionId) {
        self.log(format!("stream.abort {id}"));
    }
}

impl Batch for Fake {
    fn submit(&self, id: SessionId, key: &ApiKey, upload: &Upload, params: &Params, _timeout: Duration) {
        self.0.key_uses.lock().unwrap().push(("submit", id, key.expose().to_string()));
        self.0.batch_submits.lock().unwrap().push((id, params.clone()));
        self.0.uploads.lock().unwrap().push((upload.content_type, upload.bytes.len()));
        self.log(format!("batch.submit {id}"));
    }
    fn warm_up(&self) {
        self.log("batch.warm_up");
    }
}

impl Encoder for Fake {
    fn start(&self, id: SessionId) {
        self.log(format!("encoder.start {id}"));
    }
    fn push(&self, _id: SessionId, _pcm: Vec<u8>) {
        self.log("encoder.push");
    }
    fn finish(&self, _id: SessionId) -> Option<Vec<u8>> {
        self.log("encoder.finish");
        (!*self.0.encoder_fails.lock().unwrap()).then(|| b"OggS-fake".to_vec())
    }
    fn abort(&self, id: SessionId) {
        self.log(format!("encoder.abort {id}"));
    }
}

impl Deliver for Fake {
    fn deliver(&self, id: Option<SessionId>, req: DeliverRequest) {
        self.0.delivered.lock().unwrap().push((id, req.text, req.window));
    }
}

impl Desktop for Fake {
    fn active_window(&self) -> Option<Window> {
        self.0.active.lock().unwrap().clone()
    }
    fn set_submap(&self, on: bool) {
        self.log(format!("submap {on}"));
    }
    fn notify(&self, message: &str) {
        self.log(format!("notify {message}"));
    }
    fn sound(&self, sound: Sound) {
        self.log(format!("sound {sound:?}"));
    }
}

impl Store for Fake {
    fn save_wav(&self, id: SessionId, wav: &[u8]) -> io::Result<PathBuf> {
        self.0.wavs.lock().unwrap().insert(id, wav.to_vec());
        Ok(PathBuf::from(format!("/spool/{id}.wav")))
    }
    fn delete_wav(&self, id: SessionId) {
        self.0.wavs.lock().unwrap().remove(&id);
    }
    fn spooled(&self) -> Vec<(SessionId, PathBuf)> {
        self.0.wavs.lock().unwrap().keys().map(|id| (*id, PathBuf::from(format!("/spool/{id}.wav")))).collect()
    }
    fn load_wav(&self, path: &Path) -> io::Result<Vec<u8>> {
        let id: u64 = path.file_stem().unwrap().to_str().unwrap().parse().unwrap();
        Ok(self.0.wavs.lock().unwrap()[&id].clone())
    }
    fn append(&self, entry: &Entry) {
        self.0.entries.lock().unwrap().push(entry.clone());
    }
    fn recent(&self, n: usize) -> Vec<Entry> {
        let e = self.0.entries.lock().unwrap();
        e[e.len().saturating_sub(n)..].to_vec()
    }
    fn last_text(&self) -> Option<String> {
        self.0.entries.lock().unwrap().iter().rev().find_map(|e| e.text.clone())
    }
}

impl Hud for Fake {
    fn phase(&self, phase: Phase) {
        self.log(format!("hud {phase:?}"));
    }
    fn voice(&self, _voice: &Voice) {}
}

/// Frames filled with 1 count as voice, frames of 0 as silence.
struct FakeVad(u32);

impl Vad for FakeVad {
    fn push(&mut self, pcm: &[u8]) -> Voice {
        if pcm[0] != 0 {
            self.0 += 20;
        }
        Voice::default()
    }
    fn voiced_ms(&self) -> u32 {
        self.0
    }
}

struct Rig {
    m: Machine,
    f: Fake,
    t0: Instant,
}

/// Most tests cover the live stream as well, so they turn it on.
fn cfg() -> Config {
    Config { live_stream: true, ..Config::default() }
}

#[test]
fn without_the_live_stream_only_pass_2_runs() {
    let mut r = rig_with(Config::default(), true);
    let id = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    r.batch(id, Ok("hello there"), 2500);
    assert_eq!(r.f.count("stream.open"), 0);
    assert_eq!(r.f.count("batch.submit"), 1);
    assert_eq!(r.entries()[0].text.as_deref(), Some("hello there "));
}

fn rig_with(cfg: Config, key: bool) -> Rig {
    let keys: &[&str] = if key { &["test-key-0000-fake"] } else { &[] };
    rig_keys(cfg, keys)
}

fn rig_keys(cfg: Config, keys: &[&str]) -> Rig {
    let f = Fake::default();
    f.focus("kitty", "0xa");
    let ports = Ports {
        capture: Box::new(f.clone()),
        stream: Box::new(f.clone()),
        batch: Box::new(f.clone()),
        encoder: Box::new(f.clone()),
        deliver: Box::new(f.clone()),
        desktop: Box::new(f.clone()),
        store: Box::new(f.clone()),
        hud: Box::new(f.clone()),
        new_vad: || Box::new(FakeVad(0)),
    };
    // A fixed clock: ids still differ because the machine bumps them.
    let pool = KeyPool::new(keys.iter().map(|k| ApiKey::new(k.to_string())).collect());
    let m = Machine::new(cfg, pool, ports, || 1_763_900_000_000);
    Rig { m, f, t0: Instant::now() }
}

fn rig() -> Rig {
    rig_with(cfg(), true)
}

impl Rig {
    fn at(&self, ms: u64) -> Instant {
        self.t0 + Duration::from_millis(ms)
    }

    fn cmd(&mut self, c: Command, ms: u64) -> String {
        let (tx, rx) = mpsc::channel();
        self.m.handle(Event::Command(c, tx), self.at(ms));
        rx.recv().unwrap()
    }

    /// Feeds `ms` of audio: voiced or silent 20 ms frames.
    fn audio(&mut self, id: SessionId, ms: u64, voiced: bool) {
        for _ in 0..ms / 20 {
            self.m.handle(Event::Frame(id, vec![u8::from(voiced); 640]), self.t0);
        }
    }

    /// The first line of the status reply; the second one is about the keys.
    fn status(&mut self, ms: u64) -> String {
        self.cmd(Command::Status, ms).lines().next().unwrap_or_default().to_string()
    }

    fn keys_line(&mut self, ms: u64) -> String {
        self.cmd(Command::Status, ms).lines().nth(1).unwrap_or_default().to_string()
    }

    /// Who used which key, as (kind, key).
    fn uses(&self) -> Vec<(&'static str, String)> {
        self.f.0.key_uses.lock().unwrap().iter().map(|(k, _, key)| (*k, key.clone())).collect()
    }

    fn ev(&mut self, e: Event, ms: u64) {
        self.m.handle(e, self.at(ms));
    }

    fn stream(&mut self, id: SessionId, e: StreamEvent, ms: u64) {
        self.ev(Event::Stream(id, e), ms);
    }

    fn text(&mut self, id: SessionId, text: &str, is_final: bool, ms: u64) {
        self.stream(id, StreamEvent::Transcript { text: text.into(), is_final }, ms);
    }

    fn batch(&mut self, id: SessionId, r: Result<&str, BatchError>, ms: u64) {
        self.ev(Event::Batch(id, r.map(String::from)), ms);
    }

    /// Starts a dictation with 1 s of voice and returns its id.
    fn dictate(&mut self, ms: u64) -> SessionId {
        assert_eq!(self.cmd(Command::Toggle, ms), "ok listening");
        let id = self.m.active.expect("listening");
        self.audio(id, 1000, true);
        id
    }

    fn delivered(&self) -> Vec<(Option<SessionId>, String, Option<Window>)> {
        self.f.0.delivered.lock().unwrap().clone()
    }

    fn entries(&self) -> Vec<Entry> {
        self.f.0.entries.lock().unwrap().clone()
    }
}

fn win(addr: &str, class: &str) -> Window {
    Window { address: addr.into(), class: class.into() }
}

#[test]
fn scenario_1_and_2_tap_starts_and_tap_stops() {
    let mut r = rig();
    assert_eq!(r.cmd(Command::Toggle, 0), "ok listening");
    assert!(r.f.has("submap true") && r.f.has("hud Listening"));
    let id = r.m.active.unwrap();
    r.audio(id, 1000, true);
    assert_eq!(r.cmd(Command::Toggle, 2000), "ok processing 1");
    assert!(r.f.has("submap false") && r.f.has("hud Processing"));
    assert_eq!(r.f.count("stream.finish"), 1);
    assert_eq!(r.f.count("batch.submit"), 1);
    assert_eq!(r.f.0.wavs.lock().unwrap()[&id].len(), 44 + 32_000);
}

#[test]
fn scenario_3_hold_and_release_stops_but_a_short_press_is_a_tap() {
    let mut r = rig();
    assert_eq!(r.cmd(Command::Press, 0), "ok listening");
    let id = r.m.active.unwrap();
    r.audio(id, 1000, true);
    assert_eq!(r.cmd(Command::Release, 400), "ok processing 1");
    assert!(r.m.active.is_none());

    // A short press leaves the dictation open; the second press stops it.
    assert_eq!(r.cmd(Command::Press, 5000), "ok listening");
    assert_eq!(r.cmd(Command::Release, 5100), "ok listening");
    let id2 = r.m.active.unwrap();
    r.audio(id2, 1000, true);
    assert_eq!(r.cmd(Command::Press, 9000), "ok processing 2");
    assert_eq!(r.cmd(Command::Release, 9100), "ok processing 2");
}

#[test]
fn scenario_4_start_while_listening_is_ignored() {
    let mut r = rig();
    r.cmd(Command::Start, 0);
    assert_eq!(r.cmd(Command::Start, 100), "ok listening");
    assert_eq!(r.f.count("capture.start"), 1);
    assert_eq!(r.f.count("stream.open"), 1);
}

#[test]
fn scenario_5_new_dictation_while_the_old_one_processes_pastes_into_its_own_target() {
    let mut r = rig();
    r.f.focus("kitty", "0xa");
    let first = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    r.f.focus("firefox", "0xb");
    let second = r.dictate(2500);
    assert_ne!(first, second);
    r.cmd(Command::Toggle, 4000);

    r.batch(second, Ok("Second."), 4500);
    r.batch(first, Ok("First."), 4600);
    let d = r.delivered();
    assert_eq!(d[0], (Some(second), "Second. ".into(), Some(win("0xb", "firefox"))));
    assert_eq!(d[1], (Some(first), "First. ".into(), Some(win("0xa", "kitty"))));
}

#[test]
fn scenario_6_cancel_drops_everything() {
    let mut r = rig();
    let id = r.dictate(0);
    assert_eq!(r.cmd(Command::Cancel, 1500), "ok idle");
    assert!(r.f.has(&format!("stream.abort {id}")) && r.f.has("submap false") && r.f.has("hud Hidden"));
    assert_eq!(r.f.count("batch.submit"), 0);
    assert!(r.entries().is_empty() && r.f.0.wavs.lock().unwrap().is_empty());
    assert!(r.delivered().is_empty());
}

#[test]
fn scenario_7_too_short_recordings_are_discarded() {
    let mut r = rig();
    r.cmd(Command::Toggle, 0);
    let id = r.m.active.unwrap();
    r.audio(id, 300, true);
    assert_eq!(r.cmd(Command::Toggle, 400), "ok idle");

    assert_eq!(r.f.count("batch.submit"), 0);
    assert!(r.entries().is_empty() && r.f.0.wavs.lock().unwrap().is_empty());
    assert_eq!(r.f.count("hud NothingHeard"), 1);
    assert!(r.f.notifications().is_empty());
}

#[test]
fn scenario_7_no_voice_activity_still_asks_pass_2() {
    // A noisy microphone can hide real speech from the voice detector; Deepgram decides.
    let mut r = rig();
    r.cmd(Command::Toggle, 0);
    let id = r.m.active.unwrap();
    r.audio(id, 2000, false);
    r.cmd(Command::Toggle, 2000);
    assert_eq!(r.f.count("batch.submit"), 1);
}

#[test]
fn scenario_8_max_length_stops_by_itself() {
    let mut c = cfg();
    c.max_seconds = 5;
    let mut r = rig_with(c, true);
    r.cmd(Command::Toggle, 0);
    let id = r.m.active.unwrap();
    r.audio(id, 1000, true);
    r.m.tick(r.at(4999));
    assert!(r.m.active.is_some());
    r.m.tick(r.at(5000));
    assert!(r.m.active.is_none());
    assert_eq!(r.f.count("batch.submit"), 1);
}

#[test]
fn scenario_9_stream_drop_retries_once_and_replays_the_buffer() {
    let mut r = rig();
    let id = r.dictate(0);
    r.stream(id, StreamEvent::Failed(Failure::Other("reset".into())), 300);
    assert_eq!(r.f.count("stream.open"), 1);
    r.m.tick(r.at(549));
    assert_eq!(r.f.count("stream.open"), 1);
    r.m.tick(r.at(550));
    assert_eq!(r.f.count("stream.open"), 2);
    assert_eq!(*r.f.0.stream_replays.lock().unwrap(), [0, 32_000]);

    // The retry fails too: carry on without live text, pass 2 still runs.
    r.stream(id, StreamEvent::Failed(Failure::Other("reset".into())), 600);
    r.m.tick(r.at(5000));
    assert_eq!(r.f.count("stream.open"), 2);
    r.cmd(Command::Toggle, 2000);
    assert_eq!(r.f.count("batch.submit"), 1);
    assert_eq!(r.f.count("stream.finish"), 0);
    r.batch(id, Ok("Still works."), 2400);
    assert_eq!(r.delivered()[0].1, "Still works. ");
}

#[test]
fn scenario_10_stream_without_words_is_decided_by_pass_2() {
    let mut r = rig();
    let id = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    r.stream(id, StreamEvent::Closed, 2100);
    assert!(r.delivered().is_empty());
    r.batch(id, Ok("Hello there."), 2800);
    let e = &r.entries()[0];
    assert_eq!((e.chosen.as_deref(), e.pass1.as_deref(), e.text.as_deref()), (Some("pass2"), None, Some("Hello there. ")));
}

#[test]
fn pass_2_wins_without_waiting_for_the_stream() {
    let mut r = rig();
    let id = r.dictate(0);
    r.text(id, "hello wor", false, 500);
    r.cmd(Command::Toggle, 2000);
    r.batch(id, Ok("Hello world."), 2500);
    assert_eq!(r.delivered().len(), 1);
    assert_eq!(r.entries()[0].pass1.as_deref(), Some("hello wor"));
    assert!(r.f.0.wavs.lock().unwrap().is_empty());
}

#[test]
fn scenario_11_pass_2_failure_retries_once_then_uses_pass_1() {
    let mut r = rig();
    let id = r.dictate(0);
    r.text(id, "hello", true, 400);
    r.text(id, "world.", true, 900);
    r.cmd(Command::Toggle, 2000);
    r.stream(id, StreamEvent::Closed, 2200);
    r.batch(id, Err(BatchError::Status(503, String::new())), 2300);
    assert_eq!(r.f.count("batch.submit"), 1);
    r.m.tick(r.at(2799));
    assert_eq!(r.f.count("batch.submit"), 1);
    r.m.tick(r.at(2800));
    assert_eq!(r.f.count("batch.submit"), 2);
    r.batch(id, Err(BatchError::Timeout), 9000);
    assert_eq!(r.f.count("batch.submit"), 2);
    let e = &r.entries()[0];
    assert_eq!((e.chosen.as_deref(), e.text.as_deref()), (Some("pass1"), Some("hello world. ")));
}

#[test]
fn pass_2_failure_waits_for_the_stream_flush_but_not_longer_than_1_5_s() {
    let mut r = rig();
    let id = r.dictate(0);
    r.text(id, "half a sentence", false, 900);
    r.cmd(Command::Toggle, 2000);
    r.batch(id, Err(BatchError::Status(400, "bad".into())), 2100);
    assert!(r.delivered().is_empty());
    r.m.tick(r.at(3499));
    assert!(r.delivered().is_empty());
    r.m.tick(r.at(3500));
    assert_eq!(r.delivered()[0].1, "half a sentence ");
    assert!(r.f.has(&format!("stream.abort {id}")));
}

#[test]
fn scenario_12_and_13_both_passes_failed_keeps_the_audio() {
    let mut r = rig();
    let id = r.dictate(0);
    r.stream(id, StreamEvent::Failed(Failure::Other("offline".into())), 100);
    r.m.tick(r.at(400));
    r.stream(id, StreamEvent::Failed(Failure::Other("offline".into())), 450);
    r.cmd(Command::Toggle, 2000);
    r.batch(id, Err(BatchError::Network("offline".into())), 2010);
    r.m.tick(r.at(2600));
    r.batch(id, Err(BatchError::Network("offline".into())), 2610);

    let e = &r.entries()[0];
    assert_eq!((e.text.as_deref(), e.pass1.as_deref(), e.chosen.as_deref()), (None, None, None));
    assert_eq!(e.wav.as_deref(), Some(format!("/spool/{id}.wav").as_str()));
    assert!(e.error.as_deref().unwrap().starts_with("pass2:"));
    assert!(r.f.0.wavs.lock().unwrap().contains_key(&id));
    assert_eq!(r.f.notifications(), ["Kept your audio. Run `elf retry`"]);
    assert!(r.f.has("hud Failed") && r.delivered().is_empty());
    assert_eq!(r.status(3000), "ok idle");
}

#[test]
fn both_passes_empty_means_nothing_heard() {
    // Pass 2 succeeded on the whole recording and found no words: nothing to keep or retry.
    let mut r = rig();
    let id = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    r.stream(id, StreamEvent::Closed, 2100);
    r.batch(id, Ok(""), 2500);
    assert!(r.entries().is_empty());
    assert!(!r.f.0.wavs.lock().unwrap().contains_key(&id));
    assert_eq!(r.f.count("hud NothingHeard"), 1);
    assert!(r.f.notifications().is_empty());
}

#[test]
fn scenario_14_rejected_key_notifies_once_and_never_retries() {
    let mut r = rig();
    let id = r.dictate(0);
    r.stream(id, StreamEvent::Failed(Failure::Auth), 100);
    r.m.tick(r.at(1000));
    assert_eq!(r.f.count("stream.open"), 1);
    r.cmd(Command::Toggle, 2000);
    assert_eq!(r.f.count("batch.submit"), 0);
    assert_eq!(r.f.notifications(), ["Deepgram key rejected. Check DEEPGRAM_API_KEY"]);
    assert!(r.entries()[0].text.is_none());
    assert!(r.f.0.wavs.lock().unwrap().contains_key(&id));
}

#[test]
fn scenario_14_pass_2_401_does_not_retry() {
    let mut r = rig();
    let id = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    r.stream(id, StreamEvent::Closed, 2100);
    r.batch(id, Err(BatchError::Auth), 2200);
    r.m.tick(r.at(9000));
    assert_eq!(r.f.count("batch.submit"), 1);
    assert_eq!(r.f.notifications().len(), 1);
    assert!(r.f.0.wavs.lock().unwrap().contains_key(&id));
}

#[test]
fn scenario_15_no_key_refuses_start_with_a_notification() {
    let mut r = rig_with(cfg(), false);
    assert!(r.cmd(Command::Toggle, 0).starts_with("err"));
    assert_eq!(r.f.count("capture.start"), 0);
    assert!(r.f.notifications()[0].contains("DEEPGRAM_API_KEY"));
}

#[test]
fn scenario_16_mic_failure_at_start_and_recorder_ending_without_audio() {
    let mut r = rig();
    *r.f.0.mic_fails.lock().unwrap() = true;
    assert!(r.cmd(Command::Toggle, 0).starts_with("err"));
    assert!(r.f.has("hud Failed") && !r.f.has("submap true"));
    assert!(r.f.notifications()[0].starts_with("Microphone error"));

    *r.f.0.mic_fails.lock().unwrap() = false;
    r.cmd(Command::Toggle, 3000);
    let id = r.m.active.unwrap();
    r.ev(Event::CaptureEnded(id, Some("connect failed".into())), 3100);
    assert_eq!(r.status(3200), "ok idle");
    assert!(r.f.has("submap false"));
    assert_eq!(r.f.count("batch.submit"), 0);
}

#[test]
fn scenario_17_capture_ending_early_processes_what_was_recorded() {
    let mut r = rig();
    let id = r.dictate(0);
    r.ev(Event::CaptureEnded(id, None), 1200);
    assert!(r.m.active.is_none());
    assert_eq!(r.f.count("batch.submit"), 1);
    assert_eq!(r.status(1300), "ok processing 1");
}

#[test]
fn scenario_18_target_closed_notifies_that_the_text_is_copied() {
    let mut r = rig();
    let id = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    r.batch(id, Ok("Hi."), 2500);
    r.ev(Event::Delivered(Some(id), Outcome::CopiedOnly(Reason::TargetClosed)), 2600);
    assert_eq!(r.f.notifications(), ["Copied. Paste with Super+Ctrl+M"]);
    assert_eq!(r.status(2700), "ok idle");
    assert_eq!(r.entries()[0].text.as_deref(), Some("Hi. "));
}

#[test]
fn scenario_19_paste_into_original_or_focused() {
    let mut r = rig();
    let id = r.dictate(0);
    r.f.focus("firefox", "0xb");
    r.cmd(Command::Toggle, 2000);
    r.batch(id, Ok("Hi."), 2500);
    assert_eq!(r.delivered()[0].2, Some(win("0xa", "kitty")));

    let mut c = cfg();
    c.paste_into = PasteInto::Focused;
    let mut r = rig_with(c, true);
    let id = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    r.batch(id, Ok("Hi."), 2500);
    assert_eq!(r.delivered()[0].2, None);
}

#[test]
fn scenario_22_retry_resends_spooled_audio_to_the_focused_window() {
    let mut r = rig();
    let id = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    r.stream(id, StreamEvent::Closed, 2100);
    r.batch(id, Err(BatchError::Status(400, String::new())), 2200);
    assert!(r.f.0.wavs.lock().unwrap().contains_key(&id));

    // elfd restarted: a new machine finds the spool.
    let spooled = r.f.0.wavs.lock().unwrap().clone();
    let mut r2 = rig();
    *r2.f.0.wavs.lock().unwrap() = spooled;
    assert_eq!(r2.cmd(Command::Retry, 0), "ok retrying 1");
    assert_eq!(r2.cmd(Command::Retry, 10), "ok nothing to retry");
    r2.batch(id, Ok("Recovered."), 500);
    assert_eq!(r2.delivered(), [(Some(id), "Recovered. ".into(), None)]);
    assert!(r2.f.0.wavs.lock().unwrap().is_empty());
}

#[test]
fn scenario_24_snippet_replaces_a_whole_transcript() {
    let mut c = cfg();
    c.snippets.insert("insert my email".into(), "someone@example.com".into());
    let mut r = rig_with(c, true);
    let id = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    r.batch(id, Ok("Insert my email."), 2400);
    assert_eq!(r.delivered()[0].1, "someone@example.com ");
}

#[test]
fn paste_again_sends_the_last_text_to_the_focused_window() {
    let mut r = rig();
    assert!(r.cmd(Command::PasteAgain, 0).starts_with("err"));
    let id = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    r.batch(id, Ok("Hello."), 2400);
    assert_eq!(r.cmd(Command::PasteAgain, 3000), "ok pasting");
    assert_eq!(r.delivered()[1], (None, "Hello. ".into(), None));
}

#[test]
fn per_app_keyterms_follow_the_target_window() {
    let mut c = cfg();
    c.keyterms.always = vec!["ELF".into()];
    c.keyterms.apps.insert("kitty".into(), vec!["cargo".into()]);
    let mut r = rig_with(c, true);
    let id = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    assert_eq!(r.f.0.batch_submits.lock().unwrap()[0], (id, Params {
        language: "en".into(),
        keyterms: vec!["ELF".into(), "cargo".into()],
        dictation: false,
        mip_opt_out: true,
    }));
}

#[test]
fn history_command_lists_entries() {
    let mut r = rig();
    let id = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    r.batch(id, Ok("Hello."), 2400);
    let h = r.cmd(Command::History(5), 3000);
    assert!(h.starts_with("ok 1\n") && h.ends_with("kitty Hello."), "{h}");
}

#[test]
fn hud_hides_after_done_but_a_newer_dictation_keeps_it() {
    let mut r = rig();
    let id = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    r.batch(id, Ok("Hi."), 2100);
    r.ev(Event::Delivered(Some(id), Outcome::Pasted), 2200);
    assert!(r.f.has("hud Done"));
    let hides = r.f.count("hud Hidden");
    // A new dictation begins before the hide timer would fire.
    r.cmd(Command::Toggle, 2300);
    r.m.tick(Instant::now() + Duration::from_secs(5));
    assert_eq!(r.f.count("hud Hidden"), hides);
}

#[test]
fn live_text_joins_finals_and_the_current_guess() {
    let mut l = Live::default();
    l.update("hello", false);
    l.update("hello world.", true);
    l.update("how are", false);
    assert_eq!(l.text(), "hello world. how are");
    l.replaying = true;
    l.update("hello world.", true);
    assert_eq!(l.text(), "hello world.");
}

#[test]
fn sounds_play_only_when_enabled() {
    let mut r = rig();
    r.dictate(0);
    assert_eq!(r.f.count("sound"), 0);
    let mut c = cfg();
    c.sounds = true;
    let mut r = rig_with(c, true);
    r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    assert!(r.f.has("sound Start") && r.f.has("sound Stop"));
}

#[test]
fn quit_replies_and_stops_the_loop() {
    let mut r = rig();
    let (tx, rx) = mpsc::channel();
    assert_eq!(r.m.handle(Event::Command(Command::Quit, tx), r.at(0)), Flow::Quit);
    assert_eq!(rx.recv().unwrap(), "ok bye");
}

#[test]
fn pass_2_uploads_the_opus_file_and_warms_the_connection_at_start() {
    let mut r = rig();
    r.cmd(Command::Toggle, 0);
    assert_eq!(r.f.count("batch.warm_up"), 1);
    assert_eq!(r.f.count("encoder.start"), 1);
    let id = r.m.active.unwrap();
    r.audio(id, 1000, true);
    assert_eq!(r.f.count("encoder.push"), 50);
    // The connection is touched every 2 s while recording, and not after stop.
    r.m.tick(r.at(2000));
    r.m.tick(r.at(4000));
    assert_eq!(r.f.count("batch.warm_up"), 3);
    r.cmd(Command::Toggle, 4500);
    r.m.tick(r.at(6000));
    assert_eq!(r.f.count("batch.warm_up"), 3);
    assert_eq!(*r.f.0.uploads.lock().unwrap(), [("audio/ogg", 9)]);
    // The spool still gets the full WAV.
    assert_eq!(r.f.0.wavs.lock().unwrap()[&id].len(), 44 + 32_000);
}

#[test]
fn pass_2_falls_back_to_wav_when_the_encoder_fails() {
    let mut r = rig();
    *r.f.0.encoder_fails.lock().unwrap() = true;
    r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    assert_eq!(*r.f.0.uploads.lock().unwrap(), [("audio/wav", 44 + 32_000)]);
}

#[test]
fn cancel_and_discard_stop_the_encoder() {
    let mut r = rig();
    r.dictate(0);
    r.cmd(Command::Cancel, 1500);
    assert_eq!(r.f.count("encoder.abort"), 1);
    r.cmd(Command::Toggle, 3000);
    r.cmd(Command::Toggle, 3100);
    assert_eq!(r.f.count("encoder.abort"), 2);
}

#[test]
fn retry_uploads_the_spooled_wav() {
    let mut r = rig();
    let id = r.dictate(0);
    r.cmd(Command::Toggle, 2000);
    r.stream(id, StreamEvent::Closed, 2100);
    r.batch(id, Err(BatchError::Status(400, String::new())), 2200);
    let spooled = r.f.0.wavs.lock().unwrap().clone();
    let mut r2 = rig();
    *r2.f.0.wavs.lock().unwrap() = spooled;
    r2.cmd(Command::Retry, 0);
    assert_eq!(*r2.f.0.uploads.lock().unwrap(), [("audio/wav", 44 + 32_000)]);
}

// ---- several Deepgram keys ----

const KA: &str = "fake-key-AAAA-11110001";
const KB: &str = "fake-key-BBBB-22220002";
const KC: &str = "fake-key-CCCC-33330003";

impl Rig {
    /// A dictation from `ms`, stopped 2 s later. Returns its id.
    fn dictation(&mut self, ms: u64) -> SessionId {
        let id = self.dictate(ms);
        self.cmd(Command::Toggle, ms + 2000);
        id
    }

    fn keys_of(&self, kind: &str) -> Vec<String> {
        self.uses().into_iter().filter(|(k, _)| *k == kind).map(|(_, key)| key).collect()
    }
}

#[test]
fn keys_rotate_one_per_dictation_and_a_dictation_keeps_its_key() {
    let mut r = rig_keys(cfg(), &[KA, KB, KC]);
    for i in 0..4u64 {
        let id = r.dictation(i * 10_000);
        r.batch(id, Ok("Hi."), i * 10_000 + 2500);
        r.ev(Event::Delivered(Some(id), Outcome::Pasted), i * 10_000 + 2600);
    }
    // The stream and pass 2 of each dictation use the same key; the next dictation takes the next.
    assert_eq!(r.keys_of("open"), [KA, KB, KC, KA]);
    assert_eq!(r.keys_of("submit"), [KA, KB, KC, KA]);
}

#[test]
fn a_pass_2_retry_stays_on_the_dictations_key() {
    let mut r = rig_keys(cfg(), &[KA, KB]);
    let id = r.dictation(0);
    r.batch(id, Err(BatchError::Status(503, String::new())), 2100);
    r.m.tick(r.at(2700));
    assert_eq!(r.keys_of("submit"), [KA, KA]);
    // A stream retry replays on the same key too.
    let id2 = r.dictate(10_000);
    r.stream(id2, StreamEvent::Failed(Failure::Other("reset".into())), 10_300);
    r.m.tick(r.at(10_600));
    assert_eq!(r.keys_of("open"), [KA, KB, KB]);
}

#[test]
fn elf_retry_of_a_spooled_recording_takes_the_next_key() {
    let mut r = rig_keys(cfg(), &[KA, KB]);
    let id = r.dictation(0);
    r.stream(id, StreamEvent::Closed, 2100);
    r.batch(id, Err(BatchError::Status(400, String::new())), 2200);
    assert!(r.f.0.wavs.lock().unwrap().contains_key(&id));
    assert_eq!(r.cmd(Command::Retry, 5000), "ok retrying 1");
    assert_eq!(r.keys_of("submit"), [KA, KB]);
}

#[test]
fn a_rejected_key_hands_the_request_to_the_next_key_at_once() {
    let mut r = rig_keys(cfg(), &[KA, KB]);
    let id = r.dictation(0);
    r.stream(id, StreamEvent::Closed, 2050);
    r.batch(id, Err(BatchError::Auth), 2100);
    // No timer, no delay: the same request is already on its way with the next key.
    assert_eq!(r.keys_of("submit"), [KA, KB]);
    r.batch(id, Ok("Worked."), 2600);
    assert_eq!(r.delivered()[0].1, "Worked. ");
    assert!(r.f.notifications().is_empty(), "no complaint while a healthy key remains");
    assert_eq!(r.keys_line(3000), "keys: 2 (1 rejected)");
    // The rejected key stays out for good: the next two dictations both use key B.
    for (i, ms) in [10_000u64, 20_000].into_iter().enumerate() {
        let d = r.dictation(ms);
        r.batch(d, Ok("x"), ms + 2500);
        assert_eq!(r.keys_of("submit").len(), 3 + i);
    }
    assert_eq!(r.keys_of("submit"), [KA, KB, KB, KB]);
}

#[test]
fn a_rate_limited_key_cools_down_for_a_minute() {
    let mut r = rig_keys(cfg(), &[KA, KB]);
    let id = r.dictation(0);
    r.stream(id, StreamEvent::Closed, 2050);
    r.batch(id, Err(BatchError::Status(429, "slow down".into())), 2100);
    assert_eq!(r.keys_of("submit"), [KA, KB]);
    r.batch(id, Ok("Hi."), 2600);
    assert_eq!(r.keys_line(3000), "keys: 2 (1 cooling down)");
    // The rotation moves on to B, then A is back once its minute is over.
    let d2 = r.dictation(10_000);
    r.batch(d2, Ok("Hi."), 12_500);
    let d3 = r.dictation(70_000);
    r.batch(d3, Ok("Hi."), 72_500);
    assert_eq!(r.keys_of("submit"), [KA, KB, KB, KA]);
    assert_eq!(r.keys_line(73_000), "keys: 2");
}

#[test]
fn an_out_of_credit_key_cools_down_for_ten_minutes() {
    let mut r = rig_keys(cfg(), &[KA, KB]);
    let id = r.dictation(0);
    r.stream(id, StreamEvent::Closed, 2050);
    r.batch(id, Err(BatchError::Status(402, "no credit".into())), 2100);
    r.batch(id, Ok("Hi."), 2600);
    let d2 = r.dictation(10_000);
    r.batch(d2, Ok("Hi."), 12_500);
    let d3 = r.dictation(70_000);
    r.batch(d3, Ok("Hi."), 72_500);
    let d4 = r.dictation(603_000);
    r.batch(d4, Ok("Hi."), 605_500);
    assert_eq!(r.keys_of("submit"), [KA, KB, KB, KB, KA]);
}

#[test]
fn a_stream_turned_away_on_connect_moves_to_the_next_key_and_replays() {
    let mut r = rig_keys(cfg(), &[KA, KB]);
    let id = r.dictate(0);
    r.stream(id, StreamEvent::Failed(Failure::Auth), 300);
    assert_eq!(r.keys_of("open"), [KA, KB]);
    assert_eq!(*r.f.0.stream_replays.lock().unwrap(), [0, 32_000], "the buffer is replayed on the new key");
    // Pass 2 follows the dictation to its new key.
    r.cmd(Command::Toggle, 2000);
    assert_eq!(r.keys_of("submit"), [KB]);
    assert!(r.f.notifications().is_empty());
}

#[test]
fn a_rate_limited_stream_moves_to_the_next_key_too() {
    let mut r = rig_keys(cfg(), &[KA, KB]);
    let id = r.dictate(0);
    r.stream(id, StreamEvent::Failed(Failure::RateLimited), 300);
    assert_eq!(r.keys_of("open"), [KA, KB]);
    r.stream(id, StreamEvent::Failed(Failure::Credit), 400);
    // Both keys are out now: nothing to move to, and the stream is given up quietly.
    assert_eq!(r.keys_of("open"), [KA, KB]);
    assert_eq!(r.f.notifications().len(), 1);
}

#[test]
fn scenario_14_every_key_out_notifies_once_and_keeps_the_audio() {
    let mut r = rig_keys(cfg(), &[KA, KB]);
    let id = r.dictation(0);
    r.stream(id, StreamEvent::Closed, 2050);
    r.batch(id, Err(BatchError::Auth), 2100);
    assert_eq!(r.keys_of("submit"), [KA, KB]);
    r.batch(id, Err(BatchError::Auth), 2200);
    r.m.tick(r.at(9000));
    assert_eq!(r.keys_of("submit").len(), 2, "no retry");
    assert_eq!(r.f.notifications(), ["Every Deepgram key was rejected. Check your keys"]);
    assert!(r.f.0.wavs.lock().unwrap().contains_key(&id));
    assert!(r.entries()[0].text.is_none());
    assert_eq!(r.keys_line(10_000), "keys: 2 (2 rejected)");

    // A later dictation still starts; it fails the old way, with one notice of its own.
    let d = r.dictation(20_000);
    r.stream(d, StreamEvent::Closed, 22_050);
    r.batch(d, Err(BatchError::Auth), 22_100);
    assert_eq!(r.keys_of("submit").len(), 3);
    assert_eq!(r.f.notifications().len(), 2);
}

#[test]
fn a_single_rejected_key_keeps_the_old_message() {
    let mut r = rig_keys(cfg(), &[KA]);
    let id = r.dictation(0);
    r.stream(id, StreamEvent::Closed, 2050);
    r.batch(id, Err(BatchError::Auth), 2100);
    assert_eq!(r.f.notifications(), ["Deepgram key rejected. Check DEEPGRAM_API_KEY"]);
}

#[test]
fn credit_gone_on_every_key_says_so_once() {
    let mut r = rig_keys(cfg(), &[KA]);
    let id = r.dictation(0);
    r.stream(id, StreamEvent::Closed, 2050);
    r.batch(id, Err(BatchError::Status(402, String::new())), 2100);
    assert_eq!(r.f.notifications().len(), 1);
    assert!(r.f.notifications()[0].contains("credit"));
    assert!(r.f.0.wavs.lock().unwrap().contains_key(&id));
}

#[test]
fn the_only_key_rate_limited_still_gets_the_old_retry() {
    let mut r = rig_keys(cfg(), &[KA]);
    let id = r.dictation(0);
    r.batch(id, Err(BatchError::Status(429, String::new())), 2100);
    r.m.tick(r.at(2700));
    assert_eq!(r.keys_of("submit"), [KA, KA]);
}

#[test]
fn the_status_reply_has_a_keys_line() {
    let mut r = rig_keys(cfg(), &[KA, KB, KC]);
    let reply = r.cmd(Command::Status, 0);
    assert_eq!(reply, "ok idle\nkeys: 3");
}

#[test]
fn log_lines_never_hold_a_key() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    let path = std::env::temp_dir().join(format!("elf-keys-log-{}.log", std::process::id()));
    INIT.call_once(|| {
        let _ = std::fs::remove_file(&path);
        crate::log::init_file(&path);
    });
    let secrets = ["SECRETKEY-alpha-555566667777", "SECRETKEY-bravo-888899990000"];
    let mut r = rig_keys(cfg(), &secrets);
    let id = r.dictation(0);
    r.stream(id, StreamEvent::Failed(Failure::Auth), 300);
    r.stream(id, StreamEvent::Closed, 2050);
    r.batch(id, Err(BatchError::Status(429, String::new())), 2100);
    r.batch(id, Err(BatchError::Auth), 2200);
    let logged = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(logged.contains("key#1"), "the scenario logged by key label: {logged}");
    for secret in secrets {
        assert!(!logged.contains(secret) && !logged.contains(&secret[10..20]), "key text in the log");
    }
    // Replies and notifications carry no key text either.
    let status = r.cmd(Command::Status, 3000);
    assert!(!status.contains("SECRET") && r.f.notifications().iter().all(|n| !n.contains("SECRET")));
    let _ = std::fs::remove_file(&path);
}
