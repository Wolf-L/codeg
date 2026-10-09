//! Read-only native guard projection. Never put these bytes into display history.
use super::*;
use serde_json::Value;

fn refuse(reason: &str) -> ParseError {
    ParseError::InvalidData(format!("Native user identity refused: {reason}"))
}

pub(crate) fn native_user_message_text(
    session_id: &str,
    turn: &MessageTurn,
) -> Result<String, ParseError> {
    read_user(&CodexParser::new(), session_id, turn)
}

pub(super) fn validate_rollout(lines: &[String]) -> Result<(), ParseError> {
    for line in lines.iter().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line)?;
        if value["type"] == "session_meta" {
            let p = &value["payload"];
            if let Some(base) = p.get("history_base").filter(|v| !v.is_null()) {
                if base["thread_id"].as_str().is_none_or(|s| s.is_empty())
                    || base["end_ordinal_exclusive"].as_u64().is_none()
                {
                    return Err(refuse("malformed history_base"));
                }
            }
        }
    }
    Ok(())
}

fn text_only(turn: &MessageTurn) -> Result<String, ParseError> {
    if !matches!(turn.role, TurnRole::User) {
        return Err(refuse("selected turn is not a user"));
    }
    let mut result = String::new();
    for block in &turn.blocks {
        match block {
            ContentBlock::Text { text } => result.push_str(text),
            _ => return Err(refuse("attachment identity requires native message ID")),
        }
    }
    if result.is_empty() {
        return Err(refuse("empty user projection"));
    }
    Ok(result)
}

fn read_user(
    parser: &CodexParser,
    session_id: &str,
    selected: &MessageTurn,
) -> Result<String, ParseError> {
    let expected = text_only(selected)?;
    let path = parser
        .find_current_rollout(session_id)
        .ok_or_else(|| ParseError::ConversationNotFound(session_id.to_owned()))?;
    let snapshot = parser.rollout_lines_checked(&path, true)?;
    let lines = active_lines(&snapshot)?;
    let current = parser.parse_conversation_lines(&path, lines.clone(), session_id)?;
    let matching: Vec<_> = current
        .turns
        .iter()
        .filter(|turn| {
            matches!(turn.role, TurnRole::User)
                && text_only(turn).ok().as_deref() == Some(expected.as_str())
        })
        .collect();
    if matching.len() != 1
        || matching[0].id != selected.id
        || matching[0].timestamp != selected.timestamp
    {
        return Err(refuse("selected parsed user is stale or ambiguous"));
    }
    let raw = resolve_records(&lines, &expected)?;
    // Detect append/revert races across the parse and identity projection. The
    // provider still validates the fingerprint atomically at the actual rewind.
    if parser.find_current_rollout(session_id).as_ref() != Some(&path)
        || parser.rollout_lines_checked(&path, true)? != snapshot
    {
        return Err(refuse("rollout changed while resolving identity; reload"));
    }
    Ok(raw)
}

/// Replay legacy rollback against native task boundaries, not UI turn indices.
/// Steered users belong to the same task. Without boundaries we cannot prove
/// how many tasks a rollback removed, so refuse instead of retaining a stale tail.
fn active_lines(lines: &[String]) -> Result<Vec<String>, ParseError> {
    let mut kept = Vec::new();
    let mut starts = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)?;
        if value["type"] == "event_msg" {
            match value["payload"]["type"].as_str() {
                Some("task_started") => starts.push(kept.len()),
                Some("thread_rolled_back") => {
                    let count = value["payload"]["num_turns"]
                        .as_u64()
                        .ok_or_else(|| refuse("malformed rollback"))?;
                    if count != 0 {
                        if starts.is_empty() {
                            return Err(refuse("rollback has no task boundaries"));
                        }
                        let remain = starts
                            .len()
                            .saturating_sub(usize::try_from(count).unwrap_or(usize::MAX));
                        kept.truncate(starts[remain]);
                        starts.truncate(remain);
                    }
                    continue;
                }
                _ => {}
            }
        }
        kept.push(line.clone());
    }
    Ok(kept)
}

struct Candidate {
    display: String,
    raw: Result<String, ParseError>,
}

fn safe_text(parts: &[&str]) -> Result<String, ParseError> {
    // ACP converts a Desktop envelope to resource links + request text. Until
    // that conversion is proven here, hashing its raw envelope would be wrong.
    if parts.iter().any(|s| s.trim_start().starts_with("# Files ")) {
        return Err(refuse(
            "Desktop attachment conversion requires native message ID",
        ));
    }
    Ok(parts.concat())
}

fn content_candidate(content: &Value, tag: &str) -> Result<Candidate, ParseError> {
    let items = content
        .as_array()
        .ok_or_else(|| refuse("unknown user content shape"))?;
    let mut parts = Vec::new();
    let mut unsupported = false;
    for item in items {
        if item["type"].as_str() == Some(tag) {
            parts.push(
                item["text"]
                    .as_str()
                    .ok_or_else(|| refuse("missing user text"))?,
            );
        } else {
            // Images, skills, audio and resource links have ACP-specific visible
            // conversions. Never silently hash only their text subset.
            unsupported = true;
        }
    }
    Ok(Candidate {
        display: normalize_user_text(
            &parts
                .iter()
                .copied()
                .filter(|text| !text.is_empty() && text.trim() != "<image>")
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        raw: if unsupported {
            Err(refuse("unsupported native attachment conversion"))
        } else {
            safe_text(&parts)
        },
    })
}

/// Canonical UserMessage items outrank their legacy/event and response twins
/// within the same task. They are exactly the UserInput[] read by SessionRewind.
fn resolve_records(lines: &[String], expected: &str) -> Result<String, ParseError> {
    let mut candidates = Vec::new();
    let mut canonical = Vec::new();
    let mut events = Vec::new();
    let mut responses = Vec::new();
    let flush = |all: &mut Vec<Candidate>,
                 canonical: &mut Vec<Candidate>,
                 events: &mut Vec<Candidate>,
                 responses: &mut Vec<Candidate>| {
        if !canonical.is_empty() {
            // Normalization is many-to-one. A response/event twin with the
            // same display but different raw bytes is not evidence of identity.
            for item in canonical.iter_mut() {
                if events.iter().chain(responses.iter()).any(|twin| {
                    twin.display == item.display
                        && match (&twin.raw, &item.raw) {
                            (Ok(a), Ok(b)) => a != b,
                            _ => true,
                        }
                }) {
                    item.raw = Err(refuse("native and parsed user records disagree"));
                }
            }
            all.append(canonical);
        } else if !events.is_empty() {
            all.append(events);
        } else {
            all.append(responses);
        }
        canonical.clear();
        events.clear();
        responses.clear();
    };
    for line in lines {
        let value: Value = serde_json::from_str(line)?;
        let p = &value["payload"];
        if value["type"] == "event_msg" {
            match p["type"].as_str() {
                Some("task_started" | "task_complete") => {
                    flush(&mut candidates, &mut canonical, &mut events, &mut responses)
                }
                Some("item_completed") if p["item"]["type"] == "UserMessage" => {
                    canonical.push(content_candidate(&p["item"]["content"], "text")?);
                }
                Some("user_message") => {
                    let raw = p["message"]
                        .as_str()
                        .ok_or_else(|| refuse("missing user message"))?;
                    let attachments =
                        ["images", "file_ids", "local_images", "audio", "local_audio"]
                            .iter()
                            .any(|key| {
                                p.get(*key).is_some_and(|v| {
                                    !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty())
                                })
                            });
                    events.push(Candidate {
                        display: normalize_user_text(raw),
                        raw: if attachments {
                            Err(refuse(
                                "legacy attachment conversion requires native message ID",
                            ))
                        } else {
                            safe_text(&[raw])
                        },
                    });
                }
                _ => {}
            }
        } else if value["type"] == "response_item" && p["type"] == "message" && p["role"] == "user"
        {
            let candidate = content_candidate(&p["content"], "input_text")?;
            if is_promotable_user_text(&candidate.display) {
                responses.push(candidate);
            }
        }
    }
    flush(&mut candidates, &mut canonical, &mut events, &mut responses);
    let mut matches = candidates.into_iter().filter(|candidate| {
        candidate.display == expected
            || candidate
                .raw
                .as_ref()
                .is_ok_and(|raw| normalize_user_text(raw) == expected)
    });
    let matched = matches
        .next()
        .ok_or_else(|| refuse("no matching native user record"))?;
    if matches.next().is_some() {
        return Err(refuse("multiple native users share the parsed text"));
    }
    matched.raw
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(kind: &str, payload: Value) -> String {
        json!({"timestamp":"2026-10-09T04:54:44.599Z", "type":kind, "payload":payload}).to_string()
    }
    fn user(raw: &str) -> String {
        line("event_msg", json!({"type":"user_message", "message":raw}))
    }
    #[test]
    fn native_guard_preserves_e2e_whitespace_and_prefers_canonical() {
        let raw = " \t\nCODEX_KEEP\u{a0}  \r\n";
        let lines = vec![
            line(
                "response_item",
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":raw}]}),
            ),
            line(
                "event_msg",
                json!({"type":"item_completed","item":{"type":"UserMessage","id":"native-id","content":[{"type":"text","text":raw,"text_elements":[]}]}}),
            ),
        ];
        assert_eq!(resolve_records(&lines, "CODEX_KEEP").unwrap(), raw);
        let parts = json!([{"type":"text","text":" A  "},{"type":"text","text":"B\t "}]);
        assert_eq!(
            content_candidate(&parts, "text").unwrap().raw.unwrap(),
            " A  B\t "
        );
    }
    #[test]
    fn native_guard_refuses_ambiguous_and_attachments() {
        assert!(resolve_records(&[user(" same  text "), user("same text")], "same text").is_err());
        let image = line(
            "event_msg",
            json!({"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"look"},{"type":"image","url":"data:image/png;base64,AA"}]}}),
        );
        assert!(resolve_records(&[image], "look").is_err());
        assert!(safe_text(&["# Files pasted by the user:\n..."]).is_err());
        let canonical = line(
            "event_msg",
            json!({"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"same text"}]}}),
        );
        assert!(resolve_records(&[user("same  text"), canonical], "same text").is_err());
    }

    #[test]
    fn native_guard_uses_current_revert_lineage() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        super::super::tests::reverted_thread_fixture(&sessions, "native-thread", "native-revert");
        let parser = CodexParser::with_base_dir(sessions);
        let detail = parser.get_conversation("native-thread").unwrap();
        for turn in detail
            .turns
            .iter()
            .filter(|t| matches!(t.role, TurnRole::User))
        {
            assert_eq!(
                read_user(&parser, "native-thread", turn).unwrap(),
                text_only(turn).unwrap()
            );
        }
    }
    #[test]
    fn native_guard_rollback_drops_whole_task_including_steers() {
        let start = || line("event_msg", json!({"type":"task_started"}));
        let lines = vec![
            start(),
            user("keep"),
            start(),
            user("drop"),
            user("steer"),
            line(
                "event_msg",
                json!({"type":"thread_rolled_back","num_turns":1}),
            ),
            start(),
            user("new"),
        ];
        let active = active_lines(&lines).unwrap();
        assert_eq!(resolve_records(&active, "keep").unwrap(), "keep");
        assert_eq!(resolve_records(&active, "new").unwrap(), "new");
        assert!(resolve_records(&active, "drop").is_err());
        assert!(resolve_records(&active, "steer").is_err());
    }
    #[test]
    fn native_guard_reads_cold_current_rollout_and_refuses_stale_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-2026-10-09T12-54-44-fixture.jsonl");
        let raw = " \t\nCODEX_KEEP\u{a0}  \r\n";
        fs::write(
            &path,
            [line("session_meta", json!({"id":"fixture"})), user(raw)].join("\n"),
        )
        .unwrap();
        let parser = CodexParser::with_base_dir(dir.path().to_owned());
        let detail = parser.get_conversation("fixture").unwrap();
        assert_eq!(
            read_user(&parser, "fixture", &detail.turns[0]).unwrap(),
            raw
        );
        let mut stale = detail.turns[0].clone();
        stale.id = "turn-9".into();
        assert!(read_user(&parser, "fixture", &stale).is_err());
    }

    #[test]
    fn native_guard_multiblock_display_matches_parser_but_hash_text_is_concat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-2026-10-09T12-54-44-multi.jsonl");
        let parts = [" \tA  ", "", "B\t \r\n"];
        let lines = [
            line(
                "session_meta",
                json!({"id":"multi","history_mode":"paginated"}),
            ),
            line(
                "event_msg",
                json!({"type":"task_started","turn_id":"task-1"}),
            ),
            line(
                "response_item",
                json!({"type":"message","role":"user","content":parts.iter().map(|text| json!({"type":"input_text","text":text})).collect::<Vec<_>>()}),
            ),
            line(
                "event_msg",
                json!({"type":"item_completed","item":{"type":"UserMessage","id":"native-1","content":parts.iter().map(|text| json!({"type":"text","text":text,"text_elements":[]})).collect::<Vec<_>>()}}),
            ),
        ];
        fs::write(path, lines.join("\n")).unwrap();
        let parser = CodexParser::with_base_dir(dir.path().to_owned());
        let detail = parser.get_conversation("multi").unwrap();
        assert_eq!(
            text_only(&detail.turns[0]).unwrap(),
            normalize_user_text(" \tA  \nB\t \r\n")
        );
        assert_eq!(
            read_user(&parser, "multi", &detail.turns[0]).unwrap(),
            parts.concat()
        );
        assert_ne!(
            normalize_user_text(&parts.join("\n")),
            normalize_user_text(&parts.concat())
        );
    }
}
