//! Local history of what happened on this computer, without any content.
//!
//! One JSON line per event: time, Environment, event and, for tool calls, the
//! tool name, whether it succeeded and how long it took. Never arguments,
//! screen contents, text or passwords.

use crate::time::{now_ms, rfc3339};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    pub ts: String,
    pub env: String,
    pub event: Event,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ms: Option<u64>,
    /// A reason code from a fixed vocabulary, see [`crate::channel::Reason`].
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
    /// A command an agent ran (contract v1, section 3); never its output.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub exit_code: Option<i32>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Event {
    Paired,
    Removed,
    SwitchedOn,
    SwitchedOff,
    Connected,
    Disconnected,
    Stopped,
    Tool,
    /// An agent's command finished or was ended.
    Command,
    /// The "commands on this computer" switch.
    CommandsOn,
    CommandsOff,
}

impl HistoryEntry {
    pub fn new(env: &str, event: Event) -> Self {
        Self {
            ts: rfc3339(now_ms()),
            env: env.to_string(),
            event,
            tool: None,
            ok: None,
            ms: None,
            reason: None,
            command: None,
            exit_code: None,
        }
    }
}

pub struct History {
    path: PathBuf,
    lock: Mutex<()>,
}

impl History {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn record(&self, env: &str, event: Event) -> HistoryEntry {
        self.append(HistoryEntry::new(env, event))
    }

    pub fn append(&self, entry: HistoryEntry) -> HistoryEntry {
        let _guard = self.lock.lock().unwrap();
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() > MAX_BYTES) {
            let _ = std::fs::rename(&self.path, self.path.with_extension("1.jsonl"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            if let Ok(line) = serde_json::to_string(&entry) {
                let _ = writeln!(f, "{line}");
            }
        }
        entry
    }

    /// The newest `limit` entries, newest first, optionally for one Environment.
    pub fn recent(&self, limit: usize, env: Option<&str>) -> Vec<HistoryEntry> {
        let _guard = self.lock.lock().unwrap();
        let mut out = Vec::new();
        for path in [self.path.with_extension("1.jsonl"), self.path.clone()] {
            let Ok(f) = std::fs::File::open(&path) else {
                continue;
            };
            for line in BufReader::new(f).lines().map_while(|l| l.ok()) {
                if let Ok(e) = serde_json::from_str::<HistoryEntry>(&line) {
                    if env.is_none_or(|id| id == e.env) {
                        out.push(e);
                    }
                }
            }
        }
        out.reverse();
        out.truncate(limit);
        out
    }

    pub fn forget(&self, env: &str) {
        let _guard = self.lock.lock().unwrap();
        for path in [self.path.with_extension("1.jsonl"), self.path.clone()] {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let kept: String = text
                .lines()
                .filter(|l| serde_json::from_str::<HistoryEntry>(l).map_or(true, |e| e.env != env))
                .map(|l| format!("{l}\n"))
                .collect();
            let _ = crate::platform::write_atomic(&path, kept.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_first_and_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::new(dir.path().join("history.jsonl"));
        h.record("a", Event::Connected);
        h.append(HistoryEntry {
            ts: "t".into(),
            env: "b".into(),
            event: Event::Tool,
            tool: Some("desktop_click".into()),
            ok: Some(true),
            ms: Some(12),
            ..HistoryEntry::new("b", Event::Tool)
        });
        h.record("a", Event::Disconnected);
        let all = h.recent(10, None);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].event, Event::Disconnected);
        assert_eq!(
            h.recent(10, Some("b"))[0].tool.as_deref(),
            Some("desktop_click")
        );
        h.forget("a");
        assert_eq!(h.recent(10, None).len(), 1);
    }
}
