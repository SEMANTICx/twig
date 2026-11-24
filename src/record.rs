//! Append-only, hash-chained session record.
//!
//! Every entry commits to the previous one. Rewriting or dropping history breaks
//! the chain, which is the property the surveyed harnesses all lack: an outside
//! party can check the record without trusting the runtime.
//!
//! The record is the API. Resume, replay, audit and fork all read it.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Canonical JSON: object keys sorted at every depth, so the hash is stable
/// regardless of insertion order.
fn canonical_into(value: &Value, buf: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            buf.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    buf.push(',');
                }
                buf.push_str(&Value::String((*key).clone()).to_string());
                buf.push(':');
                canonical_into(&map[*key], buf);
            }
            buf.push('}');
        }
        Value::Array(items) => {
            buf.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    buf.push(',');
                }
                canonical_into(item, buf);
            }
            buf.push(']');
        }
        other => buf.push_str(&other.to_string()),
    }
}

pub fn hash_of(value: &Value) -> String {
    let mut buf = String::new();
    canonical_into(value, &mut buf);
    blake3::hash(buf.as_bytes()).to_hex().to_string()
}

pub struct Record {
    path: PathBuf,
    sid: String,
    prev: String,
    seq: u64,
}

impl Record {
    pub fn open(path: impl Into<PathBuf>, sid: impl Into<String>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // an empty chain is still a chain
        OpenOptions::new().create(true).append(true).open(&path)?;
        Self::verify_chain(&path).map_err(|(i,e)|std::io::Error::other(format!("record corruption at {i}: {e}")))?;
        let mut prev = GENESIS.to_string();
        let mut seq = 0u64;
        if let Ok(file) = std::fs::File::open(&path) {
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if let Some(h) = v.get("hash").and_then(Value::as_str) {
                        prev = h.to_string();
                        seq += 1;
                    }
                }
            }
        }
        Ok(Self { path, sid: sid.into(), prev, seq })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> u64 {
        self.seq
    }

    pub fn is_empty(&self) -> bool {
        self.seq == 0
    }

    /// Append one entry. `extra` must be an object; its fields join the envelope.
    pub fn append(&mut self, kind: &str, extra: Value) -> std::io::Result<Value> {
        let mut map = serde_json::Map::new();
        map.insert("seq".into(), json!(self.seq));
        map.insert("ts".into(), json!(now_ms()));
        map.insert("sid".into(), json!(self.sid));
        map.insert("kind".into(), json!(kind));
        map.insert("prev".into(), json!(self.prev));
        if let Value::Object(fields) = extra {
            for (k, v) in fields {
                if !["seq","ts","sid","kind","prev","hash"].contains(&k.as_str()) { map.insert(k, v); }
            }
        }
        let mut body = Value::Object(map);
        let hash = hash_of(&body);
        body.as_object_mut()
            .expect("object")
            .insert("hash".into(), json!(hash));

        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        writeln!(file, "{body}")?;
        file.sync_data()?;
        self.prev = hash;
        self.seq += 1;
        Ok(body)
    }

    pub fn tail(&self, n: usize) -> std::io::Result<Vec<Value>> {
        let file = std::fs::File::open(&self.path)?;
        let mut rows: Vec<Value> = BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
            .collect();
        let start = rows.len().saturating_sub(n);
        Ok(rows.split_off(start))
    }

    /// `Ok(entries)` when the whole chain is intact, `Err((index, reason))` otherwise.
    pub fn verify_chain(path: &Path) -> Result<u64, (usize, String)> {
        let file = std::fs::File::open(path)
            .map_err(|e| (0, format!("open {}: {e}", path.display())))?;
        let mut prev = GENESIS.to_string();
        let mut count = 0u64;
        let mut index = 0usize;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|e| (index, e.to_string()))?;
            if line.trim().is_empty() {
                continue;
            }
            let mut value: Value =
                serde_json::from_str(&line).map_err(|e| (index, format!("json: {e}")))?;
            let claimed = value
                .get("hash")
                .and_then(Value::as_str)
                .ok_or((index, "missing hash".to_string()))?
                .to_string();
            value
                .as_object_mut()
                .ok_or((index, "not an object".to_string()))?
                .remove("hash");
            if hash_of(&value) != claimed {
                return Err((index, "hash mismatch".into()));
            }
            if value.get("prev").and_then(Value::as_str) != Some(prev.as_str()) {
                return Err((index, "chain break".into()));
            }
            prev = claimed;
            count += 1;
            index += 1;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        // Parallel tests must never share a path: pid + millis collides under load.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "twig-rec-{}-{}-{}.jsonl",
            std::process::id(),
            now_ms(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn chain_round_trip() {
        let path = tmp();
        let mut rec = Record::open(&path, "s1").unwrap();
        rec.append("turn.started", json!({"model":"m"})).unwrap();
        rec.append("tool.result", json!({"id":"t1","output":"hi"})).unwrap();
        assert_eq!(Record::verify_chain(&path).unwrap(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tamper_is_detected() {
        let path = tmp();
        let mut rec = Record::open(&path, "s1").unwrap();
        rec.append("a", json!({"x":1})).unwrap();
        rec.append("b", json!({"x":2})).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let patched = body.replace("\"x\":2", "\"x\":99");
        std::fs::write(&path, patched).unwrap();
        let err = Record::verify_chain(&path).unwrap_err();
        assert_eq!(err.0, 1);
        assert_eq!(err.1, "hash mismatch");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn resume_continues_the_chain() {
        let path = tmp();
        {
            let mut rec = Record::open(&path, "s1").unwrap();
            rec.append("a", json!({})).unwrap();
        }
        {
            let mut rec = Record::open(&path, "s1").unwrap();
            assert_eq!(rec.len(), 1);
            rec.append("b", json!({})).unwrap();
        }
        assert_eq!(Record::verify_chain(&path).unwrap(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn key_order_does_not_change_the_hash() {
        let a = json!({"b":1,"a":2});
        let b = json!({"a":2,"b":1});
        assert_eq!(hash_of(&a), hash_of(&b));
    }
}
