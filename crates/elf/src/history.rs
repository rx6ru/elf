//! `history.jsonl` and the WAV spool.

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::machine::SessionId;

/// One line of `history.jsonl`. `id` is the unix time in ms the dictation began.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: String,
    pub ts: String,
    pub app: Option<String>,
    pub target: Option<String>,
    pub ms_audio: u64,
    pub ms_to_text: u64,
    pub pass1: Option<String>,
    pub pass2: Option<String>,
    pub text: Option<String>,
    /// `"pass1"` or `"pass2"`.
    pub chosen: Option<String>,
    pub error: Option<String>,
    /// The spooled WAV, while the text is not yet delivered.
    pub wav: Option<String>,
}

impl Entry {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("an entry is always valid JSON")
    }
}

/// Where dictations and audio are kept. A fake replaces it in tests.
pub trait Store: Send {
    fn save_wav(&self, id: SessionId, wav: &[u8]) -> io::Result<PathBuf>;
    fn delete_wav(&self, id: SessionId);
    fn spooled(&self) -> Vec<(SessionId, PathBuf)>;
    fn load_wav(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn append(&self, entry: &Entry);
    /// The newest `n` entries, oldest first.
    fn recent(&self, n: usize) -> Vec<Entry>;
    /// The newest text that was produced.
    fn last_text(&self) -> Option<String>;
}

const PRUNE_EVERY: Duration = Duration::from_secs(24 * 3600);

pub struct FileStore {
    dir: PathBuf,
    history_days: u64,
    last_prune: Mutex<Instant>,
}

impl FileStore {
    pub fn open(dir: &Path, history_days: u64) -> io::Result<Self> {
        fs::create_dir_all(dir.join("spool"))?;
        let store = Self { dir: dir.to_path_buf(), history_days, last_prune: Mutex::new(Instant::now()) };
        store.prune(crate::clock::unix_ms());
        Ok(store)
    }

    fn history(&self) -> PathBuf {
        self.dir.join("history.jsonl")
    }

    fn entries(&self) -> Vec<Entry> {
        let text = fs::read_to_string(self.history()).unwrap_or_default();
        text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    /// Drops entries older than `history_days`.
    pub fn prune(&self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(self.history_days * 86_400_000);
        let all = self.entries();
        let keep: Vec<&Entry> = all.iter().filter(|e| e.id.parse::<u64>().map_or(true, |id| id >= cutoff)).collect();
        if keep.len() == all.len() {
            return;
        }
        let body: String = keep.iter().map(|e| e.to_line() + "\n").collect();
        let tmp = self.dir.join("history.jsonl.tmp");
        if fs::write(&tmp, body).and_then(|()| fs::rename(&tmp, self.history())).is_err() {
            crate::elog!("history prune failed");
        }
    }
}

impl Store for FileStore {
    fn save_wav(&self, id: SessionId, wav: &[u8]) -> io::Result<PathBuf> {
        let path = self.dir.join("spool").join(format!("{id}.wav"));
        fs::write(&path, wav)?;
        Ok(path)
    }

    fn delete_wav(&self, id: SessionId) {
        let _ = fs::remove_file(self.dir.join("spool").join(format!("{id}.wav")));
    }

    fn spooled(&self) -> Vec<(SessionId, PathBuf)> {
        let mut found: Vec<_> = fs::read_dir(self.dir.join("spool"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                let id = path.file_stem()?.to_str()?.parse().ok()?;
                (path.extension()? == "wav").then_some((id, path))
            })
            .collect();
        found.sort();
        found
    }

    fn load_wav(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn append(&self, entry: &Entry) {
        let line = entry.to_line() + "\n";
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.history())
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = written {
            crate::elog!("history write failed: {e}");
        }
        let mut last = self.last_prune.lock().unwrap();
        if last.elapsed() >= PRUNE_EVERY {
            *last = Instant::now();
            self.prune(crate::clock::unix_ms());
        }
    }

    fn recent(&self, n: usize) -> Vec<Entry> {
        let all = self.entries();
        all[all.len().saturating_sub(n)..].to_vec()
    }

    fn last_text(&self) -> Option<String> {
        self.entries().into_iter().rev().find_map(|e| e.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, text: Option<&str>) -> Entry {
        Entry {
            id: id.to_string(),
            ts: "2026-11-23T14:05:11+05:30".into(),
            app: Some("kitty".into()),
            target: Some("0xabc".into()),
            ms_audio: 8420,
            ms_to_text: 610,
            pass1: Some("okay so".into()),
            pass2: text.map(String::from),
            text: text.map(String::from),
            chosen: text.map(|_| "pass2".into()),
            error: None,
            wav: None,
        }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("elf-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn line_has_its_fields_in_order() {
        let line = entry(1, Some("Okay, so. ")).to_line();
        assert_eq!(
            line,
            r#"{"id":"1","ts":"2026-11-23T14:05:11+05:30","app":"kitty","target":"0xabc","ms_audio":8420,"ms_to_text":610,"pass1":"okay so","pass2":"Okay, so. ","text":"Okay, so. ","chosen":"pass2","error":null,"wav":null}"#
        );
        assert_eq!(serde_json::from_str::<Entry>(&line).unwrap(), entry(1, Some("Okay, so. ")));
    }

    #[test]
    fn append_recent_and_last_text() {
        let dir = temp("hist");
        let s = FileStore::open(&dir, 30).unwrap();
        let now = crate::clock::unix_ms();
        s.append(&entry(now, Some("first")));
        s.append(&entry(now + 1, None));
        assert_eq!(s.recent(5).len(), 2);
        assert_eq!(s.recent(1)[0].id, (now + 1).to_string());
        assert_eq!(s.last_text().as_deref(), Some("first"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_drops_old_entries() {
        let dir = temp("prune");
        let s = FileStore::open(&dir, 30).unwrap();
        let now = 100 * 86_400_000u64;
        s.append(&entry(now - 40 * 86_400_000, Some("old")));
        s.append(&entry(now - 1000, Some("new")));
        s.prune(now);
        let kept = s.recent(10);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].text.as_deref(), Some("new"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn spool_save_list_delete() {
        let dir = temp("spool");
        let s = FileStore::open(&dir, 30).unwrap();
        s.save_wav(20, b"b").unwrap();
        s.save_wav(10, b"a").unwrap();
        fs::write(dir.join("spool").join("junk.txt"), "x").unwrap();
        let listed = s.spooled();
        assert_eq!(listed.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [10, 20]);
        assert_eq!(s.load_wav(&listed[0].1).unwrap(), b"a");
        s.delete_wav(10);
        assert_eq!(s.spooled().len(), 1);
        let _ = fs::remove_dir_all(dir);
    }
}
