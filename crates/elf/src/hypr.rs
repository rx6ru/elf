//! Hyprland (0.57, Lua dispatch) through `hyprctl`.

use serde::Deserialize;
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Window {
    /// `0x...` as printed by `hyprctl -j activewindow`.
    pub address: String,
    #[serde(default)]
    pub class: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DispatchError {
    WindowNotFound,
    Failed(String),
}

/// `hyprctl -j activewindow` prints `{}` when nothing is focused.
pub fn parse_window(json: &str) -> Option<Window> {
    let w: Window = serde_json::from_str(json).ok()?;
    let hex = w.address.strip_prefix("0x")?;
    (!hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit())).then_some(w)
}

/// `hyprctl dispatch` prints `ok`, or `warning: ...` / `error: ...` with exit code 0.
pub fn parse_dispatch_output(out: &str) -> Result<(), DispatchError> {
    let lower = out.to_lowercase();
    if lower.contains("window not found") {
        Err(DispatchError::WindowNotFound)
    } else if lower.contains("error") || lower.contains("warning") {
        Err(DispatchError::Failed(out.trim().to_string()))
    } else {
        Ok(())
    }
}

/// How Lua dispatchers name a window. On Hyprland 0.57.2 a bare `0x...` fails
/// with "window not found"; `address:0x...` works.
pub fn window_selector(w: &Window) -> String {
    format!("address:{}", w.address)
}

/// `"ctrl+shift+v"` becomes `("CTRL SHIFT", "v")`; a key with no modifier gives `("", key)`.
pub fn parse_keys(keys: &str) -> Result<(String, String), String> {
    let mut parts: Vec<&str> = keys.split('+').map(str::trim).collect();
    let key = parts.pop().filter(|k| !k.is_empty()).ok_or_else(|| format!("bad paste keys {keys:?}"))?;
    let mods = parts
        .iter()
        .map(|m| match m.to_lowercase().as_str() {
            "ctrl" | "control" => Ok("CTRL"),
            "shift" => Ok("SHIFT"),
            "alt" => Ok("ALT"),
            "super" | "mod4" | "win" => Ok("SUPER"),
            _ => Err(format!("bad modifier {m:?} in {keys:?}")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("bad key {key:?} in {keys:?}"));
    }
    Ok((mods.join(" "), key.to_string()))
}

pub fn lua_focus(w: &Window) -> String {
    format!("hl.dsp.focus({{ window = '{}' }})", window_selector(w))
}

pub fn lua_send_shortcut(mods: &str, key: &str, w: &Window) -> String {
    format!("hl.dsp.send_shortcut({{ mods = '{mods}', key = '{key}', window = '{}' }})", window_selector(w))
}

/// Entering the `elf` submap makes Esc cancel; `reset` leaves every submap.
pub fn lua_submap(on: bool) -> String {
    format!("hl.dsp.submap(\"{}\")", if on { "elf" } else { "reset" })
}

pub fn active_window() -> Option<Window> {
    let out = Command::new("hyprctl").args(["-j", "activewindow"]).output().ok()?;
    parse_window(&String::from_utf8_lossy(&out.stdout))
}

pub fn dispatch(lua: &str) -> Result<(), DispatchError> {
    let out = Command::new("hyprctl")
        .args(["dispatch", lua])
        .output()
        .map_err(|e| DispatchError::Failed(format!("hyprctl: {e}")))?;
    if !out.status.success() {
        return Err(DispatchError::Failed(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    parse_dispatch_output(&String::from_utf8_lossy(&out.stdout))
}

pub fn set_submap(on: bool) {
    if let Err(e) = dispatch(&lua_submap(on)) {
        crate::elog!("submap {on}: {e:?}");
    }
}

/// After a crash the `elf` submap can be left on, which would block every other bind.
pub fn leave_submap_if_stuck() {
    let Ok(out) = Command::new("hyprctl").arg("submap").output() else { return };
    if String::from_utf8_lossy(&out.stdout).trim() == "elf" {
        set_submap(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_from_hyprctl_json() {
        let w = parse_window(r#"{"address":"0x5581abc","class":"kitty","title":"x","pid":1}"#).unwrap();
        assert_eq!(w, Window { address: "0x5581abc".into(), class: "kitty".into() });
        assert_eq!(parse_window("{}"), None);
        assert_eq!(parse_window(r#"{"address":"0x1'; evil","class":"k"}"#), None);
        assert_eq!(parse_window("not json"), None);
    }

    #[test]
    fn dispatch_output_is_parsed_not_the_exit_code() {
        assert_eq!(parse_dispatch_output("ok\n"), Ok(()));
        assert_eq!(parse_dispatch_output("warning: window not found\n"), Err(DispatchError::WindowNotFound));
        assert!(matches!(parse_dispatch_output("error: bad table"), Err(DispatchError::Failed(_))));
    }

    #[test]
    fn selector_has_the_address_prefix() {
        let w = Window { address: "0xabc".into(), class: "k".into() };
        assert_eq!(window_selector(&w), "address:0xabc");
        assert_eq!(lua_focus(&w), "hl.dsp.focus({ window = 'address:0xabc' })");
        assert_eq!(
            lua_send_shortcut("SHIFT", "Insert", &w),
            "hl.dsp.send_shortcut({ mods = 'SHIFT', key = 'Insert', window = 'address:0xabc' })"
        );
    }

    #[test]
    fn key_strings_split_into_mods_and_key() {
        assert_eq!(parse_keys("shift+Insert"), Ok(("SHIFT".into(), "Insert".into())));
        assert_eq!(parse_keys("ctrl+shift+v"), Ok(("CTRL SHIFT".into(), "v".into())));
        assert_eq!(parse_keys("F5"), Ok((String::new(), "F5".into())));
        assert!(parse_keys("hyper+v").is_err());
        assert!(parse_keys("ctrl+").is_err());
        assert!(parse_keys("ctrl+v'").is_err());
    }

    #[test]
    fn submap_calls() {
        assert_eq!(lua_submap(true), "hl.dsp.submap(\"elf\")");
        assert_eq!(lua_submap(false), "hl.dsp.submap(\"reset\")");
    }
}
