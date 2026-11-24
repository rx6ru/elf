//! Delivery: put the text on the clipboard, then paste it into the target window.
//!
//! On Hyprland 0.57.2 a paste into an unfocused window gets no clipboard data
//! (Wayland only serves the focused client), so the target is focused first and the previous
//! window is focused again afterwards.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use crate::config::DEFAULT_PASTE_KEYS;
use crate::hypr::{self, DispatchError, Window};
use crate::machine::{Deliver, Event, SessionId};

/// The app reads the clipboard asynchronously after the key arrives.
const PASTE_SETTLE: Duration = Duration::from_millis(120);

pub struct DeliverRequest {
    pub text: String,
    /// `None` pastes into whatever is focused now.
    pub window: Option<Window>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Pasted,
    /// Text is on the clipboard but no key was sent.
    CopiedOnly(Reason),
    Failed(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reason {
    TargetClosed,
    NothingFocused,
}

/// What `deliver` needs from the system. Fakes replace it in tests.
pub trait Backend {
    fn copy(&self, text: &str, primary: bool) -> Result<(), String>;
    fn active_window(&self) -> Option<Window>;
    fn dispatch(&self, lua: &str) -> Result<(), DispatchError>;
    fn sleep(&self, d: Duration);
}

pub struct System;

impl Backend for System {
    fn copy(&self, text: &str, primary: bool) -> Result<(), String> {
        let mut cmd = Command::new("wl-copy");
        if primary {
            cmd.arg("--primary");
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("wl-copy: {e}"))?;
        child.stdin.take().expect("piped stdin").write_all(text.as_bytes()).map_err(|e| format!("wl-copy: {e}"))?;
        match child.wait() {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(format!("wl-copy exited with {s}")),
            Err(e) => Err(format!("wl-copy: {e}")),
        }
    }

    fn active_window(&self) -> Option<Window> {
        hypr::active_window()
    }

    fn dispatch(&self, lua: &str) -> Result<(), DispatchError> {
        hypr::dispatch(lua)
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

pub fn deliver(backend: &dyn Backend, paste_keys: &HashMap<String, String>, req: &DeliverRequest) -> Outcome {
    if let Err(e) = backend.copy(&req.text, false) {
        return Outcome::Failed(e);
    }
    if let Err(e) = backend.copy(&req.text, true) {
        crate::elog!("primary selection: {e}");
    }

    let prev = backend.active_window();
    let Some(target) = req.window.clone().or_else(|| prev.clone()) else {
        return Outcome::CopiedOnly(Reason::NothingFocused);
    };
    let keys = paste_keys.get(&target.class).map_or(DEFAULT_PASTE_KEYS, String::as_str);
    let (mods, key) = match hypr::parse_keys(keys) {
        Ok(k) => k,
        Err(e) => return Outcome::Failed(e),
    };

    let moved = prev.as_ref().is_none_or(|p| p.address != target.address);
    if moved {
        match backend.dispatch(&hypr::lua_focus(&target)) {
            Ok(()) => {}
            Err(DispatchError::WindowNotFound) => return Outcome::CopiedOnly(Reason::TargetClosed),
            Err(DispatchError::Failed(e)) => return Outcome::Failed(e),
        }
    }
    let sent = backend.dispatch(&hypr::lua_send_shortcut(&mods, &key, &target));
    if moved {
        backend.sleep(PASTE_SETTLE);
        if let Some(prev) = &prev {
            // The old window may be gone; the paste has happened either way.
            let _ = backend.dispatch(&hypr::lua_focus(prev));
        }
    }
    match sent {
        Ok(()) => Outcome::Pasted,
        Err(DispatchError::WindowNotFound) => Outcome::CopiedOnly(Reason::TargetClosed),
        Err(DispatchError::Failed(e)) => Outcome::Failed(e),
    }
}

/// One worker thread, so two dictations never fight over the clipboard.
pub struct DeliverQueue {
    tx: Sender<(Option<SessionId>, DeliverRequest)>,
}

impl DeliverQueue {
    pub fn spawn(
        backend: impl Backend + Send + 'static,
        paste_keys: HashMap<String, String>,
        events: Sender<Event>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<(Option<SessionId>, DeliverRequest)>();
        std::thread::spawn(move || {
            for (id, req) in rx {
                let outcome = deliver(&backend, &paste_keys, &req);
                if events.send(Event::Delivered(id, outcome)).is_err() {
                    break;
                }
            }
        });
        Self { tx }
    }
}

impl Deliver for DeliverQueue {
    fn deliver(&self, id: Option<SessionId>, req: DeliverRequest) {
        let _ = self.tx.send((id, req));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        active: Option<Window>,
        calls: RefCell<Vec<String>>,
        closed: Vec<String>,
        copy_fails: bool,
    }

    fn win(addr: &str, class: &str) -> Window {
        Window { address: addr.into(), class: class.into() }
    }

    impl Fake {
        fn new(active: Option<Window>) -> Self {
            Fake { active, calls: RefCell::new(vec![]), closed: vec![], copy_fails: false }
        }
    }

    impl Backend for Fake {
        fn copy(&self, text: &str, primary: bool) -> Result<(), String> {
            if self.copy_fails {
                return Err("no wayland".into());
            }
            self.calls.borrow_mut().push(format!("copy {text:?} primary={primary}"));
            Ok(())
        }
        fn active_window(&self) -> Option<Window> {
            self.active.clone()
        }
        fn dispatch(&self, lua: &str) -> Result<(), DispatchError> {
            self.calls.borrow_mut().push(lua.to_string());
            if self.closed.iter().any(|a| lua.contains(&format!("address:{a}'"))) {
                return Err(DispatchError::WindowNotFound);
            }
            Ok(())
        }
        fn sleep(&self, d: Duration) {
            self.calls.borrow_mut().push(format!("sleep {}", d.as_millis()));
        }
    }

    fn req(text: &str, w: Option<Window>) -> DeliverRequest {
        DeliverRequest { text: text.into(), window: w }
    }

    #[test]
    fn paste_into_the_focused_target_sends_one_shortcut() {
        let f = Fake::new(Some(win("0xa", "kitty")));
        let out = deliver(&f, &HashMap::new(), &req("hi ", Some(win("0xa", "kitty"))));
        assert_eq!(out, Outcome::Pasted);
        assert_eq!(
            *f.calls.borrow(),
            [
                "copy \"hi \" primary=false",
                "copy \"hi \" primary=true",
                "hl.dsp.send_shortcut({ mods = 'SHIFT', key = 'Insert', window = 'address:0xa' })",
            ]
        );
    }

    #[test]
    fn scenario_19_unfocused_target_is_focused_then_focus_returns() {
        let f = Fake::new(Some(win("0xb", "firefox")));
        let out = deliver(&f, &HashMap::new(), &req("x", Some(win("0xa", "kitty"))));
        assert_eq!(out, Outcome::Pasted);
        let calls = f.calls.borrow();
        assert_eq!(calls[2], "hl.dsp.focus({ window = 'address:0xa' })");
        assert!(calls[3].starts_with("hl.dsp.send_shortcut") && calls[3].contains("address:0xa"));
        assert_eq!(calls[4], "sleep 120");
        assert_eq!(calls[5], "hl.dsp.focus({ window = 'address:0xb' })");
    }

    #[test]
    fn scenario_18_closed_target_keeps_the_text_on_the_clipboard() {
        let mut f = Fake::new(Some(win("0xb", "firefox")));
        f.closed.push("0xa".into());
        let out = deliver(&f, &HashMap::new(), &req("x", Some(win("0xa", "kitty"))));
        assert_eq!(out, Outcome::CopiedOnly(Reason::TargetClosed));
        assert!(!f.calls.borrow().iter().any(|c| c.contains("send_shortcut")));
    }

    #[test]
    fn scenario_18_target_closing_during_the_paste_is_detected() {
        let mut f = Fake::new(Some(win("0xa", "kitty")));
        f.closed.push("0xa".into());
        let out = deliver(&f, &HashMap::new(), &req("x", Some(win("0xa", "kitty"))));
        assert_eq!(out, Outcome::CopiedOnly(Reason::TargetClosed));
    }

    #[test]
    fn scenario_20_per_class_paste_keys() {
        let f = Fake::new(Some(win("0xa", "wez")));
        let keys = HashMap::from([("wez".to_string(), "ctrl+shift+v".to_string())]);
        deliver(&f, &keys, &req("x", Some(win("0xa", "wez"))));
        assert!(f.calls.borrow()[2].contains("mods = 'CTRL SHIFT', key = 'v'"));
    }

    #[test]
    fn paste_again_goes_to_whatever_is_focused() {
        let f = Fake::new(Some(win("0xc", "kitty")));
        assert_eq!(deliver(&f, &HashMap::new(), &req("x", None)), Outcome::Pasted);
        assert!(f.calls.borrow()[2].contains("address:0xc"));
        assert_eq!(f.calls.borrow().len(), 3);
    }

    #[test]
    fn nothing_focused_means_clipboard_only() {
        let f = Fake::new(None);
        assert_eq!(deliver(&f, &HashMap::new(), &req("x", None)), Outcome::CopiedOnly(Reason::NothingFocused));
    }

    #[test]
    fn clipboard_failure_sends_no_keys() {
        let mut f = Fake::new(Some(win("0xa", "k")));
        f.copy_fails = true;
        assert!(matches!(deliver(&f, &HashMap::new(), &req("x", None)), Outcome::Failed(_)));
        assert!(f.calls.borrow().is_empty());
    }

    #[test]
    fn bad_paste_keys_are_a_failure_not_a_panic() {
        let f = Fake::new(Some(win("0xa", "k")));
        let keys = HashMap::from([("k".to_string(), "hyper+v".to_string())]);
        assert!(matches!(deliver(&f, &keys, &req("x", Some(win("0xa", "k")))), Outcome::Failed(_)));
    }
}
