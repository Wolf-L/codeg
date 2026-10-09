//! Read-only projection of Claude CLI's explicit `last-prompt` rewind chain.
//!
//! Cold readers retain only UUID/parent/byte-offset metadata, then interpret the
//! selected records. Incremental readers spool replay bytes (bounded RAM), and
//! rebuild only at an anchor or a changed retained duplicate, never on append.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};

use serde_json::{json, Value};

use crate::parsers::ParseError;

const REPLAY_MEMORY_LIMIT: usize = 256 * 1024;

pub(super) fn is_sidechain(value: &Value) -> bool {
    value.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || ["parent_tool_use_id", "parent_agent_id"]
            .iter()
            .any(|key| value.get(key).is_some_and(|v| !v.is_null()))
}

fn chain_record(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("user" | "assistant" | "system" | "attachment")
    ) && value.get("parentUuid").is_some()
        && value
            .get("uuid")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
}

#[derive(Clone, Copy)]
struct Span {
    offset: u64,
    len: usize,
}

struct Node {
    parent: Option<String>,
    span: Span,
    hash: u64,
    prompt_snapshot: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Update {
    Pass,
    Skip,
    Rebuild,
}

#[derive(Default)]
struct Index {
    nodes: HashMap<String, Node>,
    // None: legacy history. Some(None): explicit empty first-message rewind.
    anchor: Option<Option<String>>,
    anchor_offset: u64,
    retained: HashSet<String>,
    // Competing writes are sticky, matching applyRewindAnchor's immediate throw.
    conflict: Option<&'static str>,
    identity: serde_json::Map<String, Value>,
    titles: HashMap<&'static str, Value>,
}

impl Index {
    /// The CLI can rewind to an assistant UUID but continue from the existing
    /// prompt_snapshot written immediately after that assistant. This is an
    /// invisible context snapshot, not a competing user/assistant branch.
    /// Only pre-anchor snapshots may bridge to the CURRENT leaf. Never walk
    /// through discarded messages, arbitrary attachments, or an empty rewind.
    fn snapshot_bridge(&self, parent: Option<&str>, leaf: Option<&str>) -> Option<Vec<String>> {
        let leaf = leaf?;
        let mut cursor = parent?;
        let mut seen = HashSet::new();
        let mut bridge = Vec::new();
        while cursor != leaf {
            if !seen.insert(cursor) {
                return None;
            }
            let node = self.nodes.get(cursor)?;
            if !node.prompt_snapshot || node.span.offset >= self.anchor_offset {
                return None;
            }
            bridge.push(cursor.to_owned());
            cursor = node.parent.as_deref()?;
        }
        Some(bridge)
    }

    fn observe(&mut self, value: &Value, span: Span, raw: &[u8]) -> Update {
        if is_sidechain(value) {
            return Update::Skip;
        }
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(kind, "user" | "assistant" | "system" | "attachment") {
            for key in ["sessionId", "cwd", "gitBranch", "timestamp"] {
                if !self.identity.contains_key(key) {
                    if let Some(v) = value.get(key).filter(|v| v.is_string()) {
                        self.identity.insert(key.into(), v.clone());
                    }
                }
            }
        }
        for (record, field) in [("custom-title", "customTitle"), ("ai-title", "aiTitle")] {
            if kind == record
                && value
                    .get(field)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty())
            {
                self.titles
                    .insert(record, json!({"type": record, field: value[field]}));
                return Update::Pass;
            }
        }
        if kind == "last-prompt" && value.get("explicit").and_then(Value::as_bool) == Some(true) {
            if let Some(leaf) = value
                .get("leafUuid")
                .filter(|v| v.is_null() || v.is_string())
            {
                self.anchor = Some(leaf.as_str().map(str::to_owned));
                self.anchor_offset = span.offset;
                return Update::Rebuild;
            }
        }
        if chain_record(value) {
            let id = value["uuid"].as_str().unwrap();
            let parent = value["parentUuid"].as_str().map(str::to_owned);
            let mut hasher = DefaultHasher::new();
            raw.hash(&mut hasher);
            let hash = hasher.finish();
            let previous = self.nodes.insert(
                id.to_owned(),
                Node {
                    parent: parent.clone(),
                    span,
                    hash,
                    prompt_snapshot: kind == "attachment"
                        && value.pointer("/attachment/type").and_then(Value::as_str)
                            == Some("prompt_snapshot")
                        && (value["parentUuid"].is_string() || value["parentUuid"].is_null()),
                },
            );
            if let Some(anchor) = &self.anchor {
                if let Some(previous) = previous {
                    // Replayed pre-rewind rows must never advance the leaf.
                    return if self.retained.contains(id)
                        && (previous.hash != hash || previous.parent != parent)
                    {
                        Update::Rebuild
                    } else {
                        Update::Skip
                    };
                }
                if &parent == anchor
                    && (value["parentUuid"].is_null() || value["parentUuid"].is_string())
                {
                    self.anchor = Some(Some(id.to_owned()));
                    self.retained.insert(id.to_owned());
                    return Update::Pass;
                }
                if let Some(bridge) = self.snapshot_bridge(parent.as_deref(), anchor.as_deref()) {
                    self.anchor = Some(Some(id.to_owned()));
                    self.retained.extend(bridge);
                    self.retained.insert(id.to_owned());
                    return Update::Pass;
                }
                if matches!(kind, "user" | "assistant") {
                    self.conflict = Some(
                        "Conversation changed outside the retained rewind chain; reload required",
                    );
                    return Update::Rebuild;
                }
                // Unrelated late system/attachment logs do not reopen a suffix.
                return Update::Skip;
            }
        } else if self.anchor.is_some() && matches!(kind, "user" | "assistant") {
            self.conflict = Some(
                "Conversation record has no rewind chain identity; refusing stale history replay",
            );
            return Update::Rebuild;
        }
        if self.anchor.is_some() {
            Update::Skip
        } else {
            Update::Pass
        }
    }

    fn selection(&mut self) -> Result<Option<Vec<Span>>, ParseError> {
        if let Some(error) = self.conflict {
            return Err(ParseError::InvalidData(error.into()));
        }
        let Some(anchor) = &self.anchor else {
            return Ok(None);
        };
        self.retained.clear();
        let mut cursor = anchor.as_deref();
        let mut selected = Vec::new();
        while let Some(id) = cursor {
            if !self.retained.insert(id.to_owned()) {
                return Err(ParseError::InvalidData(
                    "Cycle in the retained conversation chain".into(),
                ));
            }
            let node = self.nodes.get(id).ok_or_else(|| {
                ParseError::InvalidData(
                    "Rewind anchor chain is unavailable; refusing stale history replay".into(),
                )
            })?;
            selected.push(node.span);
            cursor = node.parent.as_deref();
        }
        selected.reverse();
        Ok(Some(selected))
    }

    fn metadata(&self) -> VecDeque<Value> {
        // Session identity survives an empty rewind; no discarded message text,
        // usage or model survives. Titles are session-level records, like /rename.
        let mut identity = self.identity.clone();
        identity.insert("type".into(), "session-metadata".into());
        let mut values = VecDeque::from([Value::Object(identity)]);
        for key in ["ai-title", "custom-title"] {
            if let Some(value) = self.titles.get(key) {
                values.push_back(value.clone());
            }
        }
        values
    }
}

fn read_span(reader: &mut (impl Read + Seek), span: Span) -> Result<Value, ParseError> {
    reader.seek(SeekFrom::Start(span.offset))?;
    let mut bytes = vec![0; span.len];
    reader.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Two linear passes for legacy files; an index pass plus retained-chain reads
/// for rewound ones. Never retains complete JSON payloads or opens for writing.
pub(super) struct HistoryReader<R> {
    reader: BufReader<R>,
    selected: Option<std::vec::IntoIter<Span>>,
    metadata: VecDeque<Value>,
    line: Vec<u8>,
    remaining: u64,
}

impl<R: Read + Seek> HistoryReader<R> {
    pub(super) fn new(source: R) -> Result<Self, ParseError> {
        let mut reader = BufReader::new(source);
        // Bound both passes to the same snapshot, including legacy files: an
        // anchor appended between passes must not be silently read as metadata.
        let end = reader.seek(SeekFrom::End(0))?;
        reader.seek(SeekFrom::Start(0))?;
        let mut index = Index::default();
        let mut line = Vec::new();
        let mut offset = 0;
        loop {
            line.clear();
            let len = (&mut reader)
                .take(end - offset)
                .read_until(b'\n', &mut line)?;
            if len == 0 {
                break;
            }
            if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                index.observe(&value, Span { offset, len }, &line);
            }
            offset += len as u64;
        }
        let selected = index.selection()?;
        let metadata = if selected.is_some() {
            index.metadata()
        } else {
            VecDeque::new()
        };
        reader.seek(SeekFrom::Start(0))?;
        Ok(Self {
            reader,
            selected: selected.map(Vec::into_iter),
            metadata,
            line: Vec::new(),
            remaining: end,
        })
    }
}

impl<R: Read + Seek> Iterator for HistoryReader<R> {
    type Item = Result<Value, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(value) = self.metadata.pop_front() {
            return Some(Ok(value));
        }
        if let Some(selected) = &mut self.selected {
            return selected
                .next()
                .map(|span| read_span(&mut self.reader, span));
        }
        loop {
            self.line.clear();
            match (&mut self.reader)
                .take(self.remaining)
                .read_until(b'\n', &mut self.line)
            {
                Ok(0) => return None,
                Ok(len) => self.remaining -= len as u64,
                Err(error) => return Some(Err(error.into())),
            }
            if let Ok(value) = serde_json::from_slice::<Value>(&self.line) {
                if !is_sidechain(&value) {
                    return Some(Ok(value));
                }
            }
        }
    }
}

pub(super) struct ReplayLog {
    index: Index,
    bytes: tempfile::SpooledTempFile,
    end: u64,
}

impl Default for ReplayLog {
    fn default() -> Self {
        Self {
            index: Index::default(),
            bytes: tempfile::spooled_tempfile(REPLAY_MEMORY_LIMIT),
            end: 0,
        }
    }
}

impl ReplayLog {
    pub(super) fn observe(&mut self, value: &Value, raw: &[u8]) -> Result<Update, ParseError> {
        let span = Span {
            offset: self.end,
            len: raw.len(),
        };
        if !is_sidechain(value) && chain_record(value) {
            self.bytes.seek(SeekFrom::Start(self.end))?;
            self.bytes.write_all(raw)?;
            self.end += raw.len() as u64;
        }
        Ok(self.index.observe(value, span, raw))
    }

    pub(super) fn replay(&mut self, mut feed: impl FnMut(Value)) -> Result<(), ParseError> {
        let selected = self.index.selection()?.unwrap_or_default();
        for value in self.index.metadata() {
            feed(value);
        }
        for span in selected {
            feed(read_span(&mut self.bytes, span)?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn row(kind: &str, id: &str, parent: Option<&str>) -> Value {
        json!({"type": kind, "uuid": id, "parentUuid": parent})
    }

    fn anchor(leaf: Option<&str>) -> Value {
        json!({"type": "last-prompt", "explicit": true, "leafUuid": leaf})
    }

    fn history() -> Vec<Value> {
        vec![
            row("user", "u1", None),
            row("assistant", "a1", Some("u1")),
            row("attachment", "hidden", Some("a1")),
            row("user", "u2", Some("hidden")),
            row("assistant", "a2", Some("u2")),
        ]
    }

    fn selected(rows: &[Value]) -> Result<Vec<String>, ParseError> {
        let bytes = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        HistoryReader::new(Cursor::new(bytes))?
            .filter_map(|r| match r {
                Ok(v) => v
                    .get("uuid")
                    .and_then(Value::as_str)
                    .map(|s| Ok(s.to_owned())),
                Err(e) => Some(Err(e)),
            })
            .collect()
    }

    #[test]
    fn first_historical_and_latest_anchors() {
        for (leaf, expected) in [
            (None, vec![]),
            (Some("hidden"), vec!["u1", "a1", "hidden"]),
            (Some("a2"), vec!["u1", "a1", "hidden", "u2", "a2"]),
        ] {
            let mut rows = history();
            rows.push(anchor(leaf));
            assert_eq!(selected(&rows).unwrap(), expected);
        }
    }

    #[test]
    fn only_latest_explicit_main_chain_anchor_wins() {
        let mut rows = history();
        rows.extend([anchor(Some("a2")), anchor(Some("a1")),
            json!({"type":"last-prompt", "leafUuid":"a2"}),
            json!({"type":"last-prompt", "explicit":false, "leafUuid":null}),
            json!({"type":"last-prompt", "explicit":true, "leafUuid":null,"isSidechain":true}),
            json!({"type":"last-prompt", "explicit":true, "leafUuid":null,"parent_agent_id":"child"})]);
        assert_eq!(selected(&rows).unwrap(), ["u1", "a1"]);
    }

    #[test]
    fn continuation_after_empty_and_historical_rewind() {
        for leaf in [None, Some("a1")] {
            let mut rows = history();
            rows.extend([
                anchor(leaf),
                row("user", "edited", leaf),
                row("assistant", "reply", Some("edited")),
            ]);
            let expected = if leaf.is_none() {
                vec!["edited", "reply"]
            } else {
                vec!["u1", "a1", "edited", "reply"]
            };
            assert_eq!(selected(&rows).unwrap(), expected);
        }
    }

    #[test]
    fn replayed_ids_and_late_logs_never_advance_leaf() {
        let mut rows = history();
        let stale = rows[4].clone();
        rows.extend([
            anchor(Some("a1")),
            stale,
            row("progress", "progress", Some("a2")),
            row("system", "late", Some("a2")),
            json!({"type":"assistant","uuid":"side","parentUuid":"a2","parent_tool_use_id":"tool"}),
            row("user", "edited", Some("a1")),
        ]);
        assert_eq!(selected(&rows).unwrap(), ["u1", "a1", "edited"]);
    }

    #[test]
    fn latest_duplicate_parent_and_payload_are_authoritative() {
        let mut rows = history();
        rows.push(row("assistant", "a1", None));
        rows.push(anchor(Some("a1")));
        assert_eq!(selected(&rows).unwrap(), ["a1"]);
    }

    fn snapshot(id: &str, parent: Option<&str>) -> Value {
        json!({"type":"attachment","uuid":id,"parentUuid":parent,
            "attachment":{"type":"prompt_snapshot","systemPrompt":["context"]}})
    }

    #[test]
    fn native_continuation_can_reuse_pre_anchor_prompt_snapshot() {
        let mut rows = history();
        rows[2] = snapshot("hidden", Some("a1"));
        rows.extend([
            anchor(Some("a1")),
            row("user", "edited", Some("hidden")),
            row("assistant", "reply", Some("edited")),
        ]);
        assert_eq!(
            selected(&rows).unwrap(),
            ["u1", "a1", "hidden", "edited", "reply"]
        );
        rows.push(anchor(None));
        assert!(selected(&rows).unwrap().is_empty());
    }

    #[test]
    fn snapshot_bridge_never_crosses_discarded_messages_or_competing_branches() {
        for parent in ["u2", "a2", "absent", "snapshot"] {
            let mut rows = history();
            rows.push(snapshot("snapshot", Some(parent)));
            rows.extend([anchor(Some("a1")), row("user", "edited", Some("snapshot"))]);
            assert!(selected(&rows).is_err(), "unsafe bridge through {parent}");
        }
        // An ordinary attachment is not evidence of the CLI snapshot seam.
        let mut rows = history();
        rows.extend([anchor(Some("a1")), row("user", "edited", Some("hidden"))]);
        assert!(selected(&rows).is_err());
        // Two new prompts sharing one old snapshot still compete.
        rows = history();
        rows[2] = snapshot("hidden", Some("a1"));
        rows.extend([
            anchor(Some("a1")),
            row("user", "edited", Some("hidden")),
            row("user", "competing", Some("hidden")),
        ]);
        assert!(selected(&rows).is_err());
        // Empty rewind cannot resurrect even an old root snapshot.
        let rows = vec![
            snapshot("root", None),
            row("user", "old", Some("root")),
            anchor(None),
            row("user", "new", Some("root")),
        ];
        assert!(selected(&rows).is_err());
    }

    #[test]
    fn missing_cycle_competing_and_unidentified_messages_fail_closed() {
        let mut missing = history();
        missing.push(anchor(Some("absent")));
        let cycle = vec![row("user", "loop", Some("loop")), anchor(Some("loop"))];
        let longer_cycle = vec![
            row("user", "u", Some("a")),
            row("assistant", "a", Some("u")),
            anchor(Some("a")),
        ];
        let mut competing = history();
        competing.extend([
            anchor(Some("a1")),
            row("assistant", "late-answer", Some("a2")),
            anchor(None),
        ]);
        let mut unidentified = history();
        unidentified.extend([anchor(Some("a1")), json!({"type":"user", "uuid":"new"})]);
        for rows in [missing, cycle, longer_cycle, competing, unidentified] {
            assert!(selected(&rows).is_err());
        }
    }

    #[test]
    fn legacy_without_explicit_anchor_preserves_order_and_duplicates() {
        let rows = vec![
            row("user", "u", Some("missing")),
            row("user", "u", Some("missing")),
            json!({"type":"last-prompt", "leafUuid":null}),
            json!({"type":"assistant","uuid":"a"}),
        ];
        assert_eq!(selected(&rows).unwrap(), ["u", "u", "a"]);
    }

    #[test]
    fn normal_append_never_rebuilds_and_replay_storage_spills() {
        let mut log = ReplayLog::default();
        let mut root = row("user", "root", None);
        root["message"] = json!({"content": "x".repeat(REPLAY_MEMORY_LIMIT + 1)});
        assert_eq!(
            log.observe(&root, root.to_string().as_bytes()).unwrap(),
            Update::Pass
        );
        assert!(log.bytes.is_rolled());
        let rewind = anchor(Some("root"));
        assert_eq!(
            log.observe(&rewind, rewind.to_string().as_bytes()).unwrap(),
            Update::Rebuild
        );
        log.replay(|_| {}).unwrap();
        let mut parent = "root".to_owned();
        for n in 0..2000 {
            let id = format!("reply-{n}");
            let value = row("assistant", &id, Some(&parent));
            assert_eq!(
                log.observe(&value, value.to_string().as_bytes()).unwrap(),
                Update::Pass
            );
            parent = id;
        }
        let mut count = 0;
        log.replay(|value| {
            if value.get("uuid").is_some() {
                count += 1;
            }
        })
        .unwrap();
        assert_eq!(count, 2001);
    }
}
