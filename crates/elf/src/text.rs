//! Text clean-up: snippets, dictation markers, trim, trailing space.

use crate::config::Config;

/// The text to paste. Empty when nothing is left after clean-up.
pub fn finalize(raw: &str, cfg: &Config) -> String {
    let text = match snippet(raw, cfg) {
        Some(s) => s.to_string(),
        None if cfg.spoken_punctuation => markers_to_newlines(raw),
        None => raw.to_string(),
    };
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    if cfg.trailing_space { format!("{text} ") } else { text.to_string() }
}

/// A snippet applies only when the whole transcript is its cue, ignoring case and punctuation.
fn snippet<'a>(raw: &str, cfg: &'a Config) -> Option<&'a str> {
    let said = normalise(raw);
    if said.is_empty() {
        return None;
    }
    cfg.snippets.iter().find(|(cue, _)| normalise(cue) == said).map(|(_, text)| text.as_str())
}

fn normalise(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if c != '\'' && !out.ends_with(' ') {
            out.push(' ');
        }
    }
    out.trim().to_string()
}

/// Deepgram writes a spoken "new line" as the four characters `<\n>` and "new paragraph" as `<\n\n>`.
fn markers_to_newlines(s: &str) -> String {
    let s = s.replace("<\\n\\n>", "\n\n").replace("<\\n>", "\n");
    s.split('\n').map(|l| l.trim_matches(' ')).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let mut c = Config::default();
        c.snippets.insert("insert my email".into(), "someone@example.com".into());
        c
    }

    #[test]
    fn trims_and_adds_one_trailing_space() {
        assert_eq!(finalize("  Hello there.  ", &cfg()), "Hello there. ");
    }

    #[test]
    fn trailing_space_can_be_off() {
        let mut c = cfg();
        c.trailing_space = false;
        assert_eq!(finalize(" Hello. ", &c), "Hello.");
    }

    #[test]
    fn empty_stays_empty() {
        assert_eq!(finalize("   ", &cfg()), "");
    }

    #[test]
    fn snippet_ignores_case_and_punctuation() {
        assert_eq!(finalize("Insert my email.", &cfg()), "someone@example.com ");
        assert_eq!(finalize("insert, my email", &cfg()), "someone@example.com ");
    }

    #[test]
    fn snippet_needs_the_whole_transcript() {
        assert_eq!(finalize("Please insert my email now.", &cfg()), "Please insert my email now. ");
    }

    #[test]
    fn markers_become_newlines_only_with_spoken_punctuation() {
        let raw = "Dear Sam, <\\n\\n> Thanks <\\n> Bye.";
        let mut c = cfg();
        assert_eq!(finalize(raw, &c), format!("{raw} "));
        c.spoken_punctuation = true;
        assert_eq!(finalize(raw, &c), "Dear Sam,\n\nThanks\nBye. ");
    }

    #[test]
    fn apostrophes_do_not_split_snippet_cues() {
        let mut c = Config::default();
        c.snippets.insert("what's my phone".into(), "123".into());
        assert_eq!(finalize("Whats my phone?", &c), "123 ");
    }
}
