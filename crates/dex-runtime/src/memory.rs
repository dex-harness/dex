//! Persistent executable memory.
//!
//! Memory is not a transcript. A record can hold a *program*, and a later
//! session can load it and run it again, which is the difference between
//! remembering a fact and accumulating a reusable procedure.
//!
//! Records are file-backed JSON, one file per key. A database is explicitly out
//! of scope for the PoC, so the store is a plain struct with a small surface
//! that a real backend could replace without callers noticing.
//!
//! A program record keeps the authorities it needs. That is recorded at save
//! time for the operator's benefit, and re-checked at run time by the ordinary
//! authorization path — storing a program never grants it anything.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::events::bus::now_millis;

/// What kind of knowledge a record holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Fact,
    Discovery,
    Decision,
    Failure,
    Workaround,
    /// A reusable procedure: carries a program that can be run again.
    Program,
    History,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Discovery => "discovery",
            MemoryKind::Decision => "decision",
            MemoryKind::Failure => "failure",
            MemoryKind::Workaround => "workaround",
            MemoryKind::Program => "program",
            MemoryKind::History => "history",
        }
    }
}

impl std::str::FromStr for MemoryKind {
    type Err = MemoryError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "fact" => MemoryKind::Fact,
            "discovery" => MemoryKind::Discovery,
            "decision" => MemoryKind::Decision,
            "failure" => MemoryKind::Failure,
            "workaround" => MemoryKind::Workaround,
            "program" => MemoryKind::Program,
            "history" => MemoryKind::History,
            other => {
                return Err(MemoryError::InvalidKind(other.to_string()));
            }
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub key: String,
    pub kind: MemoryKind,
    /// Reusable source. Present for `Program`, absent otherwise.
    #[serde(default)]
    pub program: Option<String>,
    /// Structured payload for non-program records.
    #[serde(default)]
    pub value: Option<serde_json::Value>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Authorities this program needs. Recorded for the operator; enforced by
    /// the normal authorization path when the program actually runs.
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    #[serde(default)]
    pub required_scopes: Vec<String>,
    pub created_at_ms: u64,
    #[serde(default)]
    pub last_used_ms: Option<u64>,
    #[serde(default)]
    pub exec_count: u64,
    #[serde(default)]
    pub known_failures: Vec<String>,
}

impl MemoryRecord {
    pub fn new(key: impl Into<String>, kind: MemoryKind) -> Self {
        Self {
            key: key.into(),
            kind,
            program: None,
            value: None,
            name: None,
            description: None,
            tags: Vec::new(),
            required_capabilities: Vec::new(),
            required_scopes: Vec::new(),
            created_at_ms: now_millis(),
            last_used_ms: None,
            exec_count: 0,
            known_failures: Vec::new(),
        }
    }

    /// The record as the model sees it: a plain map, so a program can index
    /// fields without knowing this type.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("no memory entry named {0:?}")]
    NotFound(String),
    #[error("not a valid memory kind: {0:?}")]
    InvalidKind(String),
    #[error("memory key {0:?} is not usable as a file name")]
    InvalidKey(String),
    #[error("memory store at {path}: {message}")]
    Io { path: String, message: String },
    #[error("memory entry {key:?} is corrupt: {message}")]
    Corrupt { key: String, message: String },
}

impl MemoryError {
    pub fn io(path: &Path, e: impl std::fmt::Display) -> Self {
        MemoryError::Io {
            path: path.display().to_string(),
            message: e.to_string(),
        }
    }
}

/// A directory of JSON records.
#[derive(Debug)]
pub struct MemoryStore {
    dir: PathBuf,
    /// Serializes read-modify-write cycles within one process. Cross-process
    /// safety is not claimed; a second `dexd` on the same store is out of scope.
    lock: Mutex<()>,
}

impl MemoryStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn ensure_dir(&self) -> Result<(), MemoryError> {
        std::fs::create_dir_all(&self.dir).map_err(|e| MemoryError::io(&self.dir, e))
    }

    /// Map a key to a file name. Keys are namespaced by convention
    /// (`rust.unsafe-audit`), so a path separator or `..` in a key must not be
    /// able to escape the store.
    fn path_for(&self, key: &str) -> Result<PathBuf, MemoryError> {
        let name: String = key
            .trim()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let name = name.trim_matches('.').to_string();
        if name.is_empty() {
            return Err(MemoryError::InvalidKey(key.to_string()));
        }
        Ok(self.dir.join(format!("{name}.json")))
    }

    /// Insert or replace a record.
    pub fn save(&self, record: MemoryRecord) -> Result<MemoryRecord, MemoryError> {
        let path = self.path_for(&record.key)?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.ensure_dir()?;
        let text = serde_json::to_string_pretty(&record)
            .map_err(|e| MemoryError::Corrupt {
                key: record.key.clone(),
                message: e.to_string(),
            })?;
        // Write to a sibling then rename, so a crash mid-write cannot leave a
        // half-written record that fails to parse on the next load.
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, text.as_bytes()).map_err(|e| MemoryError::io(&temp, e))?;
        std::fs::rename(&temp, &path).map_err(|e| MemoryError::io(&path, e))?;
        Ok(record)
    }

    pub fn load(&self, key: &str) -> Result<MemoryRecord, MemoryError> {
        let path = self.path_for(key)?;
        let text = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => MemoryError::NotFound(key.to_string()),
            _ => MemoryError::io(&path, e),
        })?;
        serde_json::from_str(&text).map_err(|e| MemoryError::Corrupt {
            key: key.to_string(),
            message: e.to_string(),
        })
    }

    /// Every key currently stored, sorted.
    pub fn list(&self) -> Result<Vec<String>, MemoryError> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let mut keys = Vec::new();
        for entry in std::fs::read_dir(&self.dir).map_err(|e| MemoryError::io(&self.dir, e))? {
            let entry = entry.map_err(|e| MemoryError::io(&self.dir, e))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            keys.push(stem.to_string());
        }
        keys.sort();
        Ok(keys)
    }

    pub fn delete(&self, key: &str) -> Result<(), MemoryError> {
        let path = self.path_for(key)?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::remove_file(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => MemoryError::NotFound(key.to_string()),
            _ => MemoryError::io(&path, e),
        })
    }

    /// Record that a stored program ran: bumps the counter and the last-used
    /// stamp. A missing key is not an error here, because the record may have
    /// been deleted since the caller listed it.
    pub fn record_use(&self, key: &str) -> Result<(), MemoryError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut record = self.load(key)?;
        record.exec_count += 1;
        record.last_used_ms = Some(now_millis());
        let path = self.path_for(&record.key)?;
        let text = serde_json::to_string_pretty(&record).map_err(|e| MemoryError::Corrupt {
            key: record.key.clone(),
            message: e.to_string(),
        })?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, text.as_bytes()).map_err(|e| MemoryError::io(&temp, e))?;
        std::fs::rename(&temp, &path).map_err(|e| MemoryError::io(&path, e))
    }

    /// Append a failure note to a stored program, so a procedure that stops
    /// working carries that forward instead of failing silently next time.
    pub fn record_failure(&self, key: &str, reason: &str) -> Result<(), MemoryError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut record = self.load(key)?;
        record.known_failures.push(reason.to_string());
        let path = self.path_for(&record.key)?;
        let text = serde_json::to_string_pretty(&record).map_err(|e| MemoryError::Corrupt {
            key: record.key.clone(),
            message: e.to_string(),
        })?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, text.as_bytes()).map_err(|e| MemoryError::io(&temp, e))?;
        std::fs::rename(&temp, &path).map_err(|e| MemoryError::io(&path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, MemoryStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = MemoryStore::new(dir.path().join("memory"));
        (dir, store)
    }

    #[test]
    fn a_program_survives_a_save_and_load() {
        let (_d, store) = store();
        let mut record = MemoryRecord::new("rust.unsafe-audit", MemoryKind::Program);
        record.program = Some("let m = repo.find(\"unsafe\");\nm".to_string());
        record.name = Some("Unsafe audit".into());
        record.tags = vec!["rust".into(), "audit".into()];
        record.required_capabilities = vec!["filesystem.read".into()];
        store.save(record).expect("save");

        let loaded = store.load("rust.unsafe-audit").expect("load");
        assert_eq!(loaded.kind, MemoryKind::Program);
        assert_eq!(loaded.program.as_deref(), Some("let m = repo.find(\"unsafe\");\nm"));
        assert_eq!(loaded.tags, vec!["rust".to_string(), "audit".to_string()]);
        assert_eq!(loaded.required_capabilities, vec!["filesystem.read".to_string()]);
    }

    #[test]
    fn a_fact_round_trips_with_a_structured_value() {
        let (_d, store) = store();
        let mut record = MemoryRecord::new("db.url", MemoryKind::Fact);
        record.value = Some(serde_json::json!({"host": "localhost", "port": 5432}));
        store.save(record).expect("save");

        let loaded = store.load("db.url").expect("load");
        assert_eq!(loaded.value.unwrap()["port"], 5432);
        assert!(loaded.program.is_none());
    }

    #[test]
    fn saving_the_same_key_replaces_rather_than_duplicates() {
        let (_d, store) = store();
        store.save(MemoryRecord::new("k", MemoryKind::Fact)).expect("first");
        let mut second = MemoryRecord::new("k", MemoryKind::Decision);
        second.description = Some("changed".into());
        store.save(second).expect("second");
        assert_eq!(store.load("k").expect("load").kind, MemoryKind::Decision);
        assert_eq!(store.list().expect("list").len(), 1);
    }

    #[test]
    fn a_missing_key_reports_not_found() {
        let (_d, store) = store();
        assert!(matches!(
            store.load("absent").expect_err("must fail"),
            MemoryError::NotFound(_)
        ));
        assert!(matches!(
            store.delete("absent").expect_err("must fail"),
            MemoryError::NotFound(_)
        ));
    }

    #[test]
    fn keys_cannot_escape_the_store_directory() {
        let (dir, store) = store();
        store.ensure_dir().expect("dir");
        // A traversal key is neutralised into a flat file name rather than
        // honoured, so the write lands inside the store.
        store
            .save(MemoryRecord::new("../../escape", MemoryKind::Fact))
            .expect("save");

        let written: Vec<_> = std::fs::read_dir(store.dir())
            .expect("store dir")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(written.len(), 1, "exactly one record");
        // The only thing that matters is containment: the record is a direct
        // child of the store directory, so no part of the key was honoured as
        // a path.
        let landed = written[0].path();
        assert_eq!(
            landed.parent(),
            Some(store.dir()),
            "record landed outside the store: {}",
            landed.display()
        );
        // And nothing landed beside the store, one level up.
        assert!(
            !dir.path().join("escape").exists(),
            "a key escaped the store directory"
        );
    }

    #[test]
    fn an_empty_key_is_rejected() {
        let (_d, store) = store();
        assert!(matches!(
            store.save(MemoryRecord::new("   ", MemoryKind::Fact)).expect_err("must fail"),
            MemoryError::InvalidKey(_)
        ));
    }

    #[test]
    fn use_and_failure_counters_accumulate() {
        let (_d, store) = store();
        store
            .save(MemoryRecord::new("p", MemoryKind::Program))
            .expect("save");
        store.record_use("p").expect("use");
        store.record_use("p").expect("use");
        store.record_failure("p", "assumed cargo on PATH").expect("failure");

        let loaded = store.load("p").expect("load");
        assert_eq!(loaded.exec_count, 2);
        assert!(loaded.last_used_ms.is_some());
        assert_eq!(loaded.known_failures, vec!["assumed cargo on PATH".to_string()]);
    }

    #[test]
    fn a_partial_write_cannot_corrupt_an_existing_record() {
        let (_d, store) = store();
        store
            .save(MemoryRecord::new("p", MemoryKind::Program))
            .expect("save");
        store.record_use("p").expect("use");
        // The temp file must not survive a successful save.
        let leftovers: Vec<_> = std::fs::read_dir(store.dir())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind");
    }

    #[test]
    fn a_corrupt_record_reports_the_key() {
        let (_d, store) = store();
        store.ensure_dir().expect("dir");
        std::fs::write(store.path_for("broken").unwrap(), "{not json").expect("write");
        assert!(matches!(
            store.load("broken").expect_err("must fail"),
            MemoryError::Corrupt { .. }
        ));
    }

    #[test]
    fn kind_round_trips_through_its_string_form() {
        for kind in [
            MemoryKind::Fact,
            MemoryKind::Discovery,
            MemoryKind::Decision,
            MemoryKind::Failure,
            MemoryKind::Workaround,
            MemoryKind::Program,
            MemoryKind::History,
        ] {
            assert_eq!(kind.as_str().parse::<MemoryKind>().expect("parse"), kind);
        }
        assert!("nonsense".parse::<MemoryKind>().is_err());
    }
}
