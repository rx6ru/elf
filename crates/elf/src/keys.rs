//! Deepgram keys: where they are read from, which one a dictation uses, and what happens when
//! one stops working. A key never appears in a log line or a reply; `label` names it by index
//! (and its last four characters).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::ApiKey;

const SINGLE: &str = "DEEPGRAM_API_KEY";
const LIST: &str = "DEEPGRAM_API_KEYS";
const NUMBERED_PREFIX: &str = "DEEPGRAM_API_KEY_";

/// `NAME=value` lines of `.env` text: optional `export`, optional quotes, `#` comments.
pub fn dotenv_pairs(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (k, v) = line.split_once('=')?;
            (!k.trim().starts_with('#')).then(|| (k.trim().to_string(), v.trim().trim_matches(|c| c == '"' || c == '\'').to_string()))
        })
        .collect()
}

fn split_list(value: &str) -> impl Iterator<Item = String> + '_ {
    value.split(|c: char| c == ',' || c.is_whitespace()).filter(|k| !k.is_empty()).map(String::from)
}

/// Keys named by `DEEPGRAM_API_KEY`, then `DEEPGRAM_API_KEYS` (comma or space separated), then
/// `DEEPGRAM_API_KEY_2`, `_3`, ... by number.
pub fn keys_from_pairs(pairs: &[(String, String)]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for (k, v) in pairs {
        if k == SINGLE {
            keys.extend(split_list(v));
        }
    }
    for (k, v) in pairs {
        if k == LIST {
            keys.extend(split_list(v));
        }
    }
    let mut numbered: Vec<(u32, &String)> = pairs
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix(NUMBERED_PREFIX)?.parse::<u32>().ok()?, v)))
        .collect();
    numbered.sort_by_key(|(n, _)| *n);
    for (_, v) in numbered {
        keys.extend(split_list(v));
    }
    keys
}

/// A keys file: one key per line, `#` starts a comment.
pub fn keys_from_file(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// Everything in one list, trimmed, without empties or repeats, in first-seen order.
pub fn merge(groups: impl IntoIterator<Item = Vec<String>>) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    for key in groups.into_iter().flatten() {
        let key = key.trim().to_string();
        if !key.is_empty() && !all.contains(&key) {
            all.push(key);
        }
    }
    all
}

/// The keys from the process environment, then each `.env` file, then the keys file.
pub fn load(env: &[(String, String)], env_files: &[PathBuf], keys_file: &Path, read: &dyn Fn(&Path) -> Option<String>) -> Vec<ApiKey> {
    let mut groups = vec![keys_from_pairs(env)];
    groups.extend(env_files.iter().filter_map(|f| read(f)).map(|t| keys_from_pairs(&dotenv_pairs(&t))));
    groups.extend(read(keys_file).map(|t| keys_from_file(&t)));
    merge(groups).into_iter().map(ApiKey::new).collect()
}

/// Why a request was turned away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trouble {
    /// 401 or 403: the key is wrong. Out for good.
    Rejected,
    /// 402: out of credit.
    OutOfCredit,
    /// 429: too many requests.
    RateLimited,
}

pub const COOLDOWN_CREDIT: Duration = Duration::from_secs(10 * 60);
pub const COOLDOWN_RATE: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Health {
    Healthy,
    Rejected,
    CoolingUntil(Instant),
}

pub struct KeyPool {
    keys: Vec<(ApiKey, Health)>,
    /// Where the next dictation starts looking. Lives in memory only.
    next: usize,
}

impl KeyPool {
    pub fn new(keys: Vec<ApiKey>) -> Self {
        Self { keys: keys.into_iter().map(|k| (k, Health::Healthy)).collect(), next: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn key(&self, i: usize) -> &ApiKey {
        &self.keys[i].0
    }

    fn usable(&self, i: usize, now: Instant) -> bool {
        match self.keys[i].1 {
            Health::Healthy => true,
            Health::Rejected => false,
            Health::CoolingUntil(until) => now >= until,
        }
    }

    /// The key for the next dictation: round robin over the keys that are not out. When every
    /// key is out, the one that comes back first is used anyway (or the first, if all are
    /// rejected), so the request fails the way it always did instead of not starting.
    pub fn assign(&mut self, now: Instant) -> usize {
        let n = self.keys.len();
        let chosen = (0..n).map(|off| (self.next + off) % n).find(|&i| self.usable(i, now)).unwrap_or_else(|| {
            (0..n)
                .filter_map(|i| match self.keys[i].1 {
                    Health::CoolingUntil(until) => Some((until, i)),
                    _ => None,
                })
                .min()
                .map_or(0, |(_, i)| i)
        });
        self.next = (chosen + 1) % n;
        chosen
    }

    /// Takes key `i` out for `trouble` and returns the next key that is not out, if any.
    pub fn report(&mut self, i: usize, trouble: Trouble, now: Instant) -> Option<usize> {
        self.keys[i].1 = match trouble {
            Trouble::Rejected => Health::Rejected,
            Trouble::OutOfCredit => Health::CoolingUntil(now + COOLDOWN_CREDIT),
            Trouble::RateLimited => Health::CoolingUntil(now + COOLDOWN_RATE),
        };
        let n = self.keys.len();
        (1..n).map(|off| (i + off) % n).find(|&j| self.usable(j, now))
    }

    /// `key#2(…abcd)`: the index, and the last four characters of a long key.
    pub fn label(&self, i: usize) -> String {
        match self.keys[i].0.tail() {
            Some(t) => format!("key#{}(…{t})", i + 1),
            None => format!("key#{}", i + 1),
        }
    }

    /// `keys: 3`, or `keys: 3 (1 rejected, 1 cooling down)`.
    pub fn summary(&self, now: Instant) -> String {
        let rejected = self.keys.iter().filter(|(_, h)| *h == Health::Rejected).count();
        let cooling = self.keys.iter().filter(|(_, h)| matches!(h, Health::CoolingUntil(u) if now < *u)).count();
        let mut notes = Vec::new();
        if rejected > 0 {
            notes.push(format!("{rejected} rejected"));
        }
        if cooling > 0 {
            notes.push(format!("{cooling} cooling down"));
        }
        if notes.is_empty() { format!("keys: {}", self.len()) } else { format!("keys: {} ({})", self.len(), notes.join(", ")) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn pool(n: usize) -> KeyPool {
        KeyPool::new((0..n).map(|i| ApiKey::new(format!("fake-key-{i}-wxyz{i}{i}{i}"))).collect())
    }

    fn at(secs: u64) -> Instant {
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now) + Duration::from_secs(secs)
    }

    #[test]
    fn dotenv_lines() {
        let t = "# comment\nOTHER=1\nexport DEEPGRAM_API_KEY=\"abc\"\nDEEPGRAM_API_KEYS = 'x, y'\n";
        assert_eq!(dotenv_pairs(t), pairs(&[("OTHER", "1"), ("DEEPGRAM_API_KEY", "abc"), ("DEEPGRAM_API_KEYS", "x, y")]));
    }

    #[test]
    fn all_three_env_forms_merge_in_order() {
        let p = pairs(&[
            ("DEEPGRAM_API_KEY_3", "k4"),
            ("DEEPGRAM_API_KEYS", "k2, k3\tk3x"),
            ("DEEPGRAM_API_KEY", "k1"),
            ("DEEPGRAM_API_KEY_2", "k5"),
            ("DEEPGRAM_API_KEY_X", "ignored"),
            ("DEEPGRAM_API_KEY_", "ignored"),
            ("HOME", "/x"),
        ]);
        assert_eq!(keys_from_pairs(&p), ["k1", "k2", "k3", "k3x", "k5", "k4"]);
    }

    #[test]
    fn a_keys_file_has_one_key_per_line_and_comments() {
        let t = "# my keys\nkeyA\n\n  keyB  # work account\n   # off\nkeyC\n";
        assert_eq!(keys_from_file(t), ["keyA", "keyB", "keyC"]);
    }

    #[test]
    fn merge_trims_drops_empties_and_repeats_keeping_order() {
        let merged = merge([vec!["a".into(), " b ".into(), "".into()], vec!["b".into(), "c".into(), "a".into()]]);
        assert_eq!(merged, ["a", "b", "c"]);
    }

    #[test]
    fn load_reads_every_source() {
        let env = pairs(&[("DEEPGRAM_API_KEY", "from-env")]);
        let files = [PathBuf::from("/c/.env"), PathBuf::from(".env"), PathBuf::from("/repo/.env")];
        let read = |p: &Path| -> Option<String> {
            match p.to_str()? {
                "/c/.env" => Some("DEEPGRAM_API_KEYS=conf1,from-env,conf2".into()),
                ".env" => None,
                "/repo/.env" => Some("DEEPGRAM_API_KEY=repo1\nDEEPGRAM_API_KEY_2=repo2".into()),
                "/c/deepgram_keys" => Some("file1\nrepo1 # again\nfile2".into()),
                _ => None,
            }
        };
        let keys = load(&env, &files, Path::new("/c/deepgram_keys"), &read);
        let plain: Vec<&str> = keys.iter().map(ApiKey::expose).collect();
        assert_eq!(plain, ["from-env", "conf1", "conf2", "repo1", "repo2", "file1", "file2"]);
        assert!(load(&[], &files, Path::new("/none"), &|_| None).is_empty());
    }

    #[test]
    fn dictations_take_the_keys_in_turn() {
        let mut p = pool(3);
        let order: Vec<usize> = (0..7).map(|_| p.assign(at(0))).collect();
        assert_eq!(order, [0, 1, 2, 0, 1, 2, 0]);
    }

    #[test]
    fn rotation_skips_keys_that_are_out() {
        let mut p = pool(3);
        p.report(1, Trouble::Rejected, at(0));
        let order: Vec<usize> = (0..4).map(|_| p.assign(at(0))).collect();
        assert_eq!(order, [0, 2, 0, 2]);
    }

    #[test]
    fn failover_goes_to_the_next_healthy_key() {
        let mut p = pool(3);
        assert_eq!(p.report(0, Trouble::Rejected, at(0)), Some(1));
        assert_eq!(p.report(1, Trouble::RateLimited, at(0)), Some(2));
        assert_eq!(p.report(2, Trouble::OutOfCredit, at(0)), None);
        // One key alone: nowhere to go.
        assert_eq!(pool(1).report(0, Trouble::Rejected, at(0)), None);
    }

    #[test]
    fn a_rejected_key_never_comes_back_a_cooling_one_does() {
        let mut p = pool(2);
        p.report(0, Trouble::RateLimited, at(100));
        p.report(1, Trouble::OutOfCredit, at(100));
        assert_eq!(p.summary(at(100)), "keys: 2 (2 cooling down)");
        // 429 lasts 60 s, 402 lasts 10 minutes.
        assert_eq!(p.assign(at(159)), 0, "all out: the one that returns first is tried anyway");
        let mut q = pool(2);
        q.report(0, Trouble::RateLimited, at(100));
        q.report(1, Trouble::OutOfCredit, at(100));
        assert!(q.usable(0, at(160)) && !q.usable(1, at(160)));
        assert!(q.usable(1, at(100 + 600)));
        let mut r = pool(2);
        r.report(0, Trouble::Rejected, at(0));
        assert!(!r.usable(0, at(1_000_000)));
    }

    #[test]
    fn with_every_key_rejected_a_dictation_still_gets_one() {
        let mut p = pool(2);
        p.report(0, Trouble::Rejected, at(0));
        p.report(1, Trouble::Rejected, at(0));
        assert_eq!(p.assign(at(0)), 0);
    }

    #[test]
    fn the_summary_counts_what_is_out() {
        let mut p = pool(3);
        assert_eq!(p.summary(at(0)), "keys: 3");
        p.report(0, Trouble::Rejected, at(0));
        p.report(1, Trouble::RateLimited, at(0));
        assert_eq!(p.summary(at(0)), "keys: 3 (1 rejected, 1 cooling down)");
        assert_eq!(p.summary(at(61)), "keys: 3 (1 rejected)");
    }

    #[test]
    fn labels_never_hold_more_than_the_last_four_characters() {
        let p = KeyPool::new(vec![ApiKey::new("SECRETSECRETSECRET1234".into()), ApiKey::new("short".into())]);
        assert_eq!(p.label(0), "key#1(…1234)");
        assert_eq!(p.label(1), "key#2");
        assert!(!p.label(0).contains("SECRET"));
    }
}
