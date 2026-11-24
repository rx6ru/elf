//! Config file, API key type and file locations.

use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PasteInto {
    Original,
    Focused,
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct Keyterms {
    pub always: Vec<String>,
    pub apps: HashMap<String, Vec<String>>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Config {
    pub language: String,
    pub spoken_punctuation: bool,
    pub trailing_space: bool,
    pub paste_into: PasteInto,
    pub max_seconds: u64,
    pub sounds: bool,
    pub history_days: u64,
    pub mip_opt_out: bool,
    /// Also stream audio to Deepgram while recording (pass 1). Off: only the whole recording
    /// is sent at stop, which is more accurate and costs half. The HUD shows no live text yet.
    pub live_stream: bool,
    pub keyterms: Keyterms,
    pub paste_keys: HashMap<String, String>,
    pub snippets: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: "en".into(),
            spoken_punctuation: false,
            trailing_space: true,
            paste_into: PasteInto::Original,
            max_seconds: 600,
            sounds: false,
            history_days: 30,
            mip_opt_out: true,
            live_stream: false,
            keyterms: Keyterms::default(),
            paste_keys: HashMap::new(),
            snippets: HashMap::new(),
        }
    }
}

pub const DEFAULT_PASTE_KEYS: &str = "shift+Insert";
/// Deepgram accepts at most 100 keyterms per request.
const MAX_KEYTERMS: usize = 100;

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// A missing file means all defaults; a broken file is an error.
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn keyterms_for(&self, class: &str) -> Vec<String> {
        let mut terms = self.keyterms.always.clone();
        if let Some(extra) = self.keyterms.apps.get(class) {
            terms.extend(extra.iter().cloned());
        }
        terms.truncate(MAX_KEYTERMS);
        terms
    }

    pub fn paste_keys_for(&self, class: &str) -> &str {
        self.paste_keys.get(class).map_or(DEFAULT_PASTE_KEYS, String::as_str)
    }
}

/// The Deepgram key. `Debug` never shows it.
#[derive(Clone, Default)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: String) -> Self {
        Self(key)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The last four characters, for telling keys apart in logs. A short key shows nothing, so
    /// the whole of it can never leak.
    pub fn tail(&self) -> Option<&str> {
        (self.0.chars().count() >= 12).then(|| &self.0[self.0.char_indices().rev().nth(3).map_or(0, |(i, _)| i)..])
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("ApiKey(***)")
    }
}

pub struct Paths {
    pub config_file: PathBuf,
    /// Read, in order, after the process environment, for `DEEPGRAM_API_KEY*` lines.
    pub env_files: Vec<PathBuf>,
    /// One Deepgram key per line.
    pub keys_file: PathBuf,
    pub state_dir: PathBuf,
    pub socket: PathBuf,
}

impl Paths {
    pub fn from_env() -> Paths {
        Paths::resolve(|k| std::env::var(k).ok())
    }

    pub fn resolve(get: impl Fn(&str) -> Option<String>) -> Paths {
        let home = get("HOME").unwrap_or_default();
        let dir = |var: &str, fallback: &str| {
            get(var).filter(|v| !v.is_empty()).map_or_else(|| PathBuf::from(&home).join(fallback), PathBuf::from)
        };
        let config_dir = dir("XDG_CONFIG_HOME", ".config").join("elf");
        let runtime = get("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).map_or_else(std::env::temp_dir, PathBuf::from);
        Paths {
            config_file: config_dir.join("config.toml"),
            env_files: vec![
                config_dir.join(".env"),
                PathBuf::from(".env"),
                // Development: the repo's own .env.
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env"),
            ],
            keys_file: config_dir.join("deepgram_keys"),
            state_dir: dir("XDG_STATE_HOME", ".local/state").join("elf"),
            socket: runtime.join("elf.sock"),
        }
    }

    /// Every Deepgram key from the environment, the `.env` files and the keys file, in the order
    /// they rotate.
    pub fn load_keys(&self) -> Vec<ApiKey> {
        let env: Vec<(String, String)> = std::env::vars().collect();
        crate::keys::load(&env, &self.env_files, &self.keys_file, &|p| std::fs::read_to_string(p).ok())
    }

    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("elfd.log")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_all_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.language, "en");
        assert!(c.trailing_space && !c.spoken_punctuation && !c.sounds && c.mip_opt_out);
        assert_eq!((c.max_seconds, c.history_days, c.paste_into), (600, 30, PasteInto::Original));
    }

    #[test]
    fn example_config_parses() {
        let c = Config::parse(
            r#"
            language = "multi"
            paste_into = "focused"
            max_seconds = 60
            [keyterms]
            always = ["ELF"]
            [keyterms.apps]
            kitty = ["cargo"]
            [paste_keys]
            "org.wezfurlong.wezterm" = "ctrl+shift+v"
            [snippets]
            "insert my email" = "someone@example.com"
            "#,
        )
        .unwrap();
        assert_eq!((c.language.as_str(), c.paste_into, c.max_seconds), ("multi", PasteInto::Focused, 60));
        assert_eq!(c.keyterms_for("kitty"), ["ELF", "cargo"]);
        assert_eq!(c.keyterms_for("other"), ["ELF"]);
        assert_eq!(c.paste_keys_for("org.wezfurlong.wezterm"), "ctrl+shift+v");
        assert_eq!(c.paste_keys_for("kitty"), "shift+Insert");
        assert_eq!(c.snippets["insert my email"], "someone@example.com");
    }

    /// `config.example.toml` documents every key with its default value.
    #[test]
    fn the_example_config_parses_to_the_defaults() {
        let example = include_str!("../config.example.toml");
        assert_eq!(Config::parse(example).unwrap().language, "en");
        let c = Config::parse(example).unwrap();
        assert_eq!((c.spoken_punctuation, c.trailing_space, c.max_seconds, c.history_days), (false, true, 600, 30));
        assert!(c.keyterms.always.contains(&"ELF".to_string()) && !c.snippets.is_empty());
        let (hud, notes) = elf_hud::settings::parse(example).unwrap();
        assert_eq!(hud, elf_hud::HudSettings::default());
        assert!(notes.is_empty());
    }

    #[test]
    fn a_hud_section_does_not_trouble_the_daemon_config() {
        let c = Config::parse("language = \"multi\"\n[hud]\nsize = 1.5\nanchor = \"top\"\n").unwrap();
        assert_eq!(c.language, "multi");
    }

    #[test]
    fn bad_values_are_errors() {
        assert!(Config::parse("paste_into = \"nowhere\"").is_err());
        assert!(Config::parse("max_seconds = \"x\"").is_err());
    }

    #[test]
    fn keyterms_are_capped_at_100() {
        let mut c = Config::default();
        c.keyterms.always = (0..150).map(|i| i.to_string()).collect();
        assert_eq!(c.keyterms_for("x").len(), 100);
    }

    #[test]
    fn key_never_prints() {
        assert_eq!(format!("{:?}", ApiKey("secret".into())), "ApiKey(***)");
    }

    #[test]
    fn paths_follow_xdg() {
        let p = Paths::resolve(|k| match k {
            "HOME" => Some("/home/u".into()),
            "XDG_RUNTIME_DIR" => Some("/run/user/1000".into()),
            _ => None,
        });
        assert_eq!(p.config_file, Path::new("/home/u/.config/elf/config.toml"));
        assert_eq!(p.state_dir, Path::new("/home/u/.local/state/elf"));
        assert_eq!(p.keys_file, Path::new("/home/u/.config/elf/deepgram_keys"));
        assert_eq!(p.socket, Path::new("/run/user/1000/elf.sock"));
        let p = Paths::resolve(|k| (k == "XDG_STATE_HOME").then(|| "/s".into()));
        assert_eq!(p.state_dir, Path::new("/s/elf"));
    }
}
