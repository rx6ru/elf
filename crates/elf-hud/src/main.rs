//! ELF HUD on its own: the dictation shape driven by your microphone, for trying out the look and
//! the motion without running the daemon.
//!
//!   elf-hud                                # the real overlay, cycling through a dictation
//!   elf-hud --hold listening               # stay in one phase (listening|processing|done|error)
//!   elf-hud --gallery                      # a window at 2x, on a dark and a light background
//!   elf-hud --render-strip DIR --voice FILE [--bg dark|light|mid] [--every N] [--end done|error]
//!                                             # off-screen frames of a scripted dictation
//!   ... --ink auto|white|dark                 # ink colour; default: the [hud] color of the config, else auto
//!   elf-hud --voice-debug                  # print what the mic analysis hears
//!   ... --seconds 30                          # quit after 30 s

use std::sync::Arc;
use std::time::Duration;

use elf_hud::liquid::{self, LIQUID_MARK, LiquidCompositor, SharedPhase};
use elf_hud::settings::{self, Reload, Reloader};
use elf_hud::{Ink, OVERLAY, Options, Phase, mic};
use rune::tok::type_;
use rune::{App, AppSpec, Color, Cx, El, RunOptions, WindowSpec, col, effect, row, text};

const ZOOM: f32 = 2.0;

/// The `color` of the config's `[hud]` section, for the off-screen strips.
fn config_color() -> Ink {
    let mut reloader = Reloader::new(settings::default_path());
    if let Reload::Rejected(e) = reloader.check() {
        eprintln!("config: {e}");
    }
    reloader.current.color
}

fn sleep(ms: u64) { std::thread::sleep(Duration::from_millis(ms)); }

/// One dictation's worth of phases: listen, process, then done (or error every third time).
fn cycle(n: usize, mut set: impl FnMut(Option<Phase>)) {
    set(Some(Phase::Listening));
    sleep(6500);
    set(Some(Phase::Processing));
    sleep(3800);
    if n % 3 == 2 {
        set(Some(Phase::Error));
        sleep(1800);
    } else {
        set(Some(Phase::Done));
        sleep(1500);
    }
    set(None);
    sleep(1200);
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

#[derive(Default)]
struct Gallery { phase: Phase }

fn gallery_view(s: &Gallery, _: &mut Cx<Gallery>) -> El {
    let dim = Color { r: 0.25, g: 0.25, b: 0.25, a: 1.0 };
    let cell = |key: &str, bg: Color| {
        row().padding(18.0).radius(14.0).fill(bg)
            .child(effect(LIQUID_MARK).key(key.to_string()).size(OVERLAY.0 * ZOOM, OVERLAY.1 * ZOOM).effect_data([ZOOM, 0.0, 0.0, 0.0]))
    };
    col().w_full().h_full().center().gap(18.0).fill(Color { r: 0.08, g: 0.08, b: 0.08, a: 1.0 })
        .child(text(s.phase.label()).font(type_::MONO_CAPTION).color(dim))
        .child(cell("dark", Color { r: 0.004, g: 0.005, b: 0.006, a: 1.0 }))
        .child(cell("light", Color { r: 0.62, g: 0.6, b: 0.55, a: 1.0 }))
}

fn main() {
    let ink = arg("--ink").map(|v| Ink::parse(&v).unwrap_or_else(|| panic!("--ink takes auto|white|dark, not {v}")));
    let hold = arg("--hold").map(|p| Phase::parse(&p).unwrap_or_else(|| panic!("--hold takes listening|processing|done|error, not {p}")));
    let quit_after = arg("--seconds").and_then(|s| s.parse::<u64>().ok());
    let gallery = std::env::args().any(|a| a == "--gallery");
    if let Some(dir) = arg("--render-strip") {
        let file = arg("--voice").unwrap_or_else(|| panic!("--render-strip needs --voice <recording>"));
        let bg = arg("--bg").unwrap_or_default();
        let every = arg("--every").and_then(|n| n.parse().ok()).unwrap_or(12);
        let end = arg("--end").and_then(|p| Phase::parse(&p)).unwrap_or(Phase::Done);
        let frames = liquid::render_strip(std::path::Path::new(&dir), &mic::analyse_file(&file), &bg, every, end, ink.unwrap_or(config_color()));
        println!("{} frames in {dir}", frames.len());
        return;
    }
    let level = mic::start();
    if std::env::args().any(|a| a == "--voice-debug") {
        loop {
            sleep(100);
            let v = level.get();
            let bars: String = v.bands.iter().map(|b| [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][(b * 8.0).round() as usize]).collect();
            println!("{} activity {:4.2}  level {:4.2}  bands |{bars}|  onsets {:4}", if v.active { "SPEECH" } else { "quiet " }, v.activity, v.level, v.onsets);
        }
    }
    if !gallery {
        let say: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|m| eprintln!("{m}"));
        let options = Options { mic: level, rune_args: None, config: settings::default_path(), ink, log: say.clone(), notify: say };
        let result = elf_hud::run_overlay(options, move |hud| {
            if let Some(secs) = quit_after {
                let h = hud.clone();
                std::thread::spawn(move || { sleep(secs * 1000); h.quit(); });
            }
            std::thread::spawn(move || {
                if let Some(p) = hold { hud.show(p); return; }
                for n in 0.. {
                    cycle(n, |p| match p {
                        Some(p) => hud.show(p),
                        None => hud.hide(),
                    });
                }
            });
        });
        if let Err(e) = result {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }

    let phase = SharedPhase::default();
    rune::run_windows_with(RunOptions { accessory: false }, move |_| {
        let compositor = LiquidCompositor::new(level.clone(), phase.clone());
        let phase = phase.clone();
        let size = (OVERLAY.0 * ZOOM + 100.0, 2.0 * OVERLAY.1 * ZOOM + 180.0);
        let app = App::new(AppSpec::new("ELF HUD", size, Gallery::default, gallery_view)).compositor(compositor);
        let handle = app.handle();
        if let Some(secs) = quit_after {
            let h = handle.clone();
            std::thread::spawn(move || { sleep(secs * 1000); h.quit(); });
        }
        std::thread::spawn(move || {
            let show = |p: Phase| { phase.set(p); handle.update(move |s, _| s.phase = p); };
            if let Some(p) = hold { show(p); return; }
            for n in 0.. { cycle(n, |p| if let Some(p) = p { show(p) }); }
        });
        vec![WindowSpec::from_app("gallery", app)]
    });
}
