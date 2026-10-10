//! Fixed native extensions negotiated from initialize metadata.
//!
//! This module owns no transport or lifecycle. The connection must serialize
//! mutations, consume queue notifications, and fence uncertain outcomes. The
//! public manager route must resolve rewind turn IDs from freshly parsed bound
//! history before calling `prepare_request`; it must never accept caller-authored
//! `beforeMessage` or `resumeAtMessage` guards.

use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::acp::error::AcpError;
use crate::acp::js_text::is_js_blank;
use crate::models::agent::AgentType;
use crate::models::message::{ContentBlock, MessageTurn, TurnRole};

const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const RUNTIME_READ: &str = "_session/runtime/read";
const RUNTIME_CONTROL: &str = "_session/runtime/control";
const REWIND: &str = "_session/rewind";
const REWIND_FILES: &str = "_session/rewind_files";
const FILE_REVERT: &str = "_session/files/revert";
const QUEUE: &str = "_session/queue";
const QUEUE_CHANGED: &str = "_session/queue/changed";
const QUEUE_TURN: &str = "_session/queue/turn";
const MCP_STATE: &str = "_session/mcp/state";
const MCP_SET: &str = "_session/mcp/set";
const ARCHIVE: &str = "_session/archive";
const UNARCHIVE: &str = "_session/unarchive";
const SEARCH: &str = "_session/search";
const ATTACHMENTS: &str = "_session/attachments";
const GOAL: &str = "_session/goal";
const READS: &[&str] = &[
    "context",
    "usage",
    "mcp",
    "commands",
    "plugins",
    "agents",
    "queuedMessages",
];
const CONTROLS: &[&str] = &[
    "reloadSkills",
    "reconnectMcp",
    "reloadPlugins",
    "reloadOutputStyles",
    "toggleMcp",
    "backgroundTask",
    "cancelQueuedMessage",
];
const QUEUE_ACTIONS: &[&str] = &["list", "add", "update", "delete", "reorder", "start"];
const GOAL_ACTIONS: &[&str] = &["set", "pause", "resume", "clear"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeOperation {
    RuntimeRead,
    RuntimeControl,
    Rewind,
    RewindFiles,
    WorkspaceRewindFiles,
    FileRevert,
    Queue,
    McpState,
    McpSet,
    Archive,
    Unarchive,
    Search,
    Attachments,
    Goal,
}

impl NativeOperation {
    /// Invalid actions are conservatively classified as mutations; validation
    /// must still run before dispatch, even when this returns false.
    pub fn is_mutation(self, params: &Value) -> bool {
        match self {
            Self::RuntimeRead | Self::McpState | Self::Search => false,
            Self::Queue | Self::Attachments => {
                params.get("action").and_then(Value::as_str) != Some("list")
            }
            Self::RewindFiles | Self::FileRevert | Self::WorkspaceRewindFiles => {
                params.get("dryRun") != Some(&Value::Bool(true))
            }
            _ => true,
        }
    }

    pub fn requires_idle(self, params: &Value) -> bool {
        match self {
            Self::RuntimeRead | Self::McpState | Self::Search => false,
            Self::Queue => !params
                .get("action")
                .and_then(Value::as_str)
                .is_some_and(|action| QUEUE_ACTIONS.contains(&action)),
            Self::RuntimeControl => !matches!(
                params.get("action").and_then(Value::as_str),
                Some("cancelQueuedMessage" | "backgroundTask")
            ),
            Self::Attachments => self.is_mutation(params),
            // Native file previews also require an idle, stable session.
            _ => true,
        }
    }
}

fn invalid() -> AcpError {
    // Never echo values, unknown field names, attachment payloads or credentials.
    AcpError::Protocol("Invalid native session request".into())
}

fn unsupported() -> AcpError {
    AcpError::Protocol("Native session operation was not advertised or is unsupported".into())
}

fn history_error() -> AcpError {
    AcpError::Protocol(
        "Cannot safely identify the native rewind boundary; reload persisted history".into(),
    )
}

fn version_one(value: &Value) -> bool {
    value.get("version").and_then(Value::as_u64) == Some(1)
}

fn copy_literal(source: &Value, dest: &mut Map<String, Value>, key: &str, allowed: &[&str]) {
    if let Some(value) = source.get(key).and_then(Value::as_str) {
        if allowed.contains(&value) {
            dest.insert(key.into(), Value::String(value.into()));
        }
    }
}

fn copy_list(source: &Value, dest: &mut Map<String, Value>, key: &str, allowed: &[&str]) {
    if let Some(values) = source.get(key).and_then(Value::as_array) {
        // Emit only fixed vocabulary, once each. No untrusted string crosses
        // this boundary, including inside an otherwise known advertisement.
        dest.insert(
            key.into(),
            Value::Array(
                allowed
                    .iter()
                    .filter(|item| values.iter().any(|v| v.as_str() == Some(**item)))
                    .map(|item| json!(item))
                    .collect(),
            ),
        );
    }
}

fn sanitize_capability(key: &str, source: &Value) -> Option<Value> {
    if !version_one(source) {
        return None;
    }
    let mut out = Map::new();
    out.insert("version".into(), json!(1));
    type CapabilityFields<'a> = (
        &'a [(&'a str, &'a str)],
        &'a [(&'a str, &'a [&'a str])],
        &'a [&'a str],
    );
    let (methods, lists, flags): CapabilityFields<'_> = match key {
        "runtime" => (
            &[
                ("readMethod", RUNTIME_READ),
                ("controlMethod", RUNTIME_CONTROL),
            ],
            &[("reads", READS), ("controls", CONTROLS)],
            &[],
        ),
        "sessionRewind" => (
            &[("method", REWIND)],
            &[],
            &[
                "sameSession",
                "durable",
                "firstMessage",
                "interruptIfRunning",
                "changesFiles",
            ],
        ),
        "sessionRewindFiles" => (
            &[("method", REWIND_FILES)],
            &[],
            &["dryRun", "changesConversation", "requiresCheckpoints"],
        ),
        "fileRevert" => (
            &[("method", FILE_REVERT)],
            &[],
            &[
                "dryRun",
                "previewTokenRequired",
                "requiresGit",
                "changesConversation",
            ],
        ),
        "queue" => (
            &[
                ("method", QUEUE),
                ("changedNotification", QUEUE_CHANGED),
                ("turnNotification", QUEUE_TURN),
            ],
            &[("actions", QUEUE_ACTIONS)],
            &[],
        ),
        "sessionMcp" => (
            &[("stateMethod", MCP_STATE), ("setMethod", MCP_SET)],
            &[],
            &["replacesSettingsServers", "replacesPluginServers"],
        ),
        "archive" => (
            &[("archiveMethod", ARCHIVE), ("unarchiveMethod", UNARCHIVE)],
            &[],
            &["permanentDelete"],
        ),
        "discovery" => (
            &[("searchMethod", SEARCH), ("attachmentMethod", ATTACHMENTS)],
            &[("attachmentActions", &["list", "add", "remove"])],
            &["attachmentsAreMetadata"],
        ),
        "goal" => (
            &[("controlMethod", GOAL)],
            &[("actions", GOAL_ACTIONS)],
            &[],
        ),
        _ => return None,
    };
    for (field, method) in methods {
        copy_literal(source, &mut out, field, &[*method]);
    }
    for (field, values) in lists {
        copy_list(source, &mut out, field, values);
    }
    for field in flags {
        if let Some(value) = source.get(*field).and_then(Value::as_bool) {
            out.insert((*field).into(), json!(value));
        }
    }
    match key {
        "runtime" => {
            if let Some(context) = source.get("context").filter(|v| v.is_object()) {
                let mut clean = Map::new();
                copy_list(context, &mut clean, "details", &["summary", "full"]);
                copy_literal(context, &mut clean, "defaultDetail", &["summary"]);
                if let Some(value) = context.get("fullMayUseNetwork").and_then(Value::as_bool) {
                    clean.insert("fullMayUseNetwork".into(), json!(value));
                }
                out.insert("context".into(), Value::Object(clean));
            }
            if let Some(queue) = source.get("queuedMessages").filter(|v| v.is_object()) {
                let mut clean = Map::new();
                copy_literal(queue, &mut clean, "scope", &["adapter_prompts"]);
                copy_literal(queue, &mut clean, "runtimeSupport", &["checked_on_request"]);
                copy_literal(queue, &mut clean, "cancellation", &["pending_only"]);
                out.insert("queuedMessages".into(), Value::Object(clean));
            }
        }
        "sessionRewind" => {
            copy_literal(source, &mut out, "runtimeSupport", &["checked_on_request"])
        }
        "sessionMcp" => copy_literal(source, &mut out, "scope", &["acpServers"]),
        "fileRevert" => copy_literal(source, &mut out, "scope", &["nativeFileChangeTool"]),
        _ => {}
    }
    Some(Value::Object(out))
}

/// Returns sanitized initialize `_meta`, retaining its native namespaces.
/// Unknown fields, free-form descriptions, launch configuration and credentials
/// are never copied. Absence and incompatible versions never imply support.
pub fn capabilities(meta: Option<&Map<String, Value>>) -> Value {
    let mut out = Map::new();
    let Some(meta) = meta else {
        return Value::Object(out);
    };
    for key in [
        "runtime",
        "sessionRewind",
        "sessionRewindFiles",
        "fileRevert",
        "queue",
        "sessionMcp",
        "archive",
        "discovery",
        "goal",
    ] {
        if let Some(value) = meta.get(key).and_then(|v| sanitize_capability(key, v)) {
            out.insert(key.into(), value);
        }
    }
    if let Some(air) = meta
        .get("jetbrains")
        .and_then(|v| v.get("air"))
        .filter(|v| version_one(v))
    {
        let mut clean = Map::new();
        clean.insert("version".into(), json!(1));
        copy_list(air, &mut clean, "capabilities", &["sessionRewind"]);
        if let Some(goal) = air.get("goal") {
            // Keep an invalid AIR goal as an empty advertisement so a different
            // top-level goal cannot silently take precedence after sanitizing.
            clean.insert(
                "goal".into(),
                sanitize_capability("goal", goal).unwrap_or_else(|| json!({})),
            );
        }
        out.insert("jetbrains".into(), json!({"air": clean}));
    }
    Value::Object(out)
}

fn advertised<'a>(
    caps: &'a Value,
    key: &str,
    field: &str,
    method: &str,
) -> Result<&'a Value, AcpError> {
    let cap = caps.get(key).ok_or_else(unsupported)?;
    verify_method(cap, field, method)?;
    Ok(cap)
}

fn verify_method(cap: &Value, field: &str, method: &str) -> Result<(), AcpError> {
    if version_one(cap) && cap.get(field).and_then(Value::as_str) == Some(method) {
        Ok(())
    } else {
        Err(unsupported())
    }
}

fn advertised_choice(cap: &Value, key: &str, choice: &str, known: &[&str]) -> Result<(), AcpError> {
    if known.contains(&choice)
        && cap
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().any(|v| v.as_str() == Some(choice)))
    {
        Ok(())
    } else {
        Err(unsupported())
    }
}

fn object(value: &Value) -> Result<&Map<String, Value>, AcpError> {
    value.as_object().ok_or_else(invalid)
}

fn keys(map: &Map<String, Value>, allowed: &[&str]) -> Result<(), AcpError> {
    if map.keys().all(|key| allowed.contains(&key.as_str())) {
        Ok(())
    } else {
        Err(invalid())
    }
}

fn string<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a str, AcpError> {
    map.get(key).and_then(Value::as_str).ok_or_else(invalid)
}

fn valid_id(value: &str) -> bool {
    !is_js_blank(value)
        && value.encode_utf16().count() <= 4096
        && !value.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}')
}

fn identifier(map: &Map<String, Value>, key: &str) -> Result<(), AcpError> {
    if valid_id(string(map, key)?) {
        Ok(())
    } else {
        Err(invalid())
    }
}

fn boolean(map: &Map<String, Value>, key: &str) -> Result<bool, AcpError> {
    map.get(key).and_then(Value::as_bool).ok_or_else(invalid)
}

fn integer(value: &Value, min: u64, max: u64) -> Result<u64, AcpError> {
    value
        .as_u64()
        .filter(|n| *n >= min && *n <= max)
        .ok_or_else(invalid)
}

fn hash_string(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn history_point(value: &Value) -> Result<(), AcpError> {
    let map = object(value)?;
    keys(
        map,
        &["messageId", "messageFingerprint", "messageOccurrence"],
    )?;
    identifier(map, "messageId")?;
    if !hash_string(string(map, "messageFingerprint")?) {
        return Err(invalid());
    }
    integer(
        map.get("messageOccurrence").ok_or_else(invalid)?,
        1,
        MAX_SAFE_INTEGER,
    )?;
    Ok(())
}

/// Count encoded bytes without allocating a second unbounded request buffer.
fn bounded_json(value: &Value, limit: usize) -> Result<(), AcpError> {
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| io::Error::other("JSON size limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(limit), value).map_err(|_| invalid())
}

fn page(map: &Map<String, Value>, queue: bool) -> Result<(), AcpError> {
    if let Some(cursor) = map.get("cursor").filter(|v| !v.is_null()) {
        let cursor = cursor.as_str().ok_or_else(invalid)?;
        if !queue && !valid_id(cursor) {
            return Err(invalid());
        }
    }
    if let Some(limit) = map.get("limit") {
        if !(queue && limit.is_null()) {
            integer(
                limit,
                if queue { 0 } else { 1 },
                if queue { u32::MAX.into() } else { 100 },
            )?;
        }
    }
    Ok(())
}

fn queue_input(value: &Value) -> Result<(), AcpError> {
    for input in value.as_array().ok_or_else(invalid)? {
        let map = object(input)?;
        match string(map, "type")? {
            "text" => {
                keys(map, &["type", "text", "text_elements"])?;
                string(map, "text")?;
                if let Some(elements) = map.get("text_elements") {
                    for element in elements.as_array().ok_or_else(invalid)? {
                        let element = object(element)?;
                        keys(element, &["byteRange", "placeholder"])?;
                        let range = object(element.get("byteRange").ok_or_else(invalid)?)?;
                        keys(range, &["start", "end"])?;
                        for key in ["start", "end"] {
                            integer(range.get(key).ok_or_else(invalid)?, 0, MAX_SAFE_INTEGER)?;
                        }
                        if let Some(placeholder) = element.get("placeholder") {
                            if !placeholder.is_null() && !placeholder.is_string() {
                                return Err(invalid());
                            }
                        }
                    }
                }
            }
            "image" => {
                // Exactly one of url/fileId. ACP data/mimeType is not UserInput.
                let source = if map.contains_key("url") {
                    "url"
                } else {
                    "fileId"
                };
                keys(map, &["type", source, "detail"])?;
                string(map, source)?;
                image_detail(map)?;
            }
            "localImage" => {
                keys(map, &["type", "path", "detail"])?;
                string(map, "path")?;
                image_detail(map)?;
            }
            "audio" | "localAudio" => {
                let source = if string(map, "type")? == "audio" {
                    "url"
                } else {
                    "path"
                };
                keys(map, &["type", source])?;
                string(map, source)?;
            }
            "skill" | "mention" => {
                keys(map, &["type", "name", "path"])?;
                string(map, "name")?;
                string(map, "path")?;
            }
            _ => return Err(invalid()),
        }
    }
    Ok(())
}

fn image_detail(map: &Map<String, Value>) -> Result<(), AcpError> {
    if let Some(detail) = map.get("detail").filter(|v| !v.is_null()) {
        if !matches!(detail.as_str(), Some("auto" | "low" | "high" | "original")) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn validate_queue(map: &Map<String, Value>, action: &str) -> Result<(), AcpError> {
    match action {
        "list" => {
            keys(map, &["action", "cursor", "limit"])?;
            page(map, true)?;
        }
        "add" | "update" => {
            let id = if action == "add" {
                "clientUserMessageId"
            } else {
                "queuedSubmissionId"
            };
            keys(map, &["action", "input", id])?;
            identifier(map, id)?;
            queue_input(map.get("input").ok_or_else(invalid)?)?;
        }
        "delete" => {
            keys(map, &["action", "queuedSubmissionId"])?;
            identifier(map, "queuedSubmissionId")?;
        }
        "reorder" => {
            keys(map, &["action", "queuedSubmissionIds"])?;
            for id in map
                .get("queuedSubmissionIds")
                .and_then(Value::as_array)
                .ok_or_else(invalid)?
            {
                if !id.as_str().is_some_and(valid_id) {
                    return Err(invalid());
                }
            }
        }
        "start" => {
            keys(map, &["action", "queuedSubmissionId"])?;
            if map.get("queuedSubmissionId").is_some_and(|v| !v.is_null()) {
                identifier(map, "queuedSubmissionId")?;
            }
        }
        _ => return Err(invalid()),
    }
    Ok(())
}

/// Validate a fixed operation and inject the connection's bound session ID.
/// Search alone is connection-scoped: its native strict schema has no sessionId.
/// Rewind guards here are INTERNAL manager output, not public route input.
pub fn prepare_request(
    caps: &Value,
    agent: AgentType,
    session_id: &str,
    operation: NativeOperation,
    params: Value,
) -> Result<(&'static str, Value), AcpError> {
    if !matches!(agent, AgentType::Codex | AgentType::ClaudeCode) {
        return Err(unsupported());
    }
    if !valid_id(session_id) {
        return Err(invalid());
    }
    bounded_json(&params, MAX_REQUEST_BYTES)?;
    let map = object(&params)?;
    // Even the bound value is rejected if supplied by the caller. All other
    // identity aliases and caller-selected methods fail the per-action keys.
    if map.contains_key("sessionId") {
        return Err(invalid());
    }
    let codex = agent == AgentType::Codex;
    let method = match operation {
        // Host-only operation: manager intercepts it before adapter dispatch.
        NativeOperation::WorkspaceRewindFiles => return Err(unsupported()),
        NativeOperation::RuntimeRead => {
            let cap = advertised(caps, "runtime", "readMethod", RUNTIME_READ)?;
            let resource = string(map, "resource")?;
            let known: &[&str] = if codex {
                &["context", "usage", "mcp", "commands", "plugins"]
            } else {
                &[
                    "context",
                    "usage",
                    "mcp",
                    "commands",
                    "agents",
                    "queuedMessages",
                ]
            };
            advertised_choice(cap, "reads", resource, known)?;
            if !codex && resource == "context" {
                keys(map, &["resource", "detail"])?;
                if let Some(detail) = map.get("detail") {
                    let detail = detail.as_str().ok_or_else(invalid)?;
                    advertised_choice(
                        cap.get("context").ok_or_else(unsupported)?,
                        "details",
                        detail,
                        &["summary", "full"],
                    )?;
                }
            } else {
                keys(map, &["resource"])?;
            }
            RUNTIME_READ
        }
        NativeOperation::RuntimeControl => {
            let cap = advertised(caps, "runtime", "controlMethod", RUNTIME_CONTROL)?;
            let action = string(map, "action")?;
            advertised_choice(cap, "controls", action, CONTROLS)?;
            if codex {
                if !["reloadSkills", "reloadPlugins", "reconnectMcp"].contains(&action) {
                    return Err(unsupported());
                }
                keys(map, &["action"])?;
            } else {
                match action {
                    "reloadSkills" | "reloadOutputStyles" => keys(map, &["action"])?,
                    "reloadPlugins" => {
                        keys(map, &["action", "holdOnCacheImpact"])?;
                        if map.contains_key("holdOnCacheImpact") {
                            boolean(map, "holdOnCacheImpact")?;
                        }
                    }
                    "reconnectMcp" | "toggleMcp" => {
                        if action == "toggleMcp" {
                            keys(map, &["action", "serverName", "enabled"])?;
                            boolean(map, "enabled")?;
                        } else {
                            keys(map, &["action", "serverName"])?;
                        }
                        identifier(map, "serverName")?;
                    }
                    "backgroundTask" | "cancelQueuedMessage" => {
                        let key = if action == "backgroundTask" {
                            "toolUseId"
                        } else {
                            "messageId"
                        };
                        keys(map, &["action", key])?;
                        identifier(map, key)?;
                    }
                    _ => return Err(unsupported()),
                }
            }
            RUNTIME_CONTROL
        }
        NativeOperation::Rewind => {
            if caps.get("sessionRewind").is_some() {
                advertised(caps, "sessionRewind", "method", REWIND)?;
            } else if codex {
                // This versioned AIR token is Codex's actual advertisement;
                // unlike Claude it does not publish a top-level method object.
                let air = caps
                    .pointer("/jetbrains/air")
                    .filter(|v| version_one(v))
                    .ok_or_else(unsupported)?;
                advertised_choice(air, "capabilities", "sessionRewind", &["sessionRewind"])?;
            } else {
                return Err(unsupported());
            }
            if codex {
                keys(map, &["beforeMessage", "resumeAtMessage"])?;
            } else {
                keys(
                    map,
                    &["beforeMessage", "resumeAtMessage", "interruptIfRunning"],
                )?;
                if map.contains_key("interruptIfRunning") {
                    boolean(map, "interruptIfRunning")?;
                }
            }
            history_point(map.get("beforeMessage").ok_or_else(invalid)?)?;
            if let Some(point) = map.get("resumeAtMessage") {
                history_point(point)?;
            }
            REWIND
        }
        NativeOperation::RewindFiles => {
            if codex {
                return Err(unsupported());
            }
            advertised(caps, "sessionRewindFiles", "method", REWIND_FILES)?;
            keys(map, &["beforeMessage", "dryRun"])?;
            history_point(map.get("beforeMessage").ok_or_else(invalid)?)?;
            boolean(map, "dryRun")?;
            REWIND_FILES
        }
        NativeOperation::FileRevert => {
            if !codex {
                return Err(unsupported());
            }
            advertised(caps, "fileRevert", "method", FILE_REVERT)?;
            keys(map, &["toolCallId", "dryRun", "previewToken"])?;
            identifier(map, "toolCallId")?;
            let preview = boolean(map, "dryRun")?;
            if (map.contains_key("previewToken") || !preview)
                && !hash_string(string(map, "previewToken")?)
            {
                return Err(invalid());
            }
            FILE_REVERT
        }
        NativeOperation::Queue => {
            if !codex {
                return Err(unsupported());
            }
            let cap = advertised(caps, "queue", "method", QUEUE)?;
            let action = string(map, "action")?;
            advertised_choice(cap, "actions", action, QUEUE_ACTIONS)?;
            if action != "list"
                && (cap.get("changedNotification").and_then(Value::as_str) != Some(QUEUE_CHANGED)
                    || cap.get("turnNotification").and_then(Value::as_str) != Some(QUEUE_TURN))
            {
                return Err(unsupported());
            }
            validate_queue(map, action)?;
            QUEUE
        }
        NativeOperation::McpState => {
            if codex {
                return Err(unsupported());
            }
            advertised(caps, "sessionMcp", "stateMethod", MCP_STATE)?;
            keys(map, &[])?;
            MCP_STATE
        }
        NativeOperation::McpSet => {
            if codex {
                return Err(unsupported());
            }
            advertised(caps, "sessionMcp", "setMethod", MCP_SET)?;
            // Internal host instruction, NOT the final native wire schema.
            // Connection must replace mode with the stored host-owned servers
            // and protected companion entries before dispatching this method.
            keys(map, &["expectedRevision", "mode"])?;
            if string(map, "mode")? != "reloadConfigured" {
                return Err(invalid());
            }
            integer(
                map.get("expectedRevision").ok_or_else(invalid)?,
                0,
                MAX_SAFE_INTEGER - 1,
            )?;
            MCP_SET
        }
        NativeOperation::Archive | NativeOperation::Unarchive => {
            if !codex {
                return Err(unsupported());
            }
            let (field, method) = if operation == NativeOperation::Archive {
                ("archiveMethod", ARCHIVE)
            } else {
                ("unarchiveMethod", UNARCHIVE)
            };
            advertised(caps, "archive", field, method)?;
            keys(map, &[])?;
            method
        }
        NativeOperation::Search => {
            if !codex {
                return Err(unsupported());
            }
            advertised(caps, "discovery", "searchMethod", SEARCH)?;
            keys(map, &["searchTerm", "archived", "cursor", "limit"])?;
            let term = string(map, "searchTerm")?;
            if is_js_blank(term) || term.encode_utf16().count() > 4096 {
                return Err(invalid());
            }
            if map.contains_key("archived") {
                boolean(map, "archived")?;
            }
            page(map, false)?;
            SEARCH
        }
        NativeOperation::Attachments => {
            if !codex {
                return Err(unsupported());
            }
            let cap = advertised(caps, "discovery", "attachmentMethod", ATTACHMENTS)?;
            let action = string(map, "action")?;
            advertised_choice(cap, "attachmentActions", action, &["list", "add", "remove"])?;
            if action == "list" {
                keys(map, &["action", "cursor", "limit"])?;
                page(map, false)?;
            } else {
                if action == "add" {
                    keys(map, &["action", "attachmentType", "identityKey", "payload"])?;
                    bounded_json(map.get("payload").ok_or_else(invalid)?, 256 * 1024)?;
                } else {
                    keys(map, &["action", "attachmentType", "identityKey"])?;
                }
                identifier(map, "attachmentType")?;
                identifier(map, "identityKey")?;
            }
            ATTACHMENTS
        }
        NativeOperation::Goal => {
            let cap = if let Some(air) = caps
                .pointer("/jetbrains/air")
                .filter(|air| air.get("goal").is_some())
            {
                if !version_one(air) {
                    return Err(unsupported());
                }
                &air["goal"]
            } else {
                caps.get("goal").ok_or_else(unsupported)?
            };
            verify_method(cap, "controlMethod", GOAL)?;
            let action = string(map, "action")?;
            advertised_choice(
                cap,
                "actions",
                action,
                if codex {
                    GOAL_ACTIONS
                } else {
                    &["set", "clear"]
                },
            )?;
            if action == "set" {
                keys(map, &["action", "objective"])?;
                if is_js_blank(string(map, "objective")?) {
                    return Err(invalid());
                }
            } else {
                keys(map, &["action"])?;
            }
            GOAL
        }
    };
    let mut body = params.as_object().cloned().ok_or_else(invalid)?;
    if operation != NativeOperation::Search {
        body.insert("sessionId".into(), json!(session_id));
    }
    if operation == NativeOperation::RuntimeRead
        && !codex
        && body.get("resource").and_then(Value::as_str) == Some("context")
    {
        body.entry("detail").or_insert_with(|| json!("summary"));
    }
    let body = Value::Object(body);
    bounded_json(&body, MAX_REQUEST_BYTES)?;
    Ok((method, body))
}

fn visible_text(turn: &MessageTurn) -> String {
    turn.blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn fingerprint(text: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(text.as_bytes()))
}

fn native_id(turn: &MessageTurn) -> Option<&str> {
    // Preserve exact IDs: the native resolver tries a segmented ID before its
    // base. Removing the suffix here could name a different existing item.
    turn.agent_message_id.as_deref()
}

fn base_message_id(id: &str) -> &str {
    id.rsplit_once(":segment:")
        .filter(|(_, suffix)| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
        .map_or(id, |(base, _)| base)
}

fn human_user(turn: &MessageTurn) -> bool {
    matches!(turn.role, TurnRole::User)
        && !turn.blocks.is_empty()
        && turn.blocks.iter().all(|block| {
            matches!(
                block,
                ContentBlock::Text { .. } | ContentBlock::Image { .. }
            )
        })
}

fn point_for_turn(turns: &[MessageTurn], idx: usize, assistant: bool) -> Result<Value, AcpError> {
    let turn = &turns[idx];
    let text = visible_text(turn);
    let native = native_id(turn);
    let same_role = |other: &MessageTurn| {
        if assistant {
            matches!(other.role, TurnRole::Assistant)
        } else {
            human_user(other)
        }
    };
    let same_text = |other: &MessageTurn| same_role(other) && visible_text(other) == text;
    if let Some(id) = native {
        if !valid_id(id)
            || turns
                .iter()
                .filter(|other| {
                    same_role(other)
                        && native_id(other).map(base_message_id) == Some(base_message_id(id))
                })
                .count()
                != 1
        {
            return Err(history_error());
        }
    } else if is_js_blank(&text)
        || !turn.blocks.iter().all(
            |block| matches!(block, ContentBlock::Text { text } if !text.starts_with("# Files ")),
        )
        || turns.iter().filter(|other| same_text(other)).count() != 1
    {
        return Err(history_error());
    }
    let id = native.unwrap_or(&turn.id);
    if !valid_id(id) {
        return Err(history_error());
    }
    Ok(json!({
        "messageId": id,
        "messageFingerprint": fingerprint(&text),
        "messageOccurrence": turns[..idx].iter().filter(|other| same_text(other)).count() + 1,
    }))
}

/// Resolve only a persisted human user turn from a fresh parse of the bound
/// session. Parser authors must preserve complete visible text and native IDs;
/// this function cannot recover omitted text or infer an optimistic turn's ID.
/// Codex permits unique user-text fallback without a resume point. Only emit a
/// Codex resume point when its exact native assistant ID is known; positional
/// Codeg IDs cannot satisfy that optional guard. Claude requires the boundary
/// for every non-initial user message.
pub fn resolve_rewind(
    turns: &[MessageTurn],
    turn_id: &str,
    agent: AgentType,
) -> Result<Value, AcpError> {
    if !matches!(agent, AgentType::Codex | AgentType::ClaudeCode) {
        return Err(unsupported());
    }
    let mut matches = turns
        .iter()
        .enumerate()
        .filter(|(_, turn)| turn.id == turn_id);
    let (idx, turn) = matches.next().ok_or_else(history_error)?;
    if matches.next().is_some() || !human_user(turn) {
        return Err(history_error());
    }
    let before = point_for_turn(turns, idx, false)?;
    let mut result = json!({"beforeMessage": before});
    if let Some(previous_user) = turns[..idx].iter().rposition(human_user) {
        // Do not skip an unsafe trailing assistant/system bubble to select an
        // older convenient text answer. It would guard a different boundary.
        let boundary = idx.checked_sub(1).ok_or_else(history_error)?;
        let assistant = &turns[boundary];
        if boundary <= previous_user || !matches!(assistant.role, TurnRole::Assistant) {
            return Err(history_error());
        }
        if agent == AgentType::Codex && native_id(assistant).is_none() {
            // The native resolver accepts this unique user fingerprint. The
            // point builder already rejected repeated text and attachments
            // lacking a native user ID; never invent an assistant ID here.
            return Ok(result);
        }
        if is_js_blank(&visible_text(assistant)) {
            return Err(history_error());
        }
        // Each parser keeps distinct assistant messages separate; absorbed
        // tool results and thinking do not contribute visible Text. Native ID
        // plus the exact hash guards those ordinary mixed blocks. A fallback
        // assistant still needs text-only uniqueness in point_for_turn.
        result["resumeAtMessage"] = point_for_turn(turns, boundary, true)?;
    } else if turns[..idx]
        .iter()
        .any(|t| matches!(t.role, TurnRole::User | TurnRole::Assistant))
    {
        // A partial/ambiguous prefix is not proof this is the first user turn.
        return Err(history_error());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_rewind_is_host_only_and_requires_idle_for_preview() {
        let operation: NativeOperation = serde_json::from_value(json!("workspace_rewind_files")).unwrap();
        assert_eq!(operation, NativeOperation::WorkspaceRewindFiles);
        assert!(operation.requires_idle(&json!({"dryRun":true})));
        assert!(!operation.is_mutation(&json!({"dryRun":true})));
        assert!(operation.is_mutation(&json!({"dryRun":false})));
        assert!(prepare_request(&json!({}), AgentType::Codex, "session", operation, json!({"dryRun":true})).is_err());
    }

    fn caps() -> Value {
        let meta = json!({
            "runtime": {"version":1,"readMethod":RUNTIME_READ,"controlMethod":RUNTIME_CONTROL,
                "reads":READS,"controls":CONTROLS,
                "context":{"details":["summary","full"],"defaultDetail":"summary","fullMayUseNetwork":true}},
            "sessionRewind":{"version":1,"method":REWIND},
            "sessionRewindFiles":{"version":1,"method":REWIND_FILES,"dryRun":true},
            "fileRevert":{"version":1,"method":FILE_REVERT,"dryRun":true,"previewTokenRequired":true},
            "queue":{"version":1,"method":QUEUE,"actions":QUEUE_ACTIONS,
                "changedNotification":QUEUE_CHANGED,"turnNotification":QUEUE_TURN},
            "sessionMcp":{"version":1,"stateMethod":MCP_STATE,"setMethod":MCP_SET},
            "archive":{"version":1,"archiveMethod":ARCHIVE,"unarchiveMethod":UNARCHIVE},
            "discovery":{"version":1,"searchMethod":SEARCH,"attachmentMethod":ATTACHMENTS,
                "attachmentActions":["list","add","remove"]},
            "goal":{"version":1,"controlMethod":GOAL,"actions":GOAL_ACTIONS}
        });
        capabilities(meta.as_object())
    }

    fn request(
        agent: AgentType,
        operation: NativeOperation,
        params: Value,
    ) -> Result<(&'static str, Value), AcpError> {
        prepare_request(&caps(), agent, "bound", operation, params)
    }

    fn turn(id: &str, role: TurnRole, text: &str, native: Option<&str>) -> MessageTurn {
        // Deserialize fixtures so new optional model fields do not force the
        // validator tests to invent parser identity attributes.
        serde_json::from_value(json!({
            "id":id,"role":role,"blocks":[{"type":"text","text":text}],
            "timestamp":"2026-10-09T00:00:00Z","agent_message_id":native
        }))
        .unwrap()
    }

    #[test]
    fn operation_wire_names_are_closed() {
        for (operation, wire) in [
            (NativeOperation::RuntimeRead, "runtime_read"),
            (NativeOperation::RuntimeControl, "runtime_control"),
            (NativeOperation::Rewind, "rewind"),
            (NativeOperation::RewindFiles, "rewind_files"),
            (NativeOperation::FileRevert, "file_revert"),
            (NativeOperation::Queue, "queue"),
            (NativeOperation::McpState, "mcp_state"),
            (NativeOperation::McpSet, "mcp_set"),
            (NativeOperation::Archive, "archive"),
            (NativeOperation::Unarchive, "unarchive"),
            (NativeOperation::Search, "search"),
            (NativeOperation::Attachments, "attachments"),
            (NativeOperation::Goal, "goal"),
        ] {
            assert_eq!(serde_json::to_value(operation).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<NativeOperation>(json!(wire)).unwrap(),
                operation
            );
        }
        assert!(serde_json::from_value::<NativeOperation>(json!("_arbitrary/rpc")).is_err());
    }

    #[test]
    fn absence_wrong_methods_and_versions_never_authorize() {
        assert_eq!(capabilities(None), json!({}));
        let params = json!({"resource":"context"});
        for cap in [
            json!({}),
            json!({"runtime":{"version":1,"readMethod":"_evil","reads":["context"]}}),
            json!({"runtime":{"version":2,"readMethod":RUNTIME_READ,"reads":["context"]}}),
            json!({"runtime":{"version":"1","readMethod":RUNTIME_READ,"reads":["context"]}}),
            json!({"runtime":{"version":1,"readMethod":RUNTIME_READ,"reads":[]}}),
        ] {
            assert!(prepare_request(
                &cap,
                AgentType::Codex,
                "s",
                NativeOperation::RuntimeRead,
                params.clone()
            )
            .is_err());
            assert!(prepare_request(
                &capabilities(cap.as_object()),
                AgentType::Codex,
                "s",
                NativeOperation::RuntimeRead,
                params.clone()
            )
            .is_err());
        }
        assert!(request(AgentType::OpenCode, NativeOperation::RuntimeRead, params).is_err());
    }

    #[test]
    fn sanitize_removes_nested_credentials_and_arbitrary_vocabulary() {
        let mut raw = caps();
        raw["token"] = json!("secret");
        raw["runtime"]["credentials"] = json!({"env":{"key":"secret"}});
        raw["runtime"]["reads"] = json!(["context","secret","context",{"token":"secret"}]);
        raw["runtime"]["context"]["token"] = json!("secret");
        raw["runtime"]["context"]["details"] = json!(["summary", "full", "secret"]);
        raw["fileRevert"]["coverage"] = json!("secret");
        raw["queue"]["changedNotification"] = json!("secret");
        raw["sessionMcp"]["scope"] = json!("secret");
        raw["goal"]["actions"] = json!(["clear", "secret"]);
        raw["jetbrains"] = json!({"air":{"version":1,"token":"secret","capabilities":["sessionRewind","secret"],
            "goal":{"version":1,"controlMethod":GOAL,"actions":["set"],"token":"secret"}}});
        let clean = capabilities(raw.as_object());
        assert!(!clean.to_string().contains("secret"));
        assert_eq!(clean["runtime"]["reads"], json!(["context"]));
        assert_eq!(
            clean["jetbrains"]["air"]["capabilities"],
            json!(["sessionRewind"])
        );
        assert_eq!(capabilities(clean.as_object()), clean);
    }

    #[test]
    fn every_operation_checks_its_fixed_method() {
        let point = json!({"messageId":"u","messageFingerprint":fingerprint("hello"),"messageOccurrence":1});
        for (operation, agent, key, method_field, params) in [
            (
                NativeOperation::RuntimeRead,
                AgentType::Codex,
                "runtime",
                "readMethod",
                json!({"resource":"usage"}),
            ),
            (
                NativeOperation::RuntimeControl,
                AgentType::Codex,
                "runtime",
                "controlMethod",
                json!({"action":"reloadSkills"}),
            ),
            (
                NativeOperation::Rewind,
                AgentType::Codex,
                "sessionRewind",
                "method",
                json!({"beforeMessage":point}),
            ),
            (
                NativeOperation::RewindFiles,
                AgentType::ClaudeCode,
                "sessionRewindFiles",
                "method",
                json!({"beforeMessage":point,"dryRun":true}),
            ),
            (
                NativeOperation::FileRevert,
                AgentType::Codex,
                "fileRevert",
                "method",
                json!({"toolCallId":"f","dryRun":true}),
            ),
            (
                NativeOperation::Queue,
                AgentType::Codex,
                "queue",
                "method",
                json!({"action":"list"}),
            ),
            (
                NativeOperation::McpState,
                AgentType::ClaudeCode,
                "sessionMcp",
                "stateMethod",
                json!({}),
            ),
            (
                NativeOperation::Archive,
                AgentType::Codex,
                "archive",
                "archiveMethod",
                json!({}),
            ),
            (
                NativeOperation::Unarchive,
                AgentType::Codex,
                "archive",
                "unarchiveMethod",
                json!({}),
            ),
            (
                NativeOperation::Search,
                AgentType::Codex,
                "discovery",
                "searchMethod",
                json!({"searchTerm":"hello"}),
            ),
            (
                NativeOperation::Attachments,
                AgentType::Codex,
                "discovery",
                "attachmentMethod",
                json!({"action":"list"}),
            ),
            (
                NativeOperation::Goal,
                AgentType::Codex,
                "goal",
                "controlMethod",
                json!({"action":"clear"}),
            ),
        ] {
            assert!(
                request(agent, operation, params.clone()).is_ok(),
                "{operation:?}"
            );
            for replacement in [json!(null), json!("_arbitrary/rpc"), json!(false)] {
                let mut cap = caps();
                cap[key][method_field] = replacement;
                assert!(
                    prepare_request(&cap, agent, "bound", operation, params.clone()).is_err(),
                    "{operation:?}"
                );
            }
            let mut cap = caps();
            cap[key]["version"] = json!(0);
            assert!(prepare_request(&cap, agent, "bound", operation, params.clone()).is_err());
            let mut extra = params.clone();
            extra["unexpected"] = json!(true);
            assert!(request(agent, operation, extra).is_err());
            for identity in ["bound", "another-session"] {
                let mut injected = params.clone();
                injected["sessionId"] = json!(identity);
                assert!(request(agent, operation, injected).is_err());
            }
        }
    }

    #[test]
    fn bound_identity_only_and_no_arbitrary_rpc() {
        let (_, body) = request(
            AgentType::Codex,
            NativeOperation::RuntimeRead,
            json!({"resource":"usage"}),
        )
        .unwrap();
        assert_eq!(body, json!({"resource":"usage","sessionId":"bound"}));
        for key in [
            "sessionId",
            "threadId",
            "connectionId",
            "method",
            "_meta",
            "params",
        ] {
            let mut params = json!({"resource":"usage"});
            params[key] = json!("private-injected-value");
            let error = request(AgentType::Codex, NativeOperation::RuntimeRead, params)
                .unwrap_err()
                .to_string();
            assert!(!error.contains("private-injected-value"));
        }
        for params in [Value::Null, json!([]), json!(true)] {
            assert!(request(AgentType::Codex, NativeOperation::Archive, params).is_err());
        }
        assert!(prepare_request(
            &caps(),
            AgentType::Codex,
            "",
            NativeOperation::Archive,
            json!({})
        )
        .is_err());
    }

    #[test]
    fn runtime_agent_schemas_are_distinct_and_full_is_explicit() {
        assert!(request(
            AgentType::Codex,
            NativeOperation::RuntimeRead,
            json!({"resource":"context","detail":"summary"})
        )
        .is_err());
        assert!(request(
            AgentType::Codex,
            NativeOperation::RuntimeControl,
            json!({"action":"reconnectMcp","serverName":"m"})
        )
        .is_err());
        assert!(request(
            AgentType::ClaudeCode,
            NativeOperation::RuntimeControl,
            json!({"action":"reconnectMcp"})
        )
        .is_err());
        assert!(request(
            AgentType::ClaudeCode,
            NativeOperation::RuntimeRead,
            json!({"resource":"plugins"})
        )
        .is_err());
        assert!(request(
            AgentType::Codex,
            NativeOperation::RuntimeRead,
            json!({"resource":"agents"})
        )
        .is_err());
        let (_, body) = request(
            AgentType::ClaudeCode,
            NativeOperation::RuntimeRead,
            json!({"resource":"context"}),
        )
        .unwrap();
        assert_eq!(body["detail"], "summary");
        assert!(request(
            AgentType::ClaudeCode,
            NativeOperation::RuntimeRead,
            json!({"resource":"context","detail":"full"})
        )
        .is_ok());
        assert!(request(
            AgentType::ClaudeCode,
            NativeOperation::RuntimeRead,
            json!({"resource":"usage","detail":"full"})
        )
        .is_err());
        for params in [
            json!({"action":"reloadPlugins","holdOnCacheImpact":false}),
            json!({"action":"reloadOutputStyles"}),
            json!({"action":"toggleMcp","serverName":"m","enabled":true}),
            json!({"action":"backgroundTask","toolUseId":"task"}),
            json!({"action":"cancelQueuedMessage","messageId":"q"}),
        ] {
            assert!(request(
                AgentType::ClaudeCode,
                NativeOperation::RuntimeControl,
                params.clone()
            )
            .is_ok());
            assert!(request(AgentType::Codex, NativeOperation::RuntimeControl, params).is_err());
        }
        for params in [
            json!({"action":"toggleMcp","serverName":"m","enabled":"yes"}),
            json!({"action":"reloadPlugins","holdOnCacheImpact":null}),
            json!({"action":"cancelQueuedMessage","messageId":"\n"}),
            json!({"action":"backgroundTask","toolUseId":"t","messageId":"q"}),
        ] {
            assert!(request(
                AgentType::ClaudeCode,
                NativeOperation::RuntimeControl,
                params
            )
            .is_err());
        }
    }

    #[test]
    fn active_idle_and_mutation_classification() {
        for operation in [
            NativeOperation::RuntimeRead,
            NativeOperation::Search,
            NativeOperation::McpState,
        ] {
            assert!(!operation.is_mutation(&json!({})));
            assert!(!operation.requires_idle(&json!({})));
        }
        for action in QUEUE_ACTIONS {
            assert!(!NativeOperation::Queue.requires_idle(&json!({"action":action})));
            assert_eq!(
                NativeOperation::Queue.is_mutation(&json!({"action":action})),
                *action != "list"
            );
        }
        assert!(NativeOperation::Queue.is_mutation(&json!({"action":"unknown"})));
        assert!(NativeOperation::Queue.requires_idle(&json!({"action":"unknown"})));
        for action in CONTROLS {
            let params = json!({"action":action});
            assert!(NativeOperation::RuntimeControl.is_mutation(&params));
            assert_eq!(
                NativeOperation::RuntimeControl.requires_idle(&params),
                !["cancelQueuedMessage", "backgroundTask"].contains(action)
            );
        }
        for operation in [NativeOperation::RewindFiles, NativeOperation::FileRevert] {
            assert!(!operation.is_mutation(&json!({"dryRun":true})));
            assert!(operation.is_mutation(&json!({})));
            assert!(operation.requires_idle(&json!({"dryRun":true})));
        }
        for operation in [
            NativeOperation::Rewind,
            NativeOperation::Archive,
            NativeOperation::Unarchive,
            NativeOperation::McpSet,
        ] {
            assert!(operation.is_mutation(&json!({})));
            assert!(operation.requires_idle(&json!({})));
        }
    }

    #[test]
    fn queue_writes_require_exact_notifications_and_advertised_actions() {
        for (action, params) in [
            ("list", json!({"action":"list","cursor":null,"limit":0})),
            (
                "add",
                json!({"action":"add","input":[],"clientUserMessageId":"client"}),
            ),
            (
                "update",
                json!({"action":"update","input":[],"queuedSubmissionId":"q"}),
            ),
            (
                "delete",
                json!({"action":"delete","queuedSubmissionId":"q"}),
            ),
            (
                "reorder",
                json!({"action":"reorder","queuedSubmissionIds":["q"]}),
            ),
            ("start", json!({"action":"start","queuedSubmissionId":null})),
        ] {
            assert!(request(AgentType::Codex, NativeOperation::Queue, params.clone()).is_ok());
            for field in ["changedNotification", "turnNotification"] {
                for value in [Value::Null, json!("_other")] {
                    let mut cap = caps();
                    cap["queue"][field] = value;
                    assert_eq!(
                        prepare_request(
                            &cap,
                            AgentType::Codex,
                            "s",
                            NativeOperation::Queue,
                            params.clone()
                        )
                        .is_ok(),
                        action == "list"
                    );
                }
            }
            let mut cap = caps();
            cap["queue"]["actions"] = json!([]);
            assert!(
                prepare_request(&cap, AgentType::Codex, "s", NativeOperation::Queue, params)
                    .is_err()
            );
        }
        for params in [
            json!({"action":"unknown"}),
            json!({"action":"list","limit":4294967296_u64}),
            json!({"action":"list","limit":-1}),
            json!({"action":"list","limit":1.5}),
            json!({"action":"add","input":[],"clientUserMessageId":""}),
            json!({"action":"delete","queuedSubmissionId":"q","all":true}),
            json!({"action":"reorder","queuedSubmissionIds":[null]}),
        ] {
            assert!(request(AgentType::Codex, NativeOperation::Queue, params).is_err());
        }
        assert!(request(
            AgentType::ClaudeCode,
            NativeOperation::Queue,
            json!({"action":"list"})
        )
        .is_err());
    }

    #[test]
    fn queue_native_input_round_trips_without_translation() {
        let input = json!([
            {"type":"text","text":"  中文  ","text_elements":[{"byteRange":{"start":0,"end":4},"placeholder":null}]},
            {"type":"text","text":""}, {"type":"image","url":"data:...","detail":"original"},
            {"type":"image","fileId":"f","detail":null}, {"type":"localImage","path":"C:/x"},
            {"type":"audio","url":"https://example.test/a"}, {"type":"localAudio","path":"C:/a"},
            {"type":"skill","name":"s","path":"s"}, {"type":"mention","name":"m","path":"m"}
        ]);
        let (_, result) = request(
            AgentType::Codex,
            NativeOperation::Queue,
            json!({"action":"add","input":input,"clientUserMessageId":"u"}),
        )
        .unwrap();
        assert_eq!(result["input"], input);
        for input in [
            json!({"type":"image","data":"abc","mimeType":"image/png"}),
            json!({"type":"image","url":"u","fileId":"f"}),
            json!({"type":"text","text":"t","textElements":[]}),
            json!({"type":"text","text":"t","text_elements":[{"byteRange":{"start":0,"end":1,"sessionId":"x"}}]}),
            json!({"type":"text","text":"t","text_elements":[{"byteRange":{"start":0,"end":9007199254740992_u64}}]}),
            json!({"type":"text","text":"t","text_elements":null}),
            json!({"type":"image","url":"u","detail":"maximum"}),
            json!({"type":"unknown"}),
            json!({"type":"text","text":false}),
        ] {
            assert!(request(
                AgentType::Codex,
                NativeOperation::Queue,
                json!({"action":"add","input":[input],"clientUserMessageId":"u"})
            )
            .is_err());
        }
    }

    #[test]
    fn bounded_serialized_bytes_include_escaping_and_injected_identity() {
        let base = json!({"action":"set","objective":""});
        let overhead = serde_json::to_vec(&base).unwrap().len();
        let near_limit =
            json!({"action":"set","objective":"a".repeat(MAX_REQUEST_BYTES - overhead)});
        assert!(bounded_json(&near_limit, MAX_REQUEST_BYTES).is_ok());
        assert!(request(AgentType::Codex, NativeOperation::Goal, near_limit).is_err());
        let escaped = json!({"action":"set","objective":"\n".repeat(MAX_REQUEST_BYTES / 2)});
        assert!(request(AgentType::Codex, NativeOperation::Goal, escaped).is_err());
        let unicode = json!({"action":"set","objective":"中".repeat(MAX_REQUEST_BYTES / 3 + 1)});
        assert!(request(AgentType::Codex, NativeOperation::Goal, unicode).is_err());
    }

    #[test]
    fn discovery_uses_native_scope_and_payload_bound() {
        let (_, body) = request(
            AgentType::Codex,
            NativeOperation::Search,
            json!({"searchTerm":"hello","archived":false,"limit":100,"cursor":null}),
        )
        .unwrap();
        assert!(body.get("sessionId").is_none());
        for params in [
            json!({"searchTerm":""}),
            json!({"searchTerm":"x","limit":0}),
            json!({"searchTerm":"x","cursor":""}),
            json!({"searchTerm":"x","limit":null}),
        ] {
            assert!(request(AgentType::Codex, NativeOperation::Search, params).is_err());
        }
        for payload in [
            json!(null),
            json!({"url":"https://example.test"}),
            json!([1, 2]),
        ] {
            let (_, body) = request(AgentType::Codex, NativeOperation::Attachments,
                json!({"action":"add","attachmentType":"pull_request","identityKey":"k","payload":payload})).unwrap();
            assert_eq!(body["payload"], payload);
            assert_eq!(body["sessionId"], "bound");
        }
        assert!(request(AgentType::Codex, NativeOperation::Attachments,
            json!({"action":"add","attachmentType":"t","identityKey":"k","payload":"a".repeat(256*1024)})).is_err());
    }

    #[test]
    fn goal_air_precedence_and_advertised_action_are_required() {
        let mut cap = caps();
        cap["jetbrains"] = json!({"air":{"version":1,"goal":{"version":1,"controlMethod":GOAL,"actions":["clear"]}}});
        assert!(prepare_request(
            &cap,
            AgentType::Codex,
            "s",
            NativeOperation::Goal,
            json!({"action":"pause"})
        )
        .is_err());
        assert!(prepare_request(
            &cap,
            AgentType::ClaudeCode,
            "s",
            NativeOperation::Goal,
            json!({"action":"clear"})
        )
        .is_ok());
        cap["jetbrains"]["air"]["goal"]["version"] = json!(2);
        assert!(prepare_request(
            &capabilities(cap.as_object()),
            AgentType::Codex,
            "s",
            NativeOperation::Goal,
            json!({"action":"clear"})
        )
        .is_err());
        assert!(request(
            AgentType::ClaudeCode,
            NativeOperation::Goal,
            json!({"action":"pause"})
        )
        .is_err());
        assert!(request(
            AgentType::Codex,
            NativeOperation::Goal,
            json!({"action":"set","objective":"\u{feff}"})
        )
        .is_err());
        assert!(request(
            AgentType::Codex,
            NativeOperation::Goal,
            json!({"action":"clear","objective":"x"})
        )
        .is_err());
    }

    #[test]
    fn rewind_air_token_is_versioned_and_agent_specific() {
        let cap = capabilities(
            json!({"jetbrains":{"air":{"version":1,"capabilities":["sessionRewind"]}}}).as_object(),
        );
        let guards = resolve_rewind(
            &[turn("u", TurnRole::User, "prompt", None)],
            "u",
            AgentType::Codex,
        )
        .unwrap();
        assert!(prepare_request(
            &cap,
            AgentType::Codex,
            "s",
            NativeOperation::Rewind,
            guards.clone()
        )
        .is_ok());
        assert!(prepare_request(
            &cap,
            AgentType::ClaudeCode,
            "s",
            NativeOperation::Rewind,
            guards.clone()
        )
        .is_err());
        let mut wrong = cap;
        wrong["jetbrains"]["air"]["version"] = json!(2);
        assert!(prepare_request(
            &wrong,
            AgentType::Codex,
            "s",
            NativeOperation::Rewind,
            guards
        )
        .is_err());
    }

    #[test]
    fn file_restore_requires_explicit_preview_and_native_schema() {
        assert!(request(
            AgentType::Codex,
            NativeOperation::FileRevert,
            json!({"toolCallId":"t"})
        )
        .is_err());
        assert!(request(
            AgentType::Codex,
            NativeOperation::FileRevert,
            json!({"toolCallId":"t","dryRun":false})
        )
        .is_err());
        assert!(request(
            AgentType::Codex,
            NativeOperation::FileRevert,
            json!({"toolCallId":"t","dryRun":false,"previewToken":fingerprint("preview")})
        )
        .is_ok());
        assert!(request(
            AgentType::ClaudeCode,
            NativeOperation::FileRevert,
            json!({"toolCallId":"t","dryRun":true})
        )
        .is_err());
        assert!(request(
            AgentType::ClaudeCode,
            NativeOperation::McpSet,
            json!({"expectedRevision":0,"mcpServers":[]})
        )
        .is_err());
    }

    #[test]
    fn mcp_reload_accepts_only_host_instruction_and_native_revision_range() {
        let params = json!({"mode":"reloadConfigured","expectedRevision":0});
        let (method, body) = request(
            AgentType::ClaudeCode,
            NativeOperation::McpSet,
            params.clone(),
        )
        .unwrap();
        assert_eq!(method, MCP_SET);
        assert_eq!(
            body,
            json!({"mode":"reloadConfigured","expectedRevision":0,"sessionId":"bound"})
        );
        assert!(request(AgentType::Codex, NativeOperation::McpSet, params.clone()).is_err());
        for field in ["mcpServers", "command", "env", "headers", "sessionId"] {
            let mut extra = params.clone();
            extra[field] = json!([]);
            assert!(request(AgentType::ClaudeCode, NativeOperation::McpSet, extra).is_err());
        }
        for revision in [
            json!(-1),
            json!(0.5),
            Value::Null,
            json!(MAX_SAFE_INTEGER),
            json!("0"),
        ] {
            assert!(request(
                AgentType::ClaudeCode,
                NativeOperation::McpSet,
                json!({"mode":"reloadConfigured","expectedRevision":revision})
            )
            .is_err());
        }
        for params in [
            json!({"mode":"replace","expectedRevision":0}),
            json!({"expectedRevision":0}),
            json!({"mode":"reloadConfigured"}),
        ] {
            assert!(request(AgentType::ClaudeCode, NativeOperation::McpSet, params).is_err());
        }
        for value in [json!(2), Value::Null] {
            let mut cap = caps();
            cap["sessionMcp"]["version"] = value;
            assert!(prepare_request(
                &cap,
                AgentType::ClaudeCode,
                "s",
                NativeOperation::McpSet,
                params.clone()
            )
            .is_err());
        }
        let mut cap = caps();
        cap["sessionMcp"]["setMethod"] = json!("_arbitrary/rpc");
        assert!(prepare_request(
            &cap,
            AgentType::ClaudeCode,
            "s",
            NativeOperation::McpSet,
            params
        )
        .is_err());
    }

    #[test]
    fn internal_history_points_require_strict_positive_fingerprints() {
        let valid = json!({"messageId":"u","messageFingerprint":fingerprint("hello"),"messageOccurrence":1});
        for (key, value) in [
            ("messageOccurrence", json!(0)),
            ("messageOccurrence", json!(-1)),
            ("messageOccurrence", json!(1.5)),
            ("messageFingerprint", json!("")),
            ("messageFingerprint", json!("sha256:0")),
            ("messageId", json!("")),
            ("sessionId", json!("other")),
        ] {
            let mut point = valid.clone();
            point[key] = value;
            assert!(request(
                AgentType::Codex,
                NativeOperation::Rewind,
                json!({"beforeMessage":point})
            )
            .is_err());
        }
        assert!(request(
            AgentType::Codex,
            NativeOperation::Rewind,
            json!({"beforeMessage":valid,"turnId":"u"})
        )
        .is_err());
        assert!(request(
            AgentType::Codex,
            NativeOperation::Rewind,
            json!({"beforeMessage":valid,"resumeAtMessage":null})
        )
        .is_err());
    }

    #[test]
    fn first_user_guard_concatenates_full_text_without_trimming() {
        let mut user = turn("u", TurnRole::User, "  你好\n", None);
        user.blocks.push(ContentBlock::Text {
            text: "world  ".into(),
        });
        let result = resolve_rewind(&[user], "u", AgentType::Codex).unwrap();
        assert!(result.get("resumeAtMessage").is_none());
        assert_eq!(
            result["beforeMessage"]["messageFingerprint"],
            fingerprint("  你好\nworld  ")
        );
        assert_eq!(result["beforeMessage"]["messageOccurrence"], 1);
        assert_ne!(
            result["beforeMessage"]["messageFingerprint"],
            fingerprint("你好\nworld")
        );
        assert_eq!(
            fingerprint(""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn missing_nonhuman_empty_and_repeated_fallback_are_refused() {
        for text in ["", " ", "\n", "\u{feff}"] {
            assert!(resolve_rewind(
                &[turn("u", TurnRole::User, text, None)],
                "u",
                AgentType::Codex
            )
            .is_err());
        }
        let turns = vec![
            turn("u", TurnRole::User, "same", None),
            turn("a", TurnRole::Assistant, "answer", Some("a-id")),
            turn("u2", TurnRole::User, "same", None),
        ];
        assert!(resolve_rewind(&turns, "u", AgentType::Codex).is_err());
        assert!(resolve_rewind(&turns, "u2", AgentType::Codex).is_err());
        assert!(resolve_rewind(&turns, "missing", AgentType::Codex).is_err());
        assert!(resolve_rewind(&turns, "a", AgentType::Codex).is_err());
        let mut tool = turn("t", TurnRole::User, "", Some("native"));
        tool.blocks = vec![ContentBlock::Thinking {
            text: "internal".into(),
        }];
        assert!(resolve_rewind(&[tool], "t", AgentType::ClaudeCode).is_err());
        let duplicate = vec![
            turn("u", TurnRole::User, "one", Some("one")),
            turn("u", TurnRole::User, "two", Some("two")),
        ];
        assert!(resolve_rewind(&duplicate, "u", AgentType::ClaudeCode).is_err());
    }

    #[test]
    fn native_user_identity_permits_repetition_and_attachments() {
        let turns = vec![
            turn("u", TurnRole::User, "same", Some("native-u")),
            turn("a", TurnRole::Assistant, "answer", Some("native-a")),
            turn("u2", TurnRole::User, "same", Some("native-u2:segment:1")),
        ];
        let result = resolve_rewind(&turns, "u2", AgentType::Codex).unwrap();
        assert_eq!(result["beforeMessage"]["messageId"], "native-u2:segment:1");
        assert_eq!(result["beforeMessage"]["messageOccurrence"], 2);
        assert_eq!(result["resumeAtMessage"]["messageId"], "native-a");
        let mut image = turn("u", TurnRole::User, "", None);
        image.blocks = vec![ContentBlock::Image {
            data: "data".into(),
            mime_type: "image/png".into(),
            uri: None,
        }];
        assert!(resolve_rewind(&[image.clone()], "u", AgentType::ClaudeCode).is_err());
        image.agent_message_id = Some("native-image".into());
        assert!(resolve_rewind(&[image], "u", AgentType::ClaudeCode).is_ok());
        let mut alias = turns;
        alias[2].agent_message_id = Some("native-u:segment:1".into());
        assert!(resolve_rewind(&alias, "u2", AgentType::Codex).is_err());
    }

    #[test]
    fn historical_rewind_requires_safe_immediate_assistant_boundary() {
        let base = vec![
            turn("u", TurnRole::User, "first", Some("u-id")),
            turn("a", TurnRole::Assistant, "answer", Some("a-id")),
            turn("u2", TurnRole::User, "second", Some("u2-id")),
        ];
        for agent in [AgentType::Codex, AgentType::ClaudeCode] {
            assert!(resolve_rewind(&base, "u2", agent).is_ok());
            let mut no_boundary = base.clone();
            no_boundary.remove(1);
            assert!(resolve_rewind(&no_boundary, "u2", agent).is_err());
            let mut unsafe_boundary = base.clone();
            unsafe_boundary[1].blocks.push(ContentBlock::Thinking {
                text: "hidden".into(),
            });
            assert!(resolve_rewind(&unsafe_boundary, "u2", agent).is_ok());
            unsafe_boundary[1].blocks = vec![ContentBlock::Text { text: "".into() }];
            assert!(resolve_rewind(&unsafe_boundary, "u2", agent).is_err());
            let mut intervening = base.clone();
            intervening.insert(2, turn("sys", TurnRole::System, "marker", None));
            assert!(resolve_rewind(&intervening, "u2", agent).is_err());
        }
        let mut no_native = base;
        no_native[1].agent_message_id = None;
        let codex = resolve_rewind(&no_native, "u2", AgentType::Codex).unwrap();
        assert!(codex.get("resumeAtMessage").is_none());
        assert!(resolve_rewind(&no_native, "u2", AgentType::ClaudeCode).is_ok());
        let partial = vec![
            turn("a", TurnRole::Assistant, "older", Some("a")),
            turn("u", TurnRole::User, "first?", Some("u")),
        ];
        assert!(resolve_rewind(&partial, "u", AgentType::Codex).is_err());
    }

    #[test]
    fn actual_codex_parser_first_and_historical_unique_text_need_no_fake_resume_id() {
        use crate::parsers::{codex::CodexParser, AgentParser};
        let dir = tempfile::tempdir().unwrap();
        let id = "00000000-0000-4000-8000-000000000001";
        let records = [
            json!({"timestamp":"2026-10-09T00:00:00Z","type":"session_meta","payload":{"id":id,"cwd":"C:/test"}}),
            json!({"timestamp":"2026-10-09T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"first prompt","images":[]}}),
            json!({"timestamp":"2026-10-09T00:00:02Z","type":"event_msg","payload":{"type":"agent_message","message":"first answer"}}),
            json!({"timestamp":"2026-10-09T00:00:03Z","type":"event_msg","payload":{"type":"user_message","message":"second prompt","images":[]}}),
            json!({"timestamp":"2026-10-09T00:00:04Z","type":"event_msg","payload":{"type":"agent_message","message":"second answer"}}),
        ];
        let data = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(
            dir.path()
                .join(format!("rollout-2026-10-09T00-00-00-{id}.jsonl")),
            data,
        )
        .unwrap();
        let detail = CodexParser::with_base_dir(dir.path().to_path_buf())
            .get_conversation(id)
            .unwrap();
        let users: Vec<_> = detail
            .turns
            .iter()
            .filter(|turn| human_user(turn))
            .collect();
        assert_eq!(users.len(), 2);
        for (user, text) in users.iter().zip(["first prompt", "second prompt"]) {
            assert!(user.agent_message_id.is_none());
            let guards = resolve_rewind(&detail.turns, &user.id, AgentType::Codex).unwrap();
            assert!(guards.get("resumeAtMessage").is_none());
            assert_eq!(
                guards["beforeMessage"]["messageFingerprint"],
                fingerprint(text)
            );
            let (_, request) = request(AgentType::Codex, NativeOperation::Rewind, guards).unwrap();
            assert_eq!(request["sessionId"], "bound");
        }
    }

    /// Bridge for the isolated real ACP/native E2E runner. The runner creates a
    /// real native session with a loopback fake model, then points this test at
    /// that run's private sessions directory. Only the official Codeg parser
    /// supplies the turns; no saved/idealized MessageTurn fixture is accepted.
    /// Run explicitly with --ignored --exact and capture NATIVE_REWIND_REQUEST.
    #[test]
    #[ignore = "requires an isolated live ACP test session; never defaults to the user's home"]
    fn native_rewind_parser_bridge() {
        use crate::parsers::{claude::ClaudeParser, codex::CodexParser, AgentParser};
        let base =
            std::env::var_os("CODEG_NATIVE_TEST_BASE").expect("isolated parser base required");
        let id = std::env::var("CODEG_NATIVE_TEST_SESSION").expect("test session required");
        let index: usize = std::env::var("CODEG_NATIVE_TEST_USER_INDEX")
            .unwrap()
            .parse()
            .unwrap();
        let agent = match std::env::var("CODEG_NATIVE_TEST_AGENT").as_deref() {
            Ok("codex") => AgentType::Codex,
            Ok("claude") => AgentType::ClaudeCode,
            _ => panic!("explicit supported test agent required"),
        };
        let parser: Box<dyn AgentParser> = match agent {
            AgentType::Codex => Box::new(CodexParser::with_base_dir(base.into())),
            _ => Box::new(ClaudeParser::with_base_dir(base.into())),
        };
        let detail = parser.get_conversation(&id).unwrap();
        let user = detail
            .turns
            .iter()
            .filter(|turn| human_user(turn))
            .nth(index)
            .unwrap();
        let guards = resolve_rewind(&detail.turns, &user.id, agent).unwrap();
        let cap: Value =
            serde_json::from_str(&std::env::var("CODEG_NATIVE_TEST_META").unwrap()).unwrap();
        let (method, params) = prepare_request(
            &capabilities(cap.as_object()),
            agent,
            &id,
            NativeOperation::Rewind,
            guards,
        )
        .unwrap();
        println!(
            "NATIVE_REWIND_REQUEST={}",
            json!({"method":method,"params":params})
        );
    }
}
