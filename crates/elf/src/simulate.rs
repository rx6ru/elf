//! `elfd --simulate <wav> --dry-run`: one dictation through the real pipeline, with a recording
//! played at real-time speed instead of the microphone. Nothing is pasted and no key is sent.

use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use crate::audio::capture::ProcessCapture;
use crate::audio::encode::OpusEncoder;
use crate::config::{Config, Paths};
use crate::control::Command;
use crate::deliver::{DeliverRequest, Outcome};
use crate::desktop::Sound;
use crate::history::{FileStore, Store};
use crate::keys::KeyPool;
use crate::hud::LogHud;
use crate::hypr::Window;
use crate::machine::{self, Deliver, Desktop, Event, Machine, Ports, SessionId};
use crate::stt::batch::DgBatch;
use crate::stt::stream::DgStream;
use crate::{clock, log, vad};

struct DryDeliver(Sender<Event>);

impl Deliver for DryDeliver {
    fn deliver(&self, id: Option<SessionId>, req: DeliverRequest) {
        println!("WOULD DELIVER into {:?}: {:?}", req.window.as_ref().map(|w| &w.class), req.text);
        let _ = self.0.send(Event::Delivered(id, Outcome::Pasted));
    }
}

struct DryDesktop;

impl Desktop for DryDesktop {
    fn active_window(&self) -> Option<Window> {
        Some(Window { address: "0xsimulated".into(), class: "simulate".into() })
    }
    fn set_submap(&self, _on: bool) {}
    fn notify(&self, message: &str) {
        println!("WOULD NOTIFY: {message}");
    }
    fn sound(&self, _sound: Sound) {}
}

pub fn run(wav: &Path) -> Result<(), String> {
    if !wav.is_file() {
        return Err(format!("{}: no such file", wav.display()));
    }
    let paths = Paths::from_env();
    let keys = KeyPool::new(paths.load_keys());
    if keys.is_empty() {
        return Err("no DEEPGRAM_API_KEY found".into());
    }
    let cfg = Config::load(&paths.config_file)?;
    log::init_stderr();

    let state = std::env::temp_dir().join(format!("elf-simulate-{}", std::process::id()));
    let store = FileStore::open(&state, cfg.history_days).map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel();
    let ports = Ports {
        capture: Box::new(ProcessCapture::file(tx.clone(), wav)),
        stream: Box::new(DgStream::new(tx.clone())),
        batch: Box::new(DgBatch::new(tx.clone())),
        encoder: Box::new(OpusEncoder::default()),
        deliver: Box::new(DryDeliver(tx.clone())),
        desktop: Box::new(DryDesktop),
        store: Box::new(store),
        hud: Box::new(LogHud),
        new_vad: vad::new_level_vad,
    };
    let machine = Machine::new(cfg, keys, ports, clock::unix_ms);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let worker = std::thread::spawn(move || machine::run(machine, rx, &stop));

    let ask = |cmd| -> Result<String, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(Event::Command(cmd, reply_tx)).map_err(|e| e.to_string())?;
        reply_rx.recv_timeout(Duration::from_secs(5)).map_err(|e| e.to_string())
    };
    let began = Instant::now();
    println!("start: {}", ask(Command::Start)?);
    // The recording ends by itself when the file does; wait until the dictation is fully processed.
    std::thread::sleep(Duration::from_millis(500));
    while ask(Command::Status)?.lines().next() != Some("ok idle") {
        if began.elapsed() > Duration::from_secs(180) {
            return Err("simulation did not finish in 180 s".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("finished after {} ms", began.elapsed().as_millis());

    let store = FileStore::open(&state, 30).map_err(|e| e.to_string())?;
    match store.recent(1).first() {
        Some(e) => println!(
            "history: audio_ms={} stop_to_text_ms={} chosen={:?}\n  pass1: {:?}\n  pass2: {:?}\n  text:  {:?}\n  error: {:?}",
            e.ms_audio, e.ms_to_text, e.chosen, e.pass1, e.pass2, e.text, e.error
        ),
        None => println!("history: no entry (recording discarded)"),
    }
    let _ = ask(Command::Quit);
    let _ = worker.join();
    let _ = std::fs::remove_dir_all(&state);
    Ok(())
}
