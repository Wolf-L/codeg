//! Host prompt checkpoints. Native history is evidence, never a caller supplied path.
//! Incomplete/ambiguous coverage is deliberately unavailable rather than best effort.
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use super::error::AcpError;
use super::session_state::SessionState;
use super::types::ConnectionStatus;
use super::workspace_checkpoint::{self as checkpoint, Checkpoint, CheckpointBoundary, Snapshot};
use crate::models::agent::AgentType;
use crate::models::message::{ContentBlock, MessageTurn, TurnRole};

const MAX_HISTORY: usize = 4096;
const MAX_MANIFEST: u64 = 32 * 1024 * 1024;
const SESSION_QUOTA: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
static STORE_WRITES: Mutex<()> = Mutex::new(());

/// Logical on-disk bytes, including abandoned captures and restore backups.
/// No cleanup: old checkpoint evidence is never evicted to admit a new turn.
fn check_store_budget(store: &Path, reserve: u64, quota: u64) -> Result<(), AcpError> {
    if reserve > quota {
        return Err(refused("session storage quota exhausted"));
    }
    let mut used = 0u64;
    let mut entries = 0usize;
    let mut pending = vec![(store.to_owned(), 0usize)];
    while let Some((path, depth)) = pending.pop() {
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && path == store => continue,
            Err(_) => return Err(refused("cannot measure session checkpoint storage")),
        };
        if meta.file_type().is_symlink() {
            return Err(refused("checkpoint storage contains a link"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Err(refused("checkpoint storage contains a reparse point"));
            }
        }
        entries += 1;
        if entries > 100_000 || depth > 8 {
            return Err(refused("checkpoint storage entry bound exhausted"));
        }
        if meta.is_file() {
            used = used
                .checked_add(meta.len())
                .ok_or_else(|| refused("session storage size overflow"))?;
            if used > quota - reserve {
                return Err(refused(
                    "session storage quota exhausted; existing checkpoints retained",
                ));
            }
        } else if meta.is_dir() {
            for entry in fs::read_dir(path)
                .map_err(|_| refused("cannot measure session checkpoint storage"))?
            {
                pending.push((
                    entry
                        .map_err(|_| refused("cannot measure checkpoint entry"))?
                        .path(),
                    depth + 1,
                ));
            }
        } else {
            return Err(refused("unsupported checkpoint storage entry"));
        }
    }
    Ok(())
}

fn with_store_budget<T>(
    store: &Path,
    reserve: u64,
    write: impl FnOnce() -> Result<T, AcpError>,
) -> Result<T, AcpError> {
    let _guard = STORE_WRITES
        .lock()
        .map_err(|_| refused("checkpoint storage lock failed"))?;
    check_store_budget(store, reserve, SESSION_QUOTA)?;
    write()
}

static WORKSPACE_ADMISSION: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
struct Writer {
    id: String,
    root: PathBuf,
    valid: Arc<AtomicBool>,
}
static WRITERS: Mutex<Vec<Writer>> = Mutex::new(Vec::new());
static PEERS: Mutex<Vec<std::sync::Weak<RwLock<SessionState>>>> = Mutex::new(Vec::new());

pub fn register_peer(state: &Arc<RwLock<SessionState>>) {
    if let Ok(mut peers) = PEERS.lock() {
        peers.retain(|peer| peer.strong_count() != 0);
        if !peers.iter().any(|peer| peer.ptr_eq(&Arc::downgrade(state))) {
            peers.push(Arc::downgrade(state));
        }
    }
}

async fn peers_quiet(state: &Arc<RwLock<SessionState>>) -> bool {
    let peers = match PEERS.lock() {
        Ok(peers) => peers
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect::<Vec<_>>(),
        Err(_) => return false,
    };
    let root = state
        .read()
        .await
        .working_dir
        .as_ref()
        .and_then(|p| p.canonicalize().ok());
    let Some(root) = root else {
        return false;
    };
    for peer in peers {
        if Arc::ptr_eq(&peer, state) {
            continue;
        }
        let peer = peer.read().await;
        if peer
            .working_dir
            .as_ref()
            .and_then(|p| p.canonicalize().ok())
            .is_some_and(|p| overlaps(&root, &p))
            && (!quiescent(&peer)
                || peer.turn_in_flight
                || peer.native_mutation_in_flight
                || peer.status == ConnectionStatus::Prompting)
        {
            return false;
        }
    }
    true
}

pub fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

pub struct HostWriter {
    id: String,
    valid: Arc<AtomicBool>,
    release_on_drop: bool,
}
impl HostWriter {
    /// A detached/uncertain request remains a writer even if its drain is aborted.
    pub fn retain_until_confirmed(&mut self) {
        self.release_on_drop = false;
    }
    pub fn confirm_finished(&mut self) {
        self.release_on_drop = true;
    }
}
impl Drop for HostWriter {
    fn drop(&mut self) {
        if !self.release_on_drop {
            return;
        }
        if let Ok(mut writers) = WRITERS.lock() {
            writers.retain(|w| w.id != self.id);
        }
    }
}

pub async fn host_writer(state: &Arc<RwLock<SessionState>>) -> HostWriter {
    register_peer(state);
    let s = state.read().await;
    let id = uuid::Uuid::new_v4().to_string();
    let valid = Arc::new(AtomicBool::new(true));
    let root = s.working_dir.as_ref().and_then(|p| p.canonicalize().ok());
    if let Some(root) = root {
        if let Ok(mut writers) = WRITERS.lock() {
            for writer in writers.iter().filter(|w| overlaps(&w.root, &root)) {
                writer.valid.store(false, Ordering::SeqCst);
                valid.store(false, Ordering::SeqCst);
            }
            writers.push(Writer {
                id: id.clone(),
                root,
                valid: valid.clone(),
            });
        } else {
            valid.store(false, Ordering::SeqCst);
        }
    } else {
        valid.store(false, Ordering::SeqCst);
    }
    HostWriter {
        id,
        valid,
        release_on_drop: true,
    }
}

pub async fn ensure_no_writers(state: &Arc<RwLock<SessionState>>) -> Result<(), AcpError> {
    let root = state
        .read()
        .await
        .working_dir
        .as_ref()
        .and_then(|p| p.canonicalize().ok());
    let Some(root) = root else {
        return Err(refused("workspace is inaccessible"));
    };
    if WRITERS
        .lock()
        .map_err(|_| refused("writer registry unavailable"))?
        .iter()
        .any(|w| overlaps(&root, &w.root))
    {
        return Err(refused(
            "overlapping or cancelled native writer has not confirmed completion",
        ));
    }
    Ok(())
}

pub async fn invalidate_overlapping(state: &Arc<RwLock<SessionState>>) {
    register_peer(state);
    let root = state
        .read()
        .await
        .working_dir
        .as_ref()
        .and_then(|p| p.canonicalize().ok());
    if let (Some(root), Ok(writers)) = (root, WRITERS.lock()) {
        for writer in writers.iter().filter(|w| overlaps(&root, &w.root)) {
            writer.valid.store(false, Ordering::SeqCst);
        }
    }
}

/// Serialize host admission across connections sharing a canonical workspace.
/// Prompt callers hold this only through enqueue; active turns are checked by
/// the manager before restoring. A durable gate bridges the two HTTP calls.
pub async fn lock_workspace(
    _state: &Arc<RwLock<SessionState>>,
) -> Result<Option<tokio::sync::OwnedMutexGuard<()>>, AcpError> {
    // Short admission and restore transactions only, never a whole agent turn.
    // One lock also covers nested roots and unsupported agents without lock ordering.
    let lock = WORKSPACE_ADMISSION
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    Ok(Some(lock.lock_owned().await))
}

fn transaction_path(root: &Path) -> Result<PathBuf, AcpError> {
    let root = root
        .canonicalize()
        .map_err(|_| refused("workspace is inaccessible"))?;
    let bytes = serde_json::to_vec(&root).map_err(|_| refused("invalid workspace identity"))?;
    Ok(crate::paths::codeg_home_dir()
        .join("workspace-checkpoints")
        .join("transactions")
        .join(format!("{}.json", hash(&bytes))))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreTransaction {
    version: u32,
    root: PathBuf,
    agent: String,
    session: String,
    prefix: Vec<String>,
    before_prefix: Vec<String>,
    turn_id: String,
    expected: Value,
    token: String,
}

fn load_transaction(root: &Path) -> Result<Option<RestoreTransaction>, AcpError> {
    let path = transaction_path(root)?;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(refused("workspace recovery gate cannot be inspected")),
        Ok(_) => read_manifest(&path).map(Some),
    }
}

fn files_confirmed(store: &Path, token: &str) -> Result<bool, AcpError> {
    if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(refused("invalid recovery token"));
    }
    let complete = store.join("transactions").join(token).join("complete.json");
    if complete
        .try_exists()
        .map_err(|_| refused("cannot inspect file completion journal"))?
    {
        return read_manifest::<bool>(&complete);
    }
    Ok(false)
}

fn release_gate(root: &Path) -> Result<(), AcpError> {
    fs::remove_file(transaction_path(root)?)
        .map_err(|_| refused("cannot release workspace recovery gate"))
}

fn reconcile_gate(root: &Path, agent: AgentType, session: &str) -> Result<bool, AcpError> {
    let Some(tx) = load_transaction(root)? else {
        return Ok(true);
    };
    if tx.version != 1 || tx.agent != agent.to_string() || tx.session != session {
        return Ok(false);
    }
    let store = store_for(root, agent, session)?;
    if files_confirmed(&store, &tx.token)? {
        checkpoint::validate_restored(root, &store, &tx.token).map_err(|_| {
            refused(
                "restored files changed or recovery evidence is invalid; history remains fenced",
            )
        })?;
        if parse_history(agent, session)?.prefix == tx.before_prefix {
            release_gate(root)?;
            return Ok(true);
        }
        return Ok(false);
    }
    let transaction = store.join("transactions").join(&tx.token);
    let failed = transaction.join("failed.json");
    let rolled_back = if failed
        .try_exists()
        .map_err(|_| refused("cannot inspect failed restore"))?
    {
        let (_, errors): (String, Vec<String>) = read_manifest(&failed)?;
        errors.is_empty()
    } else {
        // The utility creates this directory before its first workspace write.
        !transaction
            .try_exists()
            .map_err(|_| refused("cannot inspect restore journal"))?
    };
    if rolled_back {
        release_gate(root)?;
        return Ok(true);
    }
    Err(refused("interrupted file restore has an uncertain outcome; retained transaction backups require recovery"))
}

fn reject_overlapping_transactions(root: &Path, directory: &Path) -> Result<(), AcpError> {
    let canonical = root
        .canonicalize()
        .map_err(|_| refused("workspace is inaccessible"))?;
    if directory
        .try_exists()
        .map_err(|_| refused("cannot inspect workspace recovery gates"))?
    {
        for entry in
            fs::read_dir(directory).map_err(|_| refused("cannot read workspace recovery gates"))?
        {
            let entry = entry.map_err(|_| refused("cannot read workspace recovery gate"))?;
            let tx: RestoreTransaction = read_manifest(&entry.path())?;
            if overlaps(&canonical, &tx.root) && tx.root != canonical {
                return Err(refused(
                    "overlapping workspace has a pending file restore transaction",
                ));
            }
        }
    }
    Ok(())
}

pub async fn ensure_no_transaction(state: &Arc<RwLock<SessionState>>) -> Result<(), AcpError> {
    let (root, agent, session) = {
        let s = state.read().await;
        (s.working_dir.clone(), s.agent_type, s.external_id.clone())
    };
    let Some(root) = root else {
        return Ok(());
    };
    tokio::task::spawn_blocking(move || {
        let directory = crate::paths::codeg_home_dir().join("workspace-checkpoints/transactions");
        reject_overlapping_transactions(&root, &directory)?;
        if !reconcile_gate(&root, agent, session.as_deref().unwrap_or_default())? {
            return Err(refused("file restore transaction pending; finish its matching history rewind before new work"));
        }
        Ok(())
    }).await.map_err(|_| refused("recovery gate worker failed"))?
}

/// Only an exact retry of the paired history operation can cross the gate.
/// A lost acknowledgement is reconciled by the full retained prefix, not guessed.
pub async fn guard_history_rewind(
    state: &Arc<RwLock<SessionState>>,
    params: &Value,
) -> Result<Option<Value>, AcpError> {
    let (agent, session, root) = {
        let s = state.read().await;
        if !idle(&s) {
            return Err(refused("history rewind requires a quiescent session"));
        }
        (
            s.agent_type,
            s.external_id
                .clone()
                .ok_or_else(|| refused("session not ready"))?,
            s.working_dir.clone(),
        )
    };
    let Some(root) = root else {
        return Ok(None);
    };
    let params = params.clone();
    tokio::task::spawn_blocking(move || {
        reject_overlapping_transactions(
            &root,
            &crate::paths::codeg_home_dir().join("workspace-checkpoints/transactions"),
        )?;
        let Some(tx) = load_transaction(&root)? else {
            return Ok(None);
        };
        if tx.version != 1
            || tx.agent != agent.to_string()
            || tx.session != session
            || params.get("turnId").and_then(Value::as_str) != Some(tx.turn_id.as_str())
            || params.get("expectedTurn") != Some(&tx.expected)
            || params.as_object().is_none_or(|p| p.len() != 2)
        {
            return Err(refused(
                "only the matching history rewind may finish this file restore",
            ));
        }
        let store = store_for(&root, agent, &session)?;
        // complete.json is written by the utility after verifying every path.
        // This survives a crash before the host's ready marker or HTTP response.
        if !files_confirmed(&store, &tx.token)? {
            return Err(refused("file restoration was not confirmed complete"));
        }
        checkpoint::validate_restored(&root, &store, &tx.token)
            .map_err(|_| refused("restored files changed; matching history rewind refused"))?;
        let parsed = parse_history(agent, &session)?;
        if parsed.prefix == tx.before_prefix {
            release_gate(&root)?;
            return Ok(Some(json!({"rewound":true})));
        }
        if parsed.prefix != tx.prefix {
            return Err(refused("full history changed after file restore"));
        }
        Ok(None)
    })
    .await
    .map_err(|_| refused("history recovery worker failed"))?
}

pub async fn acknowledge_history_rewind(state: &Arc<RwLock<SessionState>>) -> Result<(), AcpError> {
    let (agent, session, root) = {
        let s = state.read().await;
        (
            s.agent_type,
            s.external_id
                .clone()
                .ok_or_else(|| refused("session not ready"))?,
            s.working_dir.clone(),
        )
    };
    let Some(root) = root else {
        return Ok(());
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(2500);
    loop {
        let root = root.clone();
        let session = session.clone();
        let settled = tokio::task::spawn_blocking(move || -> Result<bool, AcpError> {
            let Some(tx) = load_transaction(&root)? else {
                return Ok(true);
            };
            if tx.agent != agent.to_string() || tx.session != session {
                return Err(refused("history acknowledgement session mismatch"));
            }
            let store = store_for(&root, agent, &session)?;
            checkpoint::validate_restored(&root, &store, &tx.token).map_err(|_| {
                refused(
                    "restored files changed before history acknowledgement; recovery gate retained",
                )
            })?;
            if parse_history(agent, &session).is_ok_and(|h| h.prefix == tx.before_prefix) {
                release_gate(&root)?;
                return Ok(true);
            }
            Ok(false)
        })
        .await
        .map_err(|_| refused("history acknowledgement worker failed"))??;
        if settled {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(refused(
                "history acknowledgement awaits a verified retained prefix; retry matching rewind",
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn refused(message: &str) -> AcpError {
    AcpError::protocol(format!("Workspace checkpoint unavailable: {message}"))
}

pub fn supported(agent: AgentType) -> bool {
    matches!(agent, AgentType::Codex | AgentType::ClaudeCode)
}

pub fn advertise(caps: &mut Value, agent: AgentType) {
    if supported(agent) {
        caps["workspaceRewindFiles"] = json!({
            "version": 1, "method": "codeg/workspace/rewind_files",
            "dryRun": true, "previewTokenRequired": true
        });
    }
}

/// Do not age out unknown/running background work as the idle reaper does.
pub fn quiescent(s: &SessionState) -> bool {
    !s.native_recovery_required
        && !s.native_queue_pending
        && s.native_queue_turn_id.is_none()
        && !s.native_superseded_host_turn
        && s.background_outstanding == 0
        && s.pending_permission.is_none()
        && s.pending_question.is_none()
        && s.pending_plan_approval.is_none()
        && s.active_delegations.is_empty()
        && !s.goal_active
        && !s.agent_initiated_turn
        && s.active_tool_calls.values().all(|tool| {
            matches!(
                tool.status,
                super::session_state::ToolCallStatus::Completed
                    | super::session_state::ToolCallStatus::Failed
            )
        })
        && s.async_tasks
            .values()
            .all(|task| crate::acp::types::async_task_state_is_terminal(&task.state))
}

pub fn idle(s: &SessionState) -> bool {
    quiescent(s)
        && !s.turn_in_flight
        && s.status == ConnectionStatus::Connected
        && !s.native_mutation_in_flight
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn store_for(root: &Path, agent: AgentType, session: &str) -> Result<PathBuf, AcpError> {
    let root = root
        .canonicalize()
        .map_err(|_| refused("workspace is inaccessible"))?;
    let identity = serde_json::to_vec(&(agent.to_string(), session, root))
        .map_err(|_| refused("invalid session identity"))?;
    Ok(crate::paths::codeg_home_dir()
        .join("workspace-checkpoints")
        .join(hash(&identity)))
}

#[derive(Debug)]
struct History {
    turns: Vec<MessageTurn>,
    prefix: Vec<String>,
    raw_users: Vec<Option<String>>,
}

fn expected_turn(turn: &MessageTurn) -> Value {
    json!({"timestamp": turn.timestamp, "agentMessageId": turn.agent_message_id, "blocks": turn.blocks})
}

fn parse_history(agent: AgentType, session: &str) -> Result<History, AcpError> {
    parse_history_allow_new(agent, session, false)
}

fn allow_missing_new(
    error: &crate::parsers::ParseError,
    session: &str,
    confirmed_new: bool,
) -> bool {
    confirmed_new
        && matches!(error, crate::parsers::ParseError::ConversationNotFound(id) if id == session)
}

fn parse_history_allow_new(
    agent: AgentType,
    session: &str,
    confirmed_new: bool,
) -> Result<History, AcpError> {
    let detail = match crate::parsers::build_agent_parser(agent).get_conversation(session) {
        Ok(detail) => detail,
        Err(error) if allow_missing_new(&error, session, confirmed_new) => {
            return Ok(History {
                turns: vec![],
                prefix: vec![],
                raw_users: vec![],
            });
        }
        Err(_) => return Err(refused("full persisted history cannot be read")),
    };
    if detail.turns.len() > MAX_HISTORY {
        return Err(refused("history exceeds the checkpoint bound"));
    }
    let mut prefix = Vec::with_capacity(detail.turns.len());
    let mut raw_users = Vec::with_capacity(detail.turns.len());
    let mut unique = HashSet::new();
    let mut bytes = 0usize;
    for turn in &detail.turns {
        let raw = if matches!(turn.role, TurnRole::User) {
            // No lossy attachment or normalized-text identity guesses.
            if turn.blocks.is_empty()
                || !turn
                    .blocks
                    .iter()
                    .all(|b| matches!(b, ContentBlock::Text { .. }))
            {
                return Err(refused("unsupported original user payload identity"));
            }
            let text = match agent {
                AgentType::Codex => crate::parsers::codex::native_user_message_text(session, turn),
                AgentType::ClaudeCode => crate::parsers::claude::native_user_message_text(
                    session,
                    turn.agent_message_id
                        .as_deref()
                        .ok_or_else(|| refused("missing native user identity"))?,
                ),
                _ => return Err(refused("unsupported agent")),
            }
            .map_err(|_| refused("original user identity is ambiguous or stale"))?;
            Some(hash(text.as_bytes()))
        } else {
            None
        };
        let payload = serde_json::to_vec(&json!({
            "role": turn.role, "original": expected_turn(turn), "nativePayload": raw,
        }))
        .map_err(|_| refused("history identity serialization failed"))?;
        bytes = bytes.saturating_add(payload.len());
        if bytes > 16 * 1024 * 1024 {
            return Err(refused("history identity exceeds the checkpoint bound"));
        }
        let identity = hash(&payload);
        if !unique.insert(identity.clone()) {
            return Err(refused("duplicate persisted identity"));
        }
        prefix.push(identity);
        raw_users.push(raw);
    }
    Ok(History {
        turns: detail.turns,
        prefix,
        raw_users,
    })
}

async fn history(agent: AgentType, session: String) -> Result<History, AcpError> {
    tokio::task::spawn_blocking(move || parse_history(agent, &session))
        .await
        .map_err(|_| refused("history worker failed"))?
}

pub struct Pending {
    before: Snapshot,
    prefix: Vec<String>,
    prompt_hash: String,
    agent: AgentType,
    session: String,
    store: PathBuf,
    revision: u64,
    writer_valid: Arc<AtomicBool>,
}

/// Awaited BEFORE sending session/prompt. Failures leave this turn without coverage.
pub async fn begin(
    state: &Arc<RwLock<SessionState>>,
    prompt: &[agent_client_protocol::schema::v1::ContentBlock],
    confirmed_new: bool,
    writer: &HostWriter,
) -> Option<Pending> {
    if !supported(state.read().await.agent_type) {
        return None;
    }
    match begin_checked(state, prompt, confirmed_new, writer.valid.clone()).await {
        Ok(pending) => Some(pending),
        Err(error) => {
            // Every error here is a fixed host diagnostic, never a native payload.
            tracing::debug!("Workspace before checkpoint unavailable: {error}");
            None
        }
    }
}

async fn begin_checked(
    state: &Arc<RwLock<SessionState>>,
    prompt: &[agent_client_protocol::schema::v1::ContentBlock],
    confirmed_new: bool,
    writer_valid: Arc<AtomicBool>,
) -> Result<Pending, AcpError> {
    if !writer_valid.load(Ordering::SeqCst) {
        return Err(refused("overlapping host writer invalidated capture"));
    }
    if !peers_quiet(state).await {
        return Err(refused("overlapping connection has active work"));
    }
    let (agent, session, root, revision) = {
        let s = state.read().await;
        if !supported(s.agent_type) || !quiescent(&s) || s.native_mutation_in_flight {
            return Err(refused("session has unaccounted work"));
        }
        (
            s.agent_type,
            s.external_id
                .clone()
                .ok_or_else(|| refused("session not ready"))?,
            s.working_dir
                .clone()
                .ok_or_else(|| refused("workspace not bound"))?,
            s.native_queue_revision,
        )
    };
    let mut text = String::new();
    for block in prompt {
        match block {
            agent_client_protocol::schema::v1::ContentBlock::Text(block) => {
                text.push_str(&block.text)
            }
            _ => return Err(refused("prompt payload identity is unsupported")),
        }
    }
    if text.is_empty() {
        return Err(refused("empty prompt identity"));
    }
    let parse_session = session.clone();
    let before_history = tokio::task::spawn_blocking(move || {
        parse_history_allow_new(agent, &parse_session, confirmed_new)
    })
    .await
    .map_err(|_| refused("history worker failed"))??;
    let store = store_for(&root, agent, &session)?;
    let capture_store = store.clone();
    let before = tokio::task::spawn_blocking(move || {
        // 64 MiB blobs + 32 MiB manifest + temporary file headroom.
        with_store_budget(&capture_store, 128 * MIB, || {
            checkpoint::capture(&root, &capture_store, checkpoint::CaptureLimits::default())
                .map_err(|_| refused("before capture failed"))
        })
    })
    .await
    .map_err(|_| refused("capture worker failed"))??;
    if !before.complete {
        return Err(refused("before snapshot coverage is incomplete"));
    }
    let parse_session = session.clone();
    let after_history = tokio::task::spawn_blocking(move || {
        parse_history_allow_new(agent, &parse_session, confirmed_new)
    })
    .await
    .map_err(|_| refused("history worker failed"))??;
    if before_history.prefix != after_history.prefix {
        return Err(refused("history changed during capture"));
    }
    let s = state.read().await;
    if !quiescent(&s)
        || s.native_mutation_in_flight
        || s.native_queue_revision != revision
        || s.external_id.as_deref() != Some(session.as_str())
    {
        return Err(refused("session changed during capture"));
    }
    Ok(Pending {
        before,
        prefix: before_history.prefix,
        prompt_hash: hash(text.as_bytes()),
        agent,
        session,
        store,
        revision,
        writer_valid,
    })
}

fn appended_user(before: &[String], after: &History, prompt_hash: &str) -> Result<usize, AcpError> {
    if !after.prefix.starts_with(before) {
        return Err(refused("native history prefix changed"));
    }
    let users: Vec<_> = after
        .raw_users
        .iter()
        .enumerate()
        .skip(before.len())
        .filter(|(_, raw)| raw.is_some())
        .collect();
    if users.len() != 1 || users[0].1.as_deref() != Some(prompt_hash) {
        return Err(refused(
            "completion did not persist exactly the sent prompt",
        ));
    }
    let index = users[0].0;
    if index != before.len() {
        return Err(refused("unaccounted native work preceded the prompt"));
    }
    if !after.turns[index + 1..]
        .iter()
        .any(|turn| matches!(turn.role, TurnRole::Assistant))
    {
        return Err(refused("persisted assistant completion has not arrived"));
    }
    Ok(index)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    before_prefix: Vec<String>,
    after_prefix: Vec<String>,
    user_identity: String,
    checkpoint: Checkpoint,
}

fn write_new<T: Serialize>(path: &Path, value: &T) -> Result<(), AcpError> {
    let bytes = serde_json::to_vec(value).map_err(|_| refused("manifest serialization failed"))?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(refused("manifest exceeds the bound"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| refused("invalid manifest location"))?;
    fs::create_dir_all(parent).map_err(|_| refused("manifest directory cannot be created"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| refused("manifest already exists or cannot be written"))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| refused("manifest persistence failed"))
}

fn read_manifest<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, AcpError> {
    let file = fs::File::open(path).map_err(|_| {
        refused("missing checkpoint or preview; history-only rewind remains available")
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| refused("manifest cannot be read"))?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(refused("manifest exceeds the bound"));
    }
    serde_json::from_slice(&bytes).map_err(|_| refused("manifest is incomplete or invalid"))
}

fn capture_still_valid(s: &SessionState, pending: &Pending) -> bool {
    pending.writer_valid.load(Ordering::SeqCst)
        && quiescent(s)
        && !s.native_mutation_in_flight
        && s.external_id.as_deref() == Some(pending.session.as_str())
        && s.native_queue_revision == pending.revision
}

/// Never called on cancelled/error exits. A missing final record is missing coverage.
pub async fn finish(state: &Arc<RwLock<SessionState>>, pending: Option<Pending>) {
    let Some(pending) = pending else {
        return;
    };
    if let Err(error) = finish_checked(state, pending).await {
        tracing::debug!("Workspace checkpoint completion unavailable: {error}");
    }
}

async fn finish_checked(
    state: &Arc<RwLock<SessionState>>,
    pending: Pending,
) -> Result<(), AcpError> {
    if !peers_quiet(state).await {
        return Err(refused("overlapping connection has active work"));
    }
    let mut fresh = None;
    for attempt in 0..10 {
        if !capture_still_valid(&*state.read().await, &pending) {
            return Err(refused("background or queued work at completion"));
        }
        if let Ok(parsed) = history(pending.agent, pending.session.clone()).await {
            if let Ok(index) = appended_user(&pending.prefix, &parsed, &pending.prompt_hash) {
                fresh = Some((parsed, index));
                break;
            }
        }
        if attempt < 9 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    let (parsed, index) =
        fresh.ok_or_else(|| refused("fresh persisted prompt identity missing"))?;
    // The prompt response can precede the provider's last transcript flush.
    // Require a second full parse to settle before associating the after image.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    if history(pending.agent, pending.session.clone())
        .await?
        .prefix
        != parsed.prefix
    {
        return Err(refused("persisted completion is still changing"));
    }
    let before = pending.before.clone();
    let finish_store = pending.store.clone();
    let checkpoint = tokio::task::spawn_blocking(move || {
        // After blobs + snapshot/checkpoint manifests + temp file headroom.
        with_store_budget(&finish_store, 192 * MIB, || {
            checkpoint::finish(&before, &uuid::Uuid::new_v4().to_string())
                .map_err(|_| refused("after capture failed"))
        })
    })
    .await
    .map_err(|_| refused("completion worker failed"))??;
    if !checkpoint.after.complete {
        return Err(refused("after snapshot coverage is incomplete"));
    }
    let final_history = history(pending.agent, pending.session.clone()).await?;
    if final_history.prefix != parsed.prefix
        || !capture_still_valid(&*state.read().await, &pending)
        || !peers_quiet(state).await
    {
        return Err(refused(
            "history or background work changed during completion",
        ));
    }
    let record = Record {
        version: 1,
        before_prefix: pending.prefix,
        after_prefix: parsed.prefix.clone(),
        user_identity: parsed.prefix[index].clone(),
        checkpoint,
    };
    tokio::task::spawn_blocking(move || {
        with_store_budget(&pending.store, MAX_MANIFEST, || {
            write_new(
                &pending
                    .store
                    .join("history")
                    .join(format!("{}.json", record.user_identity)),
                &record,
            )
        })
    })
    .await
    .map_err(|_| refused("manifest worker failed"))?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    turn_id: String,
    expected_turn: Value,
    dry_run: bool,
    preview_token: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewBinding {
    version: u32,
    prefix: Vec<String>,
    selected: String,
    paths: Vec<String>,
}

fn selected_index(history: &History, request: &Request) -> Result<usize, AcpError> {
    let matches: Vec<_> = history
        .turns
        .iter()
        .enumerate()
        .filter(|(_, turn)| turn.id == request.turn_id)
        .collect();
    if matches.len() != 1
        || !matches!(matches[0].1.role, TurnRole::User)
        || expected_turn(matches[0].1) != request.expected_turn
    {
        return Err(refused("selected persisted user changed; reload history"));
    }
    Ok(matches[0].0)
}

fn boundaries(
    history: &History,
    selected: usize,
    store: &Path,
) -> Result<Vec<CheckpointBoundary>, AcpError> {
    let mut result: Vec<CheckpointBoundary> = Vec::new();
    let mut previous: Option<Vec<String>> = None;
    for (index, identity) in history.prefix.iter().enumerate().skip(selected) {
        if !matches!(history.turns[index].role, TurnRole::User) {
            continue;
        }
        if result.len() >= 256 {
            return Err(refused("restore span exceeds the bound"));
        }
        let record: Record =
            read_manifest(&store.join("history").join(format!("{identity}.json")))?;
        if record.version != 1
            || record.user_identity != *identity
            || record.before_prefix != history.prefix[..index]
            || record.after_prefix.len() <= index
            || !history.prefix.starts_with(&record.after_prefix)
            || previous
                .as_ref()
                .is_some_and(|p| p != &record.before_prefix)
        {
            return Err(refused("checkpoint branch or history coverage mismatch"));
        }
        if !record.checkpoint.before.complete
            || !record.checkpoint.after.complete
            || result
                .last()
                .is_some_and(|previous| !previous.after.same_state(&record.checkpoint.before))
        {
            return Err(refused("incomplete snapshot or inter-turn workspace gap"));
        }
        previous = Some(record.after_prefix);
        result.push(CheckpointBoundary {
            before: record.checkpoint.before,
            after: record.checkpoint.after,
        });
    }
    if previous.as_ref() != Some(&history.prefix) {
        return Err(refused("latest full history has no completed checkpoint"));
    }
    Ok(result)
}

/// Runs in the manager's shielded task with the per-connection prompt fence held.
pub async fn rewind(state: Arc<RwLock<SessionState>>, params: Value) -> Result<Value, AcpError> {
    ensure_no_transaction(&state).await?;
    let request: Request =
        serde_json::from_value(params).map_err(|_| refused("invalid host rewind parameters"))?;
    if request.turn_id.is_empty()
        || request.turn_id.len() > 1024
        || (!request.dry_run && request.preview_token.is_none())
    {
        return Err(refused("persisted turn and preview token are required"));
    }
    let (agent, session, root, revision) = {
        let mut s = state.write().await;
        if !supported(s.agent_type) || !idle(&s) {
            return Err(refused(
                "session must be idle with no pending approvals or background work",
            ));
        }
        let tuple = (
            s.agent_type,
            s.external_id
                .clone()
                .ok_or_else(|| refused("session not ready"))?,
            s.working_dir
                .clone()
                .ok_or_else(|| refused("workspace not bound"))?,
            s.native_queue_revision,
        );
        s.native_mutation_in_flight = true;
        tuple
    };
    let result = rewind_checked(&state, agent, session, root, revision, request).await;
    let mut s = state.write().await;
    s.native_mutation_in_flight = false;
    if result
        .as_ref()
        .ok()
        .and_then(|r| r.get("uncertain"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        s.native_recovery_required = true;
    }
    result
}

async fn rewind_checked(
    state: &Arc<RwLock<SessionState>>,
    agent: AgentType,
    session: String,
    root: PathBuf,
    revision: u64,
    request: Request,
) -> Result<Value, AcpError> {
    let store = store_for(&root, agent, &session)?;
    let parsed = history(agent, session.clone()).await?;
    let selected = selected_index(&parsed, &request)?;
    {
        let s = state.read().await;
        if !quiescent(&s)
            || s.turn_in_flight
            || s.status != ConnectionStatus::Connected
            || s.external_id.as_deref() != Some(session.as_str())
            || s.native_queue_revision != revision
        {
            return Err(refused("session changed during restore validation"));
        }
    }
    let applying = !request.dry_run;
    let work = tokio::task::spawn_blocking(move || {
        // Includes utility plans, durable backup copies, moved files and journals.
        let reserve = if request.dry_run { 64 * MIB } else { 256 * MIB };
        let budget_store = store.clone();
        with_store_budget(&budget_store, reserve, || {
        let span = boundaries(&parsed, selected, &store)?;
        // Parse again immediately before planning/apply, not from a UI cache.
        if parse_history(agent, &session)?.prefix != parsed.prefix { return Err(refused("history changed before restore")); }
        if request.dry_run {
            let preview = checkpoint::plan_restore_span(&span, &root, &store)
                .map_err(|_| refused("snapshot gap, incomplete coverage or current file conflict"))?;
            let binding = PreviewBinding { version: 1, prefix: parsed.prefix.clone(), selected: parsed.prefix[selected].clone(), paths: preview.paths.clone() };
            write_new(&store.join("host-previews").join(format!("{}.json", preview.preview_token)), &binding)?;
            Ok(json!({"canRevert":true,"previewToken":preview.preview_token,"paths":preview.paths}))
        } else {
            let token = request.preview_token.ok_or_else(|| refused("preview token required"))?;
            if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(refused("invalid preview token")); }
            let binding: PreviewBinding = read_manifest(&store.join("host-previews").join(format!("{token}.json")))?;
            if binding.version != 1 || binding.prefix != parsed.prefix || binding.selected != parsed.prefix[selected] {
                return Err(refused("preview belongs to different history or selected turn"));
            }
            let transaction = RestoreTransaction { version: 1, root: root.canonicalize().map_err(|_| refused("workspace is inaccessible"))?, agent: agent.to_string(), session,
                prefix: parsed.prefix.clone(), before_prefix: parsed.prefix[..selected].to_vec(),
                turn_id: request.turn_id, expected: request.expected_turn, token: token.clone() };
            // Created before ANY file mutation. A crash leaves an explicit fence,
            // never an invitation to run more prompts against half-restored files.
            write_new(&transaction_path(&root)?, &transaction)?;
            match checkpoint::apply(&root, &store, &token) {
                Ok(report) => {
                    if write_new(&store.join("host-transactions").join(format!("{token}.ready.json")), &token).is_err() {
                        return Ok(json!({"reverted":false,"paths":report.paths,"uncertain":true,"error":"Files restored but host acknowledgement failed; reconnect and retry only the matching history rewind"}));
                    }
                    Ok(json!({"reverted":true,"paths":report.paths}))
                }
                Err(checkpoint::CheckpointError::ApplyFailed { uncertain, .. }) => {
                    if !uncertain { release_gate(&root)?; }
                    Ok(json!({"reverted":false,"paths":binding.paths,"uncertain":uncertain,"error":"File restoration failed; history was not changed"}))
                }
                Err(_) => {
                    fs::remove_file(transaction_path(&root)?).map_err(|_| refused("cannot release failed preflight gate"))?;
                    Err(refused("preview expired, was used, or current files conflict"))
                }
            }
        }
        })
    }).await;
    match work {
        Ok(result) => result,
        Err(_) if applying => Ok(
            json!({"reverted":false,"paths":[],"uncertain":true,"error":"Restore worker failed; inspect workspace before continuing"}),
        ),
        Err(_) => Err(refused("preview worker failed")),
    }
}

#[cfg(test)]
mod admission_concurrency_tests {
    use super::*;
    use crate::acp::connection::ConnectionCommand;
    use crate::acp::manager::ConnectionManager;
    use crate::acp::native_session::NativeOperation;
    use crate::acp::types::PromptInputBlock;
    use crate::web::event_bridge::EventEmitter;
    use std::time::Duration;

    async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("admission must not wait for another workspace's RPC")
    }

    async fn connection(
        manager: &ConnectionManager,
        id: &str,
        root: &Path,
    ) -> tokio::sync::mpsc::Receiver<ConnectionCommand> {
        manager
            .insert_test_connection_live(
                id,
                AgentType::Codex,
                Some(root.to_owned()),
                EventEmitter::Noop,
            )
            .await
    }

    async fn send_other(
        manager: &ConnectionManager,
        receiver: &mut tokio::sync::mpsc::Receiver<ConnectionCommand>,
    ) {
        bounded(manager.send_prompt(
            "other",
            vec![PromptInputBlock::Text {
                text: "independent workspace".into(),
            }],
        ))
        .await
        .unwrap();
        assert!(matches!(
            bounded(receiver.recv()).await,
            Some(ConnectionCommand::Prompt { .. })
        ));
    }

    #[tokio::test]
    async fn slow_readonly_native_rpc_does_not_block_other_workspace_prompt() {
        let source = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let manager = Arc::new(ConnectionManager::new());
        let mut native = connection(&manager, "native", source.path()).await;
        let mut prompt = connection(&manager, "other", other.path()).await;
        let state = manager.get_state("native").await.unwrap();
        {
            let mut s = state.write().await;
            s.external_id = Some("synthetic-admission-read".into());
            s.native_capabilities = json!({"runtime": {
                "version": 1, "readMethod": "_session/runtime/read", "reads": ["usage"]
            }});
        }
        let caller = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .native_operation(
                        "native",
                        NativeOperation::RuntimeRead,
                        json!({"resource":"usage"}),
                    )
                    .await
            }
        });
        let Some(ConnectionCommand::NativeOperation { reply, .. }) = bounded(native.recv()).await
        else {
            panic!("expected native read");
        };
        assert!(!caller.is_finished());
        assert!(ensure_no_writers(&state).await.is_ok());
        send_other(&manager, &mut prompt).await;
        reply.send(Ok(json!({"usage":0}))).unwrap();
        assert!(bounded(caller).await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn cancelled_native_mutation_keeps_writer_without_blocking_other_workspace() {
        let source = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let manager = Arc::new(ConnectionManager::new());
        let mut native = connection(&manager, "native", source.path()).await;
        let mut prompt = connection(&manager, "other", other.path()).await;
        let state = manager.get_state("native").await.unwrap();
        {
            let mut s = state.write().await;
            s.external_id = Some("synthetic-admission-mutation".into());
            s.native_capabilities = json!({"runtime": {
                "version": 1, "controlMethod": "_session/runtime/control", "controls": ["reloadSkills"]
            }});
        }
        let caller = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .native_operation(
                        "native",
                        NativeOperation::RuntimeControl,
                        json!({"action":"reloadSkills"}),
                    )
                    .await
            }
        });
        let Some(ConnectionCommand::NativeOperation { reply, .. }) = bounded(native.recv()).await
        else {
            panic!("expected native mutation");
        };
        caller.abort();
        assert!(bounded(caller).await.unwrap_err().is_cancelled());
        assert!(ensure_no_writers(&state).await.is_err());
        send_other(&manager, &mut prompt).await;
        reply.send(Ok(json!({"ok":true}))).unwrap();
        bounded(async {
            while ensure_no_writers(&state).await.is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn prompt_channel_backpressure_does_not_hold_workspace_admission() {
        let source = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let manager = ConnectionManager::new();
        let mut full = connection(&manager, "full", source.path()).await;
        let mut prompt = connection(&manager, "other", other.path()).await;
        for _ in 0..4 {
            manager.set_mode("full", "test".into()).await.unwrap();
        }
        let blocked = manager.send_prompt(
            "full",
            vec![PromptInputBlock::Text {
                text: "queued".into(),
            }],
        );
        tokio::pin!(blocked);
        // Poll to the full-channel reserve without relying on a scheduling sleep.
        assert!(futures::poll!(&mut blocked).is_pending());
        send_other(&manager, &mut prompt).await;
        assert!(matches!(
            bounded(full.recv()).await,
            Some(ConnectionCommand::SetMode { .. })
        ));
        bounded(blocked).await.unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_only_rewind_cannot_bypass_parent_or_child_restore_gate() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("workspace");
        let child = parent.join("nested");
        let other = temp.path().join("other");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir(&other).unwrap();
        let gates = temp.path().join("gates");
        let mut tx = RestoreTransaction {
            version: 1,
            root: parent.canonicalize().unwrap(),
            agent: "codex".into(),
            session: "fixture".into(),
            prefix: vec![],
            before_prefix: vec![],
            turn_id: "fixture".into(),
            expected: json!({}),
            token: "fixture".into(),
        };
        write_new(&gates.join("parent.json"), &tx).unwrap();
        assert!(reject_overlapping_transactions(&child, &gates).is_err());
        assert!(reject_overlapping_transactions(&other, &gates).is_ok());
        // An exact workspace remains eligible for its paired history recovery.
        assert!(reject_overlapping_transactions(&parent, &gates).is_ok());
        tx.root = child.canonicalize().unwrap();
        write_new(&gates.join("child.json"), &tx).unwrap();
        assert!(reject_overlapping_transactions(&parent, &gates).is_err());
    }

    #[tokio::test]
    async fn cancelled_drain_retains_writer_until_response_and_invalidates_next_capture() {
        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(RwLock::new(SessionState::new(
            "drain".into(),
            AgentType::Codex,
            Some(temp.path().to_owned()),
            "test".into(),
            None,
        )));
        let mut writer = host_writer(&state).await;
        writer.retain_until_confirmed();
        let (send, receive) = tokio::sync::oneshot::channel::<()>();
        let drain = tokio::spawn(async move {
            if receive.await.is_ok() {
                writer.confirm_finished();
            }
        });
        assert!(ensure_no_writers(&state).await.is_err());
        let next = host_writer(&state).await;
        assert!(!next.valid.load(Ordering::SeqCst));
        drop(next);
        send.send(()).unwrap();
        drain.await.unwrap();
        assert!(ensure_no_writers(&state).await.is_ok());
    }

    #[tokio::test]
    async fn aborted_or_failed_drain_keeps_unknown_writer_fenced() {
        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(RwLock::new(SessionState::new(
            "failed-drain".into(),
            AgentType::Codex,
            Some(temp.path().to_owned()),
            "test".into(),
            None,
        )));
        let mut writer = host_writer(&state).await;
        let id = writer.id.clone();
        writer.retain_until_confirmed();
        drop(writer); // Mirrors an aborted task or unsuccessful response.
        assert!(ensure_no_writers(&state).await.is_err());
        let next = host_writer(&state).await;
        assert!(!next.valid.load(Ordering::SeqCst));
        drop(next);
        // Test-only cleanup; production never guesses that an unknown writer ended.
        WRITERS.lock().unwrap().retain(|writer| writer.id != id);
    }

    #[test]
    fn quota_refuses_new_capture_without_removing_existing_evidence() {
        let store = tempfile::tempdir().unwrap();
        fs::write(store.path().join("saved"), [7u8; 64]).unwrap();
        assert!(check_store_budget(store.path(), 64, 128).is_ok());
        assert!(check_store_budget(store.path(), 65, 128).is_err());
        assert_eq!(fs::read(store.path().join("saved")).unwrap(), vec![7u8; 64]);
        fs::create_dir(store.path().join("transactions")).unwrap();
        fs::write(store.path().join("transactions/backup"), [8u8; 32]).unwrap();
        assert!(check_store_budget(store.path(), 33, 128).is_err());
        assert!(check_store_budget(store.path(), 32, 128).is_ok());
    }

    #[tokio::test]
    async fn overlapping_non_native_host_turns_invalidate_both_captures() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("nested");
        fs::create_dir(&child).unwrap();
        let outer = Arc::new(RwLock::new(SessionState::new(
            "outer".into(),
            AgentType::Codex,
            Some(temp.path().to_owned()),
            "test".into(),
            None,
        )));
        let inner = Arc::new(RwLock::new(SessionState::new(
            "inner".into(),
            AgentType::Gemini,
            Some(child),
            "test".into(),
            None,
        )));
        let first = host_writer(&outer).await;
        assert!(first.valid.load(Ordering::SeqCst));
        let second = host_writer(&inner).await;
        assert!(!first.valid.load(Ordering::SeqCst));
        assert!(!second.valid.load(Ordering::SeqCst));
        drop(second);
        assert!(!first.valid.load(Ordering::SeqCst));
        drop(first);
        let fresh = host_writer(&outer).await;
        assert!(fresh.valid.load(Ordering::SeqCst));
        invalidate_overlapping(&inner).await;
        assert!(!fresh.valid.load(Ordering::SeqCst));
    }

    #[test]
    fn empty_first_prompt_requires_confirmed_new_and_exact_missing_session() {
        use crate::parsers::ParseError;
        let missing = ParseError::ConversationNotFound("new-session".into());
        assert!(allow_missing_new(&missing, "new-session", true));
        assert!(!allow_missing_new(&missing, "new-session", false));
        assert!(!allow_missing_new(&missing, "different-session", true));
        assert!(!allow_missing_new(
            &ParseError::InvalidData("damaged".into()),
            "new-session",
            true
        ));
        assert!(!allow_missing_new(
            &ParseError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)),
            "new-session",
            true
        ));
    }

    #[test]
    fn reconnect_uses_utility_completion_journal_without_host_ready_marker() {
        let store = tempfile::tempdir().unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        assert!(!files_confirmed(store.path(), token).unwrap());
        let transaction = store.path().join("transactions").join(token);
        fs::create_dir_all(&transaction).unwrap();
        // A journal alone is not a commit acknowledgement.
        fs::write(transaction.join("journal.json"), b"{}").unwrap();
        assert!(!files_confirmed(store.path(), token).unwrap());
        fs::write(transaction.join("complete.json"), b"true").unwrap();
        assert!(files_confirmed(store.path(), token).unwrap());
        assert!(!store.path().join("host-transactions").exists());
        fs::write(transaction.join("complete.json"), b"tru").unwrap();
        assert!(files_confirmed(store.path(), token).is_err());
        assert!(files_confirmed(store.path(), "../other").is_err());
    }

    fn turn(id: &str, role: TurnRole, text: &str) -> MessageTurn {
        MessageTurn {
            id: id.into(),
            role,
            blocks: vec![ContentBlock::Text { text: text.into() }],
            timestamp: chrono::DateTime::from_timestamp(1, 0).unwrap(),
            usage: None,
            duration_ms: None,
            model: None,
            completed_at: None,
            agent_message_id: Some(id.into()),
        }
    }

    fn fixture() -> History {
        History {
            turns: vec![
                turn("user507", TurnRole::User, "old"),
                turn("a", TurnRole::Assistant, "old result"),
                turn("user509", TurnRole::User, "sent"),
                turn("b", TurnRole::Assistant, "result"),
            ],
            prefix: vec!["old-user", "old-assistant", "sent-user", "sent-assistant"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            raw_users: vec![Some(hash(b"old")), None, Some(hash(b"sent")), None],
        }
    }

    #[test]
    fn completion_requires_exact_one_sent_user_and_unchanged_entire_prefix() {
        let mut h = fixture();
        let prefix = h.prefix[..2].to_vec();
        assert_eq!(appended_user(&prefix, &h, &hash(b"sent")).unwrap(), 2);
        assert!(appended_user(&prefix, &h, &hash(b"different")).is_err());
        h.prefix[1] = "edited prior assistant".into();
        assert!(appended_user(&prefix, &h, &hash(b"sent")).is_err());
        h.prefix[1] = prefix[1].clone();
        h.turns.push(turn("steer", TurnRole::User, "steered"));
        h.prefix.push("steered-identity".into());
        h.raw_users.push(Some(hash(b"steered")));
        assert!(appended_user(&prefix, &h, &hash(b"sent")).is_err());
    }

    #[test]
    fn user_only_parse_is_not_a_persisted_completion() {
        let mut h = fixture();
        h.turns.pop();
        h.prefix.pop();
        h.raw_users.pop();
        assert!(appended_user(&h.prefix[..2], &h, &hash(b"sent")).is_err());
    }

    #[test]
    fn old_user_has_explicit_missing_checkpoint_not_fabricated_before_image() {
        let store = tempfile::tempdir().unwrap();
        let error = boundaries(&fixture(), 0, store.path())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("missing checkpoint"));
    }

    #[test]
    fn expected_turn_rejects_positional_id_reused_for_edited_payload() {
        let h = fixture();
        let mut request = Request {
            turn_id: "user509".into(),
            expected_turn: expected_turn(&h.turns[2]),
            dry_run: true,
            preview_token: None,
        };
        assert_eq!(selected_index(&h, &request).unwrap(), 2);
        request.expected_turn["blocks"][0]["text"] = json!("edited");
        assert!(selected_index(&h, &request).is_err());
    }

    #[test]
    fn request_cannot_select_workspace_paths_or_native_guards() {
        for key in [
            "root",
            "store",
            "path",
            "paths",
            "sessionId",
            "beforeMessage",
        ] {
            let mut value = json!({"turnId":"user507","expectedTurn":{},"dryRun":true});
            value[key] = json!("untrusted");
            assert!(serde_json::from_value::<Request>(value).is_err());
        }
    }

    #[test]
    fn host_capability_is_only_advertised_for_codex_and_claude() {
        for agent in [AgentType::Codex, AgentType::ClaudeCode] {
            let mut caps = json!({});
            advertise(&mut caps, agent);
            assert_eq!(
                caps["workspaceRewindFiles"],
                json!({"version":1,"method":"codeg/workspace/rewind_files","dryRun":true,"previewTokenRequired":true})
            );
        }
        let mut caps = json!({});
        advertise(&mut caps, AgentType::Gemini);
        assert_eq!(caps, json!({}));
    }

    #[test]
    fn idle_guard_refuses_queue_approval_background_and_goal() {
        let mut state =
            SessionState::new("test".into(), AgentType::Codex, None, "test".into(), None);
        state.status = ConnectionStatus::Connected;
        assert!(idle(&state));
        state.background_outstanding = 1;
        assert!(!idle(&state));
        state.background_outstanding = 0;
        state.native_queue_pending = true;
        assert!(!idle(&state));
        state.native_queue_pending = false;
        state.native_queue_turn_id = Some("queue".into());
        assert!(!idle(&state));
        state.native_queue_turn_id = None;
        state.native_mutation_in_flight = true;
        assert!(!idle(&state));
        state.native_mutation_in_flight = false;
        state.goal_active = true;
        assert!(!idle(&state));
        state.goal_active = false;
        state.turn_in_flight = true;
        assert!(!idle(&state));
        state.turn_in_flight = false;
        state.status = ConnectionStatus::Disconnected;
        assert!(!idle(&state));
    }

    #[test]
    fn discarded_range_requires_all_records_same_branch_and_no_manual_gap() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        let store = tmp.path().join("store");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file.txt"), b"before").unwrap();
        let a = checkpoint::capture(&root, &store, checkpoint::CaptureLimits::default()).unwrap();
        fs::write(root.join("file.txt"), b"first").unwrap();
        let b = checkpoint::finish(&a, "first").unwrap();
        let h = fixture();
        let first = Record {
            version: 1,
            before_prefix: vec![],
            after_prefix: h.prefix[..2].to_vec(),
            user_identity: h.prefix[0].clone(),
            checkpoint: b.clone(),
        };
        write_new(&store.join("history/old-user.json"), &first).unwrap();
        assert!(boundaries(&h, 0, &store).is_err()); // later user uncovered
        fs::write(root.join("file.txt"), b"manual edit").unwrap();
        let c = checkpoint::capture(&root, &store, checkpoint::CaptureLimits::default()).unwrap();
        fs::write(root.join("file.txt"), b"second").unwrap();
        let d = checkpoint::finish(&c, "second").unwrap();
        let mut second = Record {
            version: 1,
            before_prefix: h.prefix[..2].to_vec(),
            after_prefix: h.prefix.clone(),
            user_identity: h.prefix[2].clone(),
            checkpoint: d,
        };
        let second_path = store.join("history/sent-user.json");
        write_new(&second_path, &second).unwrap();
        assert!(boundaries(&h, 0, &store)
            .err()
            .unwrap()
            .to_string()
            .contains("inter-turn"));
        second.checkpoint.before = b.after;
        fs::write(&second_path, serde_json::to_vec(&second).unwrap()).unwrap();
        assert_eq!(boundaries(&h, 0, &store).unwrap().len(), 2);
        second.after_prefix[1] = "stale branch".into();
        fs::write(&second_path, serde_json::to_vec(&second).unwrap()).unwrap();
        assert!(boundaries(&h, 0, &store).is_err());
    }
}
