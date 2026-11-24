//! Wires the real ports to the state machine and runs `elfd`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use crate::audio::capture::ProcessCapture;
use crate::audio::encode::OpusEncoder;
use crate::config::{Config, Paths};
use crate::control::{self, BindError};
use crate::deliver::{DeliverQueue, System};
use crate::desktop::RealDesktop;
use crate::history::{FileStore, Store};
use crate::keys::KeyPool;
use crate::hud::{Hud, HudSlot, LogHud, OverlayHud};
use crate::machine::{self, Desktop, Machine, Ports};
use crate::stt::batch::DgBatch;
use crate::stt::stream::DgStream;
use crate::{clock, elog, hypr, log, vad};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// Runs the daemon. The main thread belongs to the HUD's event loop; everything else runs on
/// worker threads. Without a HUD (`no_hud`, or no display) the main thread just waits.
pub fn run(no_hud: bool) -> Result<(), String> {
    let paths = Paths::from_env();
    std::fs::create_dir_all(&paths.state_dir).map_err(|e| format!("{}: {e}", paths.state_dir.display()))?;
    log::init_file(&paths.log_file());
    let cfg = Config::load(&paths.config_file)?;
    let keys = KeyPool::new(paths.load_keys());

    let listener = control::bind(&paths.socket).map_err(|e| match e {
        BindError::AlreadyRunning => format!("elfd is already running ({})", paths.socket.display()),
        BindError::Io(e) => format!("{}: {e}", paths.socket.display()),
    })?;

    let (tx, rx) = mpsc::channel();
    let slot = HudSlot::default();
    let hud: Box<dyn Hud> = if no_hud { Box::new(LogHud) } else { Box::new(OverlayHud(slot.clone())) };
    let store = FileStore::open(&paths.state_dir, cfg.history_days).map_err(|e| format!("state dir: {e}"))?;
    let spooled = store.spooled().len();
    let ports = Ports {
        capture: Box::new(ProcessCapture::microphone(tx.clone())),
        stream: Box::new(DgStream::new(tx.clone())),
        batch: Box::new(DgBatch::new(tx.clone())),
        encoder: Box::new(OpusEncoder::default()),
        deliver: Box::new(DeliverQueue::spawn(System, cfg.paste_keys.clone(), tx.clone())),
        desktop: Box::new(RealDesktop),
        store: Box::new(store),
        hud,
        new_vad: vad::new_level_vad,
    };
    elog!("elfd start keys={} spooled_wavs={spooled}", keys.len());
    if keys.is_empty() {
        elog!("no DEEPGRAM_API_KEY: start is refused until one is configured");
    }
    hypr::leave_submap_if_stuck();

    let handler: extern "C" fn(libc::c_int) = on_signal;
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }

    std::thread::spawn(move || control::serve(listener, tx));
    let machine = Machine::new(cfg, keys, ports, clock::unix_ms);
    let finished = Arc::new(AtomicBool::new(false));
    let worker = {
        let (slot, finished) = (slot.clone(), finished.clone());
        std::thread::spawn(move || {
            machine::run(machine, rx, &STOP);
            finished.store(true, Ordering::SeqCst);
            if let Some(hud) = slot.get() {
                hud.quit();
            }
        })
    };
    if !no_hud {
        let on_ready = move |hud: elf_hud::Handle| {
            let _ = slot.set(hud.clone());
            // The machine may have ended before the overlay came up.
            if finished.load(Ordering::SeqCst) {
                hud.quit();
            }
        };
        // `Some(vec![])`: Rune must not parse elfd's own flags.
        let options = elf_hud::Options {
            mic: elf_hud::Mic::default(),
            rune_args: Some(Vec::new()),
            config: paths.config_file.clone(),
            ink: None,
            log: Arc::new(|m| elog!("hud: {m}")),
            notify: Arc::new(|m| RealDesktop.notify(m)),
        };
        match elf_hud::run_overlay(options, on_ready) {
            Ok(()) => elog!("overlay closed"),
            Err(e) => elog!("overlay unavailable ({e}); running without a HUD"),
        }
    }
    let result = worker.join().map_err(|_| "state machine thread panicked".to_string());
    let _ = std::fs::remove_file(&paths.socket);
    elog!("elfd stop");
    result
}
