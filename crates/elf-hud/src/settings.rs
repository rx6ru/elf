//! The `[hud]` section of `~/.config/elf/config.toml`, and reloading it when the file changes.
//! Every key is optional; an omitted key keeps the automatic behaviour.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::ink::Ink;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Anchor {
    #[default]
    Bottom,
    Top,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct HudSettings {
    pub color: Ink,
    pub anchor: Anchor,
    /// Shift from the centre, as a fraction of the monitor's logical width.
    pub x_offset: f32,
    /// Distance from the anchor edge, as a fraction of the monitor's logical height.
    /// `None`: 0.065, but never under 24 or over 160 logical pixels.
    pub y_offset: Option<f32>,
    /// Multiplier on the automatic size.
    pub size: f32,
    /// How fast the size grows with the screen: 1 is in step with it, 0 is a fixed size.
    pub scale_exponent: f32,
    /// `"focused"` or a monitor name such as `"eDP-1"`.
    pub monitor: String,
}

pub const DEFAULT_EXPONENT: f32 = 0.5;
pub const FOCUSED: &str = "focused";

impl Default for HudSettings {
    fn default() -> Self {
        Self {
            color: Ink::Auto,
            anchor: Anchor::Bottom,
            x_offset: 0.0,
            y_offset: None,
            size: 1.0,
            scale_exponent: DEFAULT_EXPONENT,
            monitor: FOCUSED.into(),
        }
    }
}

#[derive(Deserialize, Default)]
struct File {
    #[serde(default)]
    hud: HudSettings,
}

/// Pulls `value` into `lo..=hi`, noting it when it had to move. Not-a-number resets to `default`.
fn clamp(name: &str, value: &mut f32, lo: f32, hi: f32, default: f32, notes: &mut Vec<String>) {
    let fixed = if value.is_finite() { value.clamp(lo, hi) } else { default };
    if fixed != *value {
        notes.push(format!("hud.{name} = {value} is outside {lo}..{hi}, using {fixed}"));
        *value = fixed;
    }
}

impl HudSettings {
    /// The settings with silly values pulled into range, and a note for each one that moved.
    pub fn clamped(mut self) -> (Self, Vec<String>) {
        let mut notes = Vec::new();
        clamp("x_offset", &mut self.x_offset, -0.5, 0.5, 0.0, &mut notes);
        clamp("size", &mut self.size, 0.5, 2.0, 1.0, &mut notes);
        clamp("scale_exponent", &mut self.scale_exponent, 0.0, 1.0, DEFAULT_EXPONENT, &mut notes);
        if let Some(y) = &mut self.y_offset {
            clamp("y_offset", y, 0.0, 0.5, 0.065, &mut notes);
        }
        (self, notes)
    }
}

/// Parses the file's text. The error names the line of the first mistake.
pub fn parse(text: &str) -> Result<(HudSettings, Vec<String>), String> {
    match toml::from_str::<File>(text) {
        Ok(file) => Ok(file.hud.clamped()),
        Err(e) => {
            let line = e.span().map_or(0, |s| text[..s.start.min(text.len())].matches('\n').count() + 1);
            let message = e.message().lines().next().unwrap_or("invalid").to_string();
            Err(if line > 0 { format!("line {line}: {message}") } else { message })
        }
    }
}

/// `$XDG_CONFIG_HOME/elf/config.toml`, like the daemon's.
pub fn default_path() -> PathBuf {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))
        .unwrap_or_default();
    dir.join("elf").join("config.toml")
}

/// What `Reloader::check` found.
#[derive(Debug, PartialEq)]
pub enum Reload {
    Unchanged,
    /// New settings are in force. `notes` says which values were clamped.
    Applied { notes: Vec<String> },
    /// The file is broken; the last good settings stay.
    Rejected(String),
}

/// Keeps the current settings and re-reads the file only when its modification time changes.
pub struct Reloader {
    path: PathBuf,
    seen: Option<Option<SystemTime>>,
    pub current: HudSettings,
}

impl Reloader {
    pub fn new(path: PathBuf) -> Self {
        Self { path, seen: None, current: HudSettings::default() }
    }

    /// Looks at the file's mtime (`None` when it is missing) and reloads when it differs from the
    /// last look. A broken file counts as seen, so it is reported once, not on every call.
    pub fn check_with(&mut self, mtime: Option<SystemTime>, read: impl FnOnce() -> Option<String>) -> Reload {
        if self.seen == Some(mtime) {
            return Reload::Unchanged;
        }
        self.seen = Some(mtime);
        let Some(text) = mtime.and_then(|_| read()) else {
            self.current = HudSettings::default();
            return Reload::Applied { notes: Vec::new() };
        };
        match parse(&text) {
            Ok((settings, notes)) => {
                self.current = settings;
                Reload::Applied { notes }
            }
            Err(e) => Reload::Rejected(e),
        }
    }

    pub fn check(&mut self) -> Reload {
        let mtime = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        let path = self.path.clone();
        self.check_with(mtime, || std::fs::read_to_string(path).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn no_hud_section_gives_the_defaults() {
        assert_eq!(parse("").unwrap(), (HudSettings::default(), vec![]));
        assert_eq!(parse("language = \"en\"\n[keyterms]\nalways = [\"x\"]").unwrap().0, HudSettings::default());
        let d = HudSettings::default();
        assert_eq!((d.color, d.anchor, d.x_offset, d.y_offset, d.size, d.scale_exponent), (Ink::Auto, Anchor::Bottom, 0.0, None, 1.0, 0.5));
        assert_eq!(d.monitor, "focused");
    }

    #[test]
    fn every_key_parses() {
        let (s, notes) = parse(
            "[hud]\ncolor = \"dark\"\nanchor = \"top\"\nx_offset = 0.1\ny_offset = 0.2\nsize = 1.5\nscale_exponent = 1\nmonitor = \"eDP-1\"\n",
        )
        .unwrap();
        assert!(notes.is_empty());
        assert_eq!(
            s,
            HudSettings { color: Ink::Dark, anchor: Anchor::Top, x_offset: 0.1, y_offset: Some(0.2), size: 1.5, scale_exponent: 1.0, monitor: "eDP-1".into() }
        );
    }

    #[test]
    fn a_partial_section_keeps_the_rest_automatic() {
        let (s, _) = parse("[hud]\nsize = 1.2").unwrap();
        assert_eq!((s.size, s.y_offset, s.monitor.as_str()), (1.2, None, "focused"));
    }

    #[test]
    fn silly_values_are_clamped_and_noted() {
        let (s, notes) = parse("[hud]\nx_offset = 3\ny_offset = -1\nsize = 9\nscale_exponent = 2").unwrap();
        assert_eq!((s.x_offset, s.y_offset, s.size, s.scale_exponent), (0.5, Some(0.0), 2.0, 1.0));
        assert_eq!(notes.len(), 4);
        assert!(notes[0].starts_with("hud.x_offset"));
        let (s, notes) = parse("[hud]\nsize = 0.1\nx_offset = -0.9").unwrap();
        assert_eq!((s.size, s.x_offset), (0.5, -0.5));
        assert_eq!(notes.len(), 2);
        assert!(parse("[hud]\nsize = 1.0").unwrap().1.is_empty());
    }

    #[test]
    fn not_a_number_falls_back_to_the_default() {
        let (s, notes) = parse("[hud]\nsize = nan").unwrap();
        assert_eq!(s.size, 1.0);
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn errors_name_the_line() {
        let e = parse("language = \"en\"\n[hud]\nsize = 1.0\nanchor = \"left\"\n").unwrap_err();
        assert!(e.starts_with("line 4:"), "{e}");
        let e = parse("[hud\nsize = 1").unwrap_err();
        assert!(e.starts_with("line 1:"), "{e}");
        assert!(parse("[hud]\ncolor = \"red\"").is_err());
        assert!(parse("[hud]\nsize = \"big\"").is_err());
    }

    #[test]
    fn reload_only_when_the_mtime_changes() {
        let mut r = Reloader::new(PathBuf::from("/nowhere"));
        let reads = std::cell::Cell::new(0);
        let read = |text: &'static str| {
            let reads = &reads;
            move || {
                reads.set(reads.get() + 1);
                Some(text.to_string())
            }
        };
        // First look: no file at all means the defaults.
        assert_eq!(r.check_with(None, read("")), Reload::Applied { notes: vec![] });
        assert_eq!(r.check_with(None, read("")), Reload::Unchanged);
        assert_eq!(reads.get(), 0);
        assert_eq!(r.check_with(t(10), read("[hud]\nsize = 1.5")), Reload::Applied { notes: vec![] });
        assert_eq!(r.current.size, 1.5);
        assert_eq!(r.check_with(t(10), read("[hud]\nsize = 1.8")), Reload::Unchanged);
        assert_eq!(r.current.size, 1.5);
        assert_eq!(reads.get(), 1);
        assert!(matches!(r.check_with(t(20), read("[hud]\nsize = 9")), Reload::Applied { notes } if notes.len() == 1));
        assert_eq!(r.current.size, 2.0);
    }

    #[test]
    fn a_broken_file_keeps_the_last_good_settings_and_is_reported_once() {
        let mut r = Reloader::new(PathBuf::from("/nowhere"));
        r.check_with(t(1), || Some("[hud]\nsize = 1.3".into()));
        let broken = || Some("[hud]\nsize = oops".to_string());
        assert!(matches!(r.check_with(t(2), broken), Reload::Rejected(e) if e.starts_with("line 2:")));
        assert_eq!(r.current.size, 1.3);
        assert_eq!(r.check_with(t(2), broken), Reload::Unchanged);
        // Fixing the file applies again; deleting it goes back to the defaults.
        assert!(matches!(r.check_with(t(3), || Some("[hud]\nsize = 1.1".into())), Reload::Applied { .. }));
        assert_eq!(r.current.size, 1.1);
        r.check_with(None, || None);
        assert_eq!(r.current, HudSettings::default());
    }
}
