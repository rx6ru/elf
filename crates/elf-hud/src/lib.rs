//! ELF HUD: the liquid dictation shape as an overlay.
//!
//! `run_overlay` runs the Rune event loop on the calling thread (it must be the main thread)
//! and gives a `Handle` to any thread through `on_ready`. The handle sets the phase and feeds
//! voice frames; the standalone `elf-hud` binary feeds it from its own microphone analysis,
//! `elfd` from the analysis it already makes of every captured frame.
//!
//! Each time the overlay is shown it is placed afresh on the focused monitor (or the one the
//! `[hud]` config names): sized and positioned from that monitor's logical size, and, in `auto`
//! colour, inked white or near-black after the screen strip just above it.

pub mod ink;
pub mod layout;
pub mod liquid;
pub mod mic;
pub mod settings;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use rune::{
    Align, App, AppHandle, AppSpec, Cx, DisplayId, DisplayTarget, OverlayHandle, OverlayOptions, OverlayPlacement, OverlaySpec, RunOptions,
    ScreenEdge, WindowSpec, effect, el,
};

pub use ink::Ink;
pub use mic::{BANDS, Mic, Voice};
pub use settings::{Anchor, HudSettings};

use liquid::{LIQUID_MARK, LiquidCompositor, SharedF32, SharedPhase};
use settings::{Reload, Reloader};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Phase {
    #[default]
    Listening = 0,
    Processing = 1,
    Done = 2,
    Error = 3,
}

impl Phase {
    pub fn parse(s: &str) -> Option<Phase> {
        Some(match s {
            "listening" => Phase::Listening,
            "processing" => Phase::Processing,
            "done" => Phase::Done,
            "error" => Phase::Error,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Phase::Listening => "LISTENING",
            Phase::Processing => "PROCESSING",
            Phase::Done => "DONE",
            Phase::Error => "ERROR",
        }
    }
}

/// The overlay surface, in points, before it is sized for a screen. Transparent: only the shape shows.
pub const OVERLAY: (f32, f32) = layout::BASE_SIZE;

/// How often the screen colour is looked at while the overlay is shown.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);

type Say = Arc<dyn Fn(&str) + Send + Sync>;

pub struct Options {
    /// Where the shape reads its voice from: `mic::start()` to analyse the microphone here, or
    /// `Mic::default()` and push frames with `Handle::voice`.
    pub mic: Mic,
    /// Replaces the process arguments for Rune's own flags: `Some(vec![])` keeps the host's
    /// flags away from it, `None` lets it read them.
    pub rune_args: Option<Vec<String>>,
    /// The config file holding the `[hud]` section, re-read when it changes.
    pub config: PathBuf,
    /// Overrides `hud.color` (the `--ink` flag).
    pub ink: Option<Ink>,
    /// Receives log lines ("auto colour: dark", clamped values, ...).
    pub log: Say,
    /// Receives messages for the user (a broken config file).
    pub notify: Say,
}

enum Msg {
    Show,
    Hide,
}

/// Controls the overlay from any thread.
#[derive(Clone)]
pub struct Handle {
    phase: SharedPhase,
    mic: Mic,
    to_overlay: Sender<Msg>,
    app: AppHandle<Idle>,
}

/// Placeholder state of the windowless app that owns the overlay.
struct Idle;

impl Handle {
    /// Shows the overlay in `phase`. A hidden overlay is placed afresh first.
    pub fn show(&self, phase: Phase) {
        self.phase.set(phase);
        let _ = self.to_overlay.send(Msg::Show);
    }

    pub fn hide(&self) {
        let _ = self.to_overlay.send(Msg::Hide);
    }

    /// The latest voice analysis. The shape reads it on every frame.
    pub fn voice(&self, voice: Voice) {
        self.mic.set(voice);
    }

    /// Ends the event loop, so `run_overlay` returns.
    pub fn quit(&self) {
        self.app.quit();
    }
}

/// Runs the overlay's event loop on this (the main) thread until `Handle::quit`.
///
/// `on_ready` runs on this thread once the overlay exists and receives its handle.
/// Returns `Err` instead of exiting when there is no display.
pub fn run_overlay(opts: Options, on_ready: impl FnOnce(Handle) + 'static) -> Result<(), String> {
    let phase = SharedPhase::default();
    let run_options = RunOptions { accessory: true };
    let rune_args = opts.rune_args.clone();
    let factory = move |displays: &[rune::Display]| {
        let (ink, zoom) = (SharedF32::new(0.0), SharedF32::new(1.0));
        let compositor = LiquidCompositor::new(opts.mic.clone(), phase.clone()).with_look(ink.clone(), zoom.clone());
        let mut app = App::new(AppSpec::new("ELF", (10.0, 10.0), || Idle, |_: &Idle, _: &mut Cx<Idle>| el())).windowless();
        let app_handle = app.handle();
        let view = |_: &(), _: &mut Cx<()>| effect(LIQUID_MARK).key("hud").w_full().h_full().effect_data([1.0, 0.0, 0.0, 0.0]);
        let spec = OverlaySpec { edge: ScreenEdge::Bottom, margin: 12.0, ..OverlaySpec::new("elf-hud", OVERLAY, view) };
        let overlay = app.overlay(spec.options(OverlayOptions::default()).compositor(compositor), ());

        let (to_overlay, inbox) = mpsc::channel();
        let actor = Actor {
            overlay,
            displays: displays.iter().filter_map(|d| Some((d.name.clone()?, d.id.clone()?))).collect(),
            settings: Reloader::new(opts.config),
            ink_override: opts.ink,
            ink_target: ink,
            zoom,
            dark: false,
            announced: false,
            strip: None,
            log: opts.log,
            notify: opts.notify,
        };
        std::thread::spawn(move || actor.run(inbox));
        on_ready(Handle { phase, mic: opts.mic, to_overlay, app: app_handle });
        vec![WindowSpec::from_app("elf", app)]
    };
    match rune_args {
        Some(args) => rune::try_run_windows_with_args(run_options, args, factory),
        None => rune::try_run_windows_with(run_options, factory),
    }
}

/// Owns every call into the overlay, so placing, showing and hiding happen in order.
struct Actor {
    overlay: OverlayHandle<()>,
    /// Monitor name and Rune's handle for it, to pin the overlay to a monitor.
    displays: Vec<(String, DisplayId)>,
    settings: Reloader,
    ink_override: Option<Ink>,
    /// 0 white, 1 dark: where the ink is heading.
    ink_target: SharedF32,
    zoom: SharedF32,
    /// The ink decided from the screen so far, and whether that was said in the log yet.
    dark: bool,
    announced: bool,
    /// The screen strip whose colour is sampled while shown.
    strip: Option<[f32; 4]>,
    log: Say,
    notify: Say,
}

impl Actor {
    fn run(mut self, inbox: Receiver<Msg>) {
        let mut visible = false;
        loop {
            let sampling = visible && self.mode() == Ink::Auto;
            let msg = if sampling {
                match inbox.recv_timeout(SAMPLE_EVERY) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match inbox.recv() {
                    Ok(m) => Some(m),
                    Err(_) => return,
                }
            };
            match msg {
                Some(Msg::Show) if !visible => {
                    self.place();
                    self.overlay.show();
                    visible = true;
                    self.resample();
                }
                Some(Msg::Show) => {}
                Some(Msg::Hide) => {
                    if visible {
                        self.overlay.hide();
                        visible = false;
                    }
                }
                None => self.resample(),
            }
        }
    }

    fn mode(&self) -> Ink {
        self.ink_override.unwrap_or(self.settings.current.color)
    }

    /// Re-reads the config if it changed, then puts the overlay on its monitor.
    fn place(&mut self) {
        match self.settings.check() {
            Reload::Unchanged => {}
            Reload::Applied { notes } => notes.iter().for_each(|n| (self.log)(n)),
            Reload::Rejected(e) => {
                (self.log)(&format!("config: {e}"));
                (self.notify)(&format!("ELF config: {e}. Keeping the last good HUD settings"));
            }
        }
        match self.mode() {
            Ink::White => self.ink_target.set(0.0),
            Ink::Dark => self.ink_target.set(1.0),
            Ink::Auto => self.ink_target.set(f32::from(self.dark)),
        }

        let cfg = self.settings.current.clone();
        let monitors = layout::query();
        let Some(monitor) = layout::pick(&monitors, &cfg.monitor) else {
            // Not Hyprland: no monitor facts. The default size and margin, and no colour sampling.
            self.strip = None;
            self.zoom.set(1.0);
            self.overlay.set_placement(Some(OverlayPlacement::Edge {
                edge: ScreenEdge::Bottom,
                margin: layout::margin_for(1080.0),
                align: Align::Center,
                inset: 0.0,
            }));
            self.overlay.resize(OVERLAY);
            return;
        };
        let l = layout::layout(monitor, &cfg);
        if let Some((_, id)) = self.displays.iter().find(|(name, _)| *name == monitor.name) {
            self.overlay.set_display(DisplayTarget::Id(id.clone()));
        }
        self.zoom.set(l.zoom);
        let (align, inset) = l.inset.map_or((Align::Center, 0.0), |i| (Align::Start, i));
        let edge = match l.anchor {
            Anchor::Bottom => ScreenEdge::Bottom,
            Anchor::Top => ScreenEdge::Top,
        };
        self.overlay.set_placement(Some(OverlayPlacement::Edge { edge, margin: l.margin, align, inset }));
        self.overlay.resize(l.size);
        self.strip = Some(l.strip);
    }

    /// Looks at the screen next to the overlay and moves the ink to white or dark.
    fn resample(&mut self) {
        if self.mode() != Ink::Auto {
            return;
        }
        let Some(strip) = self.strip else { return };
        let Some(luminance) = ink::sample(strip) else { return };
        let dark = ink::decide(self.dark, luminance);
        if dark != self.dark || !self.announced {
            self.announced = true;
            (self.log)(if dark { "auto colour: dark" } else { "auto colour: white" });
        }
        self.dark = dark;
        self.ink_target.set(f32::from(dark));
    }
}
