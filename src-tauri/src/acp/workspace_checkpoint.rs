//! Local, bounded byte checkpoints, independent of Git and of the agent's tools.
//!
//! Call these synchronous functions with `spawn_blocking`. The host supplies a trusted
//! workspace root and an account/session-scoped Codeg data directory, never paths
//! supplied by a renderer. Associate the host-generated checkpoint ID with the exact
//! original user turn/history prefix in the integration layer. Use `plan_restore_span`
//! for multiple turns: matching only the first/last snapshots loses inter-turn edits.
//!
//! Coverage: regular files, including hidden and Git-ignored source files. At every
//! depth, `.git`, `node_modules`, `target`, `.next`, and `buildcache` are excluded
//! (case insensitive). No ignore files/globs are consulted. Links, reparse points,
//! hard links, special files, unreadable/nonportable paths, and exceeded limits make
//! a snapshot incomplete; incomplete snapshots cannot restore anything. Directories
//! participate in fingerprints and type checks; empty directories are retained.
//! File bytes and ordinary Unix rwx modes are restored, not ownership, ACLs,
//! timestamps, directory permissions, Git index/HEAD, or excluded content. Unix
//! setuid/setgid/sticky files fail capture closed; restoring privileged modes is
//! unsupported. Mode changes participate in fingerprints and conflict checks.
//! Schema 1 lacked modes; it is intentionally ineligible for schema 2 restores.
//!
//! Resource bounds are PER OPERATION, not a cumulative store quota. File reads
//! are capped at 32 MiB, JSON at 32 MiB, each scan at 100,000 entries/512 MiB,
//! and restore spans at 256 boundaries; caller-owned history can still grow.
//! Disk retention is NOT implemented here: unique blobs, manifests, previews,
//! transaction backups, and blobs from failed captures accumulate indefinitely.
//! Full snapshots stay local. Before enabling ongoing capture the host must own
//! an account/session quota and retention policy, refuse capture when exhausted,
//! and release expired in-memory history. Cleanup must respect live manifests,
//! previews and journals; never prune unresolved/uncertain transaction backups.
//! Content addressing deduplicates equal bytes; it does not bound total storage.
//!
//! A process-wide mutex serializes this module's operations, NOT external writers or
//! other processes. Repeated validation detects ordinary edits, not adversarial
//! filesystem TOCTOU. The host must quiesce prompts/tools during apply. Captures use
//! two matching walks, not an OS-level atomic snapshot. Application is atomic per
//! file, not across files; a crash can leave partial work. A durable journal and
//! backups remain for diagnosis/recovery. Failed rollback explicitly reports an
//! uncertain workspace. Removing a newly created file means renaming it to backup;
//! if the store is on a different filesystem that move fails closed (no unlink
//! fallback). This module never permanently deletes workspace files.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

pub const SCHEMA_VERSION: u32 = 2;
const MAX_JSON_BYTES: u64 = 32 * 1024 * 1024;
const EXCLUDED: &[&str] = &[".git", "node_modules", "target", ".next", "buildcache"];
static WORKSPACE_LOCK: Mutex<()> = Mutex::new(());
type Result<T> = std::result::Result<T, CheckpointError>;

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("checkpoint I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("checkpoint JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("checkpoint refused: {0}")]
    Refused(String),
    #[error("workspace conflict: {0}")]
    Conflict(String),
    #[error("restore failed: {cause}; rollback errors: {rollback_errors:?}; uncertain={uncertain}; backups={backup_dir:?}")]
    ApplyFailed {
        cause: String,
        rollback_errors: Vec<String>,
        uncertain: bool,
        backup_dir: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CaptureLimits {
    pub max_files: usize,
    pub max_entries: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_depth: usize,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_files: 5_000,
            max_entries: 10_000,
            max_file_bytes: 8 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            max_depth: 64,
        }
    }
}

impl CaptureLimits {
    fn validate(self) -> Result<()> {
        if self.max_files > 50_000
            || self.max_entries > 100_000
            || self.max_file_bytes > 32 * 1024 * 1024
            || self.max_total_bytes > 512 * 1024 * 1024
            || self.max_depth > 64
        {
            return refuse("capture limits exceed the module's hard ceiling");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum Entry {
    Directory,
    File {
        hash: String,
        bytes: u64,
        /// None on non-Unix hosts. Kept out of content-addressed blob identity.
        #[serde(default, rename = "unixMode")]
        unix_mode: Option<u32>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Snapshot {
    pub schema_version: u32,
    pub id: String,
    pub fingerprint: String,
    pub complete: bool,
    pub issues: Vec<String>,
    root: PathBuf,
    store_dir: PathBuf,
    limits: CaptureLimits,
    entries: BTreeMap<String, Entry>,
}

impl Snapshot {
    /// Compare content/shape and coverage, ignoring snapshot IDs. Never equate
    /// incomplete snapshots. Persisted inputs are revalidated by planning/apply.
    pub fn same_state(&self, other: &Self) -> bool {
        self.complete
            && other.complete
            && self.issues.is_empty()
            && other.issues.is_empty()
            && self.schema_version == SCHEMA_VERSION
            && other.schema_version == SCHEMA_VERSION
            && self.root == other.root
            && self.store_dir == other.store_dir
            && self.limits == other.limits
            && self.fingerprint == other.fingerprint
            && self.entries == other.entries
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckpointBoundary {
    pub before: Snapshot,
    pub after: Snapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checkpoint {
    pub schema_version: u32,
    pub checkpoint_id: String,
    pub before: Snapshot,
    pub after: Snapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestorePreview {
    pub schema_version: u32,
    pub preview_token: String,
    /// Workspace-relative paths only. No content is included in the preview.
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreReport {
    pub schema_version: u32,
    pub paths: Vec<String>,
    pub backup_dir: PathBuf,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Plan {
    schema_version: u32,
    preview_token: String,
    before: Snapshot,
    after: Snapshot,
    paths: Vec<String>,
}

/// The store must be outside the workspace, with no symlink/reparse ancestors.
/// A coverage failure returns an explicitly incomplete, persisted Snapshot.
pub fn capture(root: &Path, store_dir: &Path, limits: CaptureLimits) -> Result<Snapshot> {
    let _guard = lock()?;
    capture_inner(root, store_dir, limits)
}

/// Convenience for a single host prompt. IDs are opaque host IDs, not paths;
/// their hash names the manifest. Existing IDs are never overwritten.
pub fn finish(before: &Snapshot, checkpoint_id: &str) -> Result<Checkpoint> {
    let _guard = lock()?;
    if checkpoint_id.is_empty() || checkpoint_id.len() > 1024 {
        return refuse("invalid host checkpoint ID");
    }
    let (root, store) = locations(&before.root, &before.store_dir, false)?;
    validate_snapshot(before, &root, &store, false)?;
    let checkpoint = Checkpoint {
        schema_version: SCHEMA_VERSION,
        checkpoint_id: checkpoint_id.to_owned(),
        before: before.clone(),
        after: capture_inner(&root, &store, before.limits)?,
    };
    save_json(
        &store
            .join("checkpoints")
            .join(format!("{}.json", hash(checkpoint_id.as_bytes()))),
        &checkpoint,
    )?;
    Ok(checkpoint)
}

/// Single recorded span only. For a chain of host prompts use plan_restore_span.
pub fn plan_restore(
    before: &Snapshot,
    after: &Snapshot,
    root: &Path,
    store_dir: &Path,
) -> Result<RestorePreview> {
    plan_restore_span(
        &[CheckpointBoundary {
            before: before.clone(),
            after: after.clone(),
        }],
        root,
        store_dir,
    )
}

/// Refuse the entire operation if any boundary is incomplete, has different
/// coverage, or has an inter-turn gap. Unrelated current files are not touched.
pub fn plan_restore_span(
    boundaries: &[CheckpointBoundary],
    root: &Path,
    store_dir: &Path,
) -> Result<RestorePreview> {
    let _guard = lock()?;
    let (root, store) = locations(root, store_dir, false)?;
    if boundaries.is_empty() || boundaries.len() > 256 {
        return refuse("restore requires between 1 and 256 ordered boundaries");
    }
    for (i, boundary) in boundaries.iter().enumerate() {
        validate_snapshot(&boundary.before, &root, &store, true)?;
        validate_snapshot(&boundary.after, &root, &store, true)?;
        if boundary.before.limits != boundary.after.limits {
            return refuse("capture coverage changed within a turn");
        }
        if i > 0 && !boundaries[i - 1].after.same_state(&boundary.before) {
            return refuse(format!("inter-turn gap before boundary {i}"));
        }
    }
    let before = boundaries[0].before.clone();
    let after = boundaries[boundaries.len() - 1].after.clone();
    let paths = changed_paths(&before, &after)?;
    preflight(&root, &store, &before, &after, &paths)?;
    let plan = Plan {
        schema_version: SCHEMA_VERSION,
        preview_token: uuid::Uuid::new_v4().simple().to_string(),
        before,
        after,
        paths: paths.clone(),
    };
    save_json(
        &store
            .join("previews")
            .join(format!("{}.json", plan.preview_token)),
        &plan,
    )?;
    Ok(RestorePreview {
        schema_version: SCHEMA_VERSION,
        preview_token: plan.preview_token,
        paths,
    })
}

/// The host must resolve root/store from its session, not accept them from UI.
/// Tokens are single-use after transaction creation, including failed applies.
pub fn apply(root: &Path, store_dir: &Path, preview_token: &str) -> Result<RestoreReport> {
    let _guard = lock()?;
    apply_inner(root, store_dir, preview_token, &mut |_| Ok(()))
}

/// Read-only recovery check before the host changes history or releases its gate.
/// Requires a completed transaction, its original durable plan/journal, valid
/// snapshots/blobs, and every affected path still matching BEFORE, including
/// absence, file type, bytes and Unix mode. Unrelated paths are not inspected.
/// A completion marker alone is never evidence of the current workspace state.
/// Keep the host's prompt/tool gate held across this check and the history update:
/// this module's mutex cannot prevent subsequent edits by external processes.
pub fn validate_restored(root: &Path, store_dir: &Path, preview_token: &str) -> Result<()> {
    let _guard = lock()?;
    let (root, store) = locations(root, store_dir, false)?;
    let plan = load_plan(&root, &store, preview_token)?;
    let transaction = store.join("transactions").join(preview_token);
    let journal: Plan = load_json(&transaction.join("journal.json"))?;
    if journal != plan {
        return refuse("restore journal differs from preview");
    }
    let complete: bool = load_json(&transaction.join("complete.json"))?;
    if !complete {
        return refuse("restore transaction is not complete");
    }
    match fs::symlink_metadata(transaction.join("failed.json")) {
        Ok(_) => return refuse("restore transaction has a failure record"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // Reverse the usual preflight: BEFORE is the expected *current* state.
    // This also revalidates both sets of referenced content-addressed blobs.
    preflight(&root, &store, &plan.after, &plan.before, &plan.paths)
}

fn load_plan(root: &Path, store: &Path, token: &str) -> Result<Plan> {
    if !valid_hex(token, 32) {
        return refuse("invalid preview token");
    }
    let plan: Plan = load_json(&store.join("previews").join(format!("{token}.json")))?;
    if plan.schema_version != SCHEMA_VERSION || plan.preview_token != token {
        return refuse("invalid preview");
    }
    validate_snapshot(&plan.before, root, store, true)?;
    validate_snapshot(&plan.after, root, store, true)?;
    if plan.before.limits != plan.after.limits
        || plan.paths != changed_paths(&plan.before, &plan.after)?
    {
        return refuse("preview changed");
    }
    Ok(plan)
}

fn lock() -> Result<std::sync::MutexGuard<'static, ()>> {
    WORKSPACE_LOCK
        .lock()
        .map_err(|_| CheckpointError::Refused("checkpoint lock poisoned".into()))
}

fn refuse<T>(message: impl Into<String>) -> Result<T> {
    Err(CheckpointError::Refused(message.into()))
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn excluded(name: &str) -> bool {
    EXCLUDED.iter().any(|x| name.eq_ignore_ascii_case(x))
}

fn valid_relative(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 4096 {
        return refuse("empty or overlong relative path");
    }
    for part in value.split('/') {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'));
        if part.is_empty()
            || part == "."
            || part == ".."
            || excluded(part)
            || part.ends_with(['.', ' '])
            || device
            || part
                .chars()
                .any(|c| c.is_control() || "\\:<>\"|?*".contains(c))
        {
            return refuse(format!("unsafe or excluded relative path: {value}"));
        }
    }
    Ok(())
}

fn reject_link(meta: &Metadata, path: &Path) -> Result<()> {
    #[cfg(windows)]
    let reparse = {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse = false;
    if meta.file_type().is_symlink() || reparse {
        return refuse(format!("link/reparse point: {}", path.display()));
    }
    Ok(())
}

// Check every existing component before canonicalizing, so canonicalization
// cannot silently authorize a symlink/junction target. Missing tails are OK.
fn check_chain(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return refuse("absolute host paths required");
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir | Component::CurDir) {
            return refuse("non-normal host path");
        }
        current.push(component.as_os_str());
        // Windows drive prefixes alone are not filesystem entries.
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(meta) => reject_link(&meta, &current)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    check_chain(path)?;
    let canonical = fs::canonicalize(path)?;
    if !fs::symlink_metadata(&canonical)?.is_dir() {
        return refuse("expected directory");
    }
    Ok(canonical)
}

fn locations(root: &Path, store: &Path, create: bool) -> Result<(PathBuf, PathBuf)> {
    let root = canonical_directory(root)?;
    for path in [root.as_path(), store] {
        if path.components().any(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(".git"))
        }) {
            return refuse("workspace/store cannot be inside .git");
        }
    }
    check_chain(store)?;
    // Resolve the nearest existing store ancestor BEFORE creating any folders.
    let mut ancestor = store;
    let mut tail = Vec::new();
    while !ancestor.try_exists()? {
        tail.push(
            ancestor
                .file_name()
                .ok_or_else(|| CheckpointError::Refused("invalid store".into()))?
                .to_owned(),
        );
        ancestor = ancestor
            .parent()
            .ok_or_else(|| CheckpointError::Refused("invalid store ancestor".into()))?;
    }
    let mut resolved = canonical_directory(ancestor)?;
    for name in tail.iter().rev() {
        resolved.push(name);
    }
    if resolved.starts_with(&root) || root.starts_with(&resolved) {
        return refuse("checkpoint store and workspace must be disjoint");
    }
    if create {
        fs::create_dir_all(&resolved)?;
    }
    let store = canonical_directory(&resolved)?;
    for child in [
        "blobs",
        "snapshots",
        "checkpoints",
        "previews",
        "transactions",
    ] {
        let path = store.join(child);
        check_chain(&path)?;
        if create && !path.try_exists()? {
            fs::create_dir(&path)?;
        }
        canonical_directory(&path)?;
    }
    Ok((root, store))
}

fn open_regular(path: &Path) -> Result<File> {
    check_chain(path)?;
    let meta = fs::symlink_metadata(path)?;
    reject_link(&meta, path)?;
    if !meta.is_file() {
        return refuse(format!("not a regular file: {}", path.display()));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    reject_link(&meta, path)?;
    if !meta.is_file() {
        return refuse("file type changed while opening");
    }
    #[cfg(windows)]
    let links = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        // SAFETY: a live File handle and correctly sized writable output buffer.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, info.as_mut_ptr()) } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        unsafe { info.assume_init() }.nNumberOfLinks as u64
    };
    #[cfg(unix)]
    let links = {
        use std::os::unix::fs::MetadataExt;
        meta.nlink()
    };
    #[cfg(not(any(windows, unix)))]
    let links = 0;
    if links != 1 {
        return refuse(format!("hard link/unknown link count: {}", path.display()));
    }
    Ok(file)
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    Ok(read_file_state(path, limit)?.0)
}

fn unix_mode(meta: &Metadata) -> Result<Option<u32>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o7777;
        if mode & 0o7000 != 0 {
            return refuse("special Unix permission bits are unsupported");
        }
        Ok(Some(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        Ok(None)
    }
}

fn validate_mode(mode: Option<u32>) -> Result<()> {
    if (cfg!(unix) && !matches!(mode, Some(0..=0o777))) || (!cfg!(unix) && mode.is_some()) {
        return refuse("missing, unsupported, or wrong-platform file mode");
    }
    Ok(())
}

// Content and mode are observed on the same open handle, including a post-read
// metadata check. This does not claim atomicity against external writers.
fn read_file_state(path: &Path, limit: u64) -> Result<(Vec<u8>, Option<u32>)> {
    let mut file = open_regular(path)?;
    let before = file.metadata()?;
    let mode = unix_mode(&before)?;
    if before.len() > limit {
        return refuse(format!("byte limit: {}", path.display()));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() as u64 > limit
        || before.len() != after.len()
        || bytes.len() as u64 != after.len()
        || before.modified()? != after.modified()?
        || mode != unix_mode(&after)?
    {
        return refuse(format!(
            "file changed/limit exceeded while reading: {}",
            path.display()
        ));
    }
    Ok((bytes, mode))
}

fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return refuse("manifest byte limit");
    }
    write_new(path, &bytes)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    check_chain(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| CheckpointError::Refused("missing parent".into()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path).map_err(|e| e.error)?;
    Ok(())
}

fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&read_bounded(
        path,
        MAX_JSON_BYTES,
    )?)?)
}

fn blob(store: &Path, entry: &Entry) -> Result<Vec<u8>> {
    let Entry::File {
        hash: digest,
        bytes,
        ..
    } = entry
    else {
        return refuse("expected blob entry");
    };
    if !valid_hex(digest, 64) || *bytes > 32 * 1024 * 1024 {
        return refuse("invalid blob reference");
    }
    let content = read_bounded(&store.join("blobs").join(digest), *bytes)?;
    if content.len() as u64 != *bytes || hash(&content) != *digest {
        return refuse("corrupt/missing blob");
    }
    Ok(content)
}

fn persist_blob(store: &Path, bytes: &[u8], unix_mode: Option<u32>) -> Result<Entry> {
    let entry = Entry::File {
        hash: hash(bytes),
        bytes: bytes.len() as u64,
        unix_mode,
    };
    let Entry::File { hash: digest, .. } = &entry else {
        unreachable!()
    };
    let path = store.join("blobs").join(digest);
    if path.try_exists()? {
        blob(store, &entry)?;
    } else {
        write_new(&path, bytes)?;
    }
    Ok(entry)
}

fn scan(
    root: &Path,
    store: Option<&Path>,
    limits: CaptureLimits,
) -> Result<BTreeMap<String, Entry>> {
    let mut entries = BTreeMap::new();
    let mut aliases = BTreeSet::new();
    let mut pending = vec![(root.to_owned(), 0)];
    let mut files = 0;
    let mut bytes = 0u64;
    let mut visited = 0usize;
    while let Some((directory, depth)) = pending.pop() {
        check_chain(&directory)?;
        for child in fs::read_dir(&directory)? {
            let child = child?;
            visited += 1;
            if visited > limits.max_entries {
                return refuse("entry limit exceeded");
            }
            let name = child.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| CheckpointError::Refused("non-UTF8 filename".into()))?;
            if excluded(name) {
                continue;
            }
            if depth >= limits.max_depth {
                return refuse("depth limit exceeded");
            }
            let path = child.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| CheckpointError::Refused("path escaped root".into()))?
                .components()
                .map(|p| p.as_os_str().to_str().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("/");
            valid_relative(&relative)?;
            if !aliases.insert(relative.to_lowercase()) {
                return refuse("case-aliased paths");
            }
            let meta = fs::symlink_metadata(&path)?;
            reject_link(&meta, &path)?;
            let entry = if meta.is_dir() {
                pending.push((path, depth + 1));
                Entry::Directory
            } else {
                files += 1;
                if files > limits.max_files {
                    return refuse("file limit exceeded");
                }
                let remaining = limits.max_total_bytes.saturating_sub(bytes);
                let (content, unix_mode) =
                    read_file_state(&path, limits.max_file_bytes.min(remaining))?;
                bytes += content.len() as u64;
                if let Some(store) = store {
                    persist_blob(store, &content, unix_mode)?
                } else {
                    Entry::File {
                        hash: hash(&content),
                        bytes: content.len() as u64,
                        unix_mode,
                    }
                }
            };
            entries.insert(relative, entry);
        }
    }
    Ok(entries)
}

fn fingerprint(entries: &BTreeMap<String, Entry>) -> Result<String> {
    Ok(hash(&serde_json::to_vec(&(
        SCHEMA_VERSION,
        EXCLUDED,
        entries,
    ))?))
}

fn capture_inner(root: &Path, store: &Path, limits: CaptureLimits) -> Result<Snapshot> {
    limits.validate()?;
    let (root, store_dir) = locations(root, store, true)?;
    let mut snapshot = Snapshot {
        schema_version: SCHEMA_VERSION,
        id: uuid::Uuid::new_v4().simple().to_string(),
        fingerprint: String::new(),
        complete: false,
        issues: Vec::new(),
        root,
        store_dir,
        limits,
        entries: BTreeMap::new(),
    };
    let result = (|| {
        let entries = scan(&snapshot.root, Some(&snapshot.store_dir), limits)?;
        let second = scan(&snapshot.root, None, limits)?;
        if entries != second {
            return refuse("workspace changed during capture");
        }
        Ok(entries)
    })();
    match result {
        Ok(entries) => {
            snapshot.entries = entries;
            snapshot.complete = true;
        }
        Err(error) => snapshot.issues.push(error.to_string()),
    }
    snapshot.fingerprint = fingerprint(&snapshot.entries)?;
    save_json(
        &snapshot
            .store_dir
            .join("snapshots")
            .join(format!("{}.json", snapshot.id)),
        &snapshot,
    )?;
    Ok(snapshot)
}

fn validate_snapshot(
    snapshot: &Snapshot,
    root: &Path,
    store: &Path,
    require_complete: bool,
) -> Result<()> {
    snapshot.limits.validate()?;
    if snapshot.schema_version != SCHEMA_VERSION
        || !valid_hex(&snapshot.id, 32)
        || snapshot.root != root
        || snapshot.store_dir != store
        || snapshot.fingerprint != fingerprint(&snapshot.entries)?
        || (require_complete && (!snapshot.complete || !snapshot.issues.is_empty()))
    {
        return refuse("incomplete, incompatible, or wrong-session snapshot");
    }
    if snapshot.entries.len() > snapshot.limits.max_entries {
        return refuse("manifest entry limit");
    }
    let mut aliases = BTreeSet::new();
    let mut files = 0;
    let mut bytes = 0u64;
    for (path, entry) in &snapshot.entries {
        valid_relative(path)?;
        if !aliases.insert(path.to_lowercase())
            || path.split('/').count() > snapshot.limits.max_depth
        {
            return refuse("manifest path alias/depth limit");
        }
        if let Some((parent, _)) = path.rsplit_once('/') {
            if snapshot.entries.get(parent) != Some(&Entry::Directory) {
                return refuse("manifest parent is not a directory");
            }
        }
        if let Entry::File {
            hash,
            bytes: size,
            unix_mode,
        } = entry
        {
            validate_mode(*unix_mode)?;
            if !valid_hex(hash, 64) || *size > snapshot.limits.max_file_bytes {
                return refuse("invalid file entry");
            }
            files += 1;
            bytes = bytes
                .checked_add(*size)
                .ok_or_else(|| CheckpointError::Refused("byte overflow".into()))?;
        }
    }
    if files > snapshot.limits.max_files || bytes > snapshot.limits.max_total_bytes {
        return refuse("manifest file/byte limit");
    }
    let persisted: Snapshot = load_json(
        &store
            .join("snapshots")
            .join(format!("{}.json", snapshot.id)),
    )?;
    if persisted != *snapshot {
        return refuse("snapshot differs from persisted manifest");
    }
    Ok(())
}

fn changed_paths(before: &Snapshot, after: &Snapshot) -> Result<Vec<String>> {
    let keys: BTreeSet<_> = before.entries.keys().chain(after.entries.keys()).collect();
    let mut paths = Vec::new();
    for key in keys {
        let old = before.entries.get(key);
        let new = after.entries.get(key);
        if old == new {
            continue;
        }
        if matches!(
            (old, new),
            (Some(Entry::Directory), Some(Entry::File { .. }))
                | (Some(Entry::File { .. }), Some(Entry::Directory))
        ) {
            return Err(CheckpointError::Conflict(format!(
                "directory/file transition: {key}"
            )));
        }
        if matches!(old, Some(Entry::File { .. })) || matches!(new, Some(Entry::File { .. })) {
            paths.push(key.clone());
        }
    }
    Ok(paths)
}

fn current_entry(root: &Path, relative: &str, limit: u64) -> Result<Option<Entry>> {
    valid_relative(relative)?;
    let path = root.join(relative);
    check_chain(&path)?;
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            reject_link(&meta, &path)?;
            if meta.is_dir() {
                return Ok(Some(Entry::Directory));
            }
            let (bytes, unix_mode) = read_file_state(&path, limit)?;
            Ok(Some(Entry::File {
                hash: hash(&bytes),
                bytes: bytes.len() as u64,
                unix_mode,
            }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn expect_current(root: &Path, path: &str, expected: Option<&Entry>, limit: u64) -> Result<()> {
    if current_entry(root, path, limit)?.as_ref() != expected {
        return Err(CheckpointError::Conflict(path.to_owned()));
    }
    Ok(())
}

fn preflight(
    root: &Path,
    store: &Path,
    before: &Snapshot,
    after: &Snapshot,
    paths: &[String],
) -> Result<()> {
    for path in paths {
        expect_current(
            root,
            path,
            after.entries.get(path),
            after.limits.max_file_bytes,
        )?;
        for snapshot in [before, after] {
            if let Some(entry @ Entry::File { .. }) = snapshot.entries.get(path) {
                blob(store, entry)?;
            }
        }
    }
    Ok(())
}

fn ensure_parent(root: &Path, relative: &str) -> Result<()> {
    valid_relative(relative)?;
    let mut directory = root.to_owned();
    let parts: Vec<_> = relative.split('/').collect();
    for part in &parts[..parts.len() - 1] {
        directory.push(part);
        check_chain(&directory)?;
        match fs::symlink_metadata(&directory) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(CheckpointError::Conflict(format!(
                    "parent is not a directory: {relative}"
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&directory)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn replace_bytes(
    root: &Path,
    relative: &str,
    bytes: &[u8],
    restored: &Entry,
    expected: Option<&Entry>,
    limit: u64,
) -> Result<()> {
    let Entry::File { unix_mode, .. } = restored else {
        return refuse("expected file restoration mode");
    };
    validate_mode(*unix_mode)?;
    ensure_parent(root, relative)?;
    let path = root.join(relative);
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    temp.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Apply after writing (writes can clear permission bits) and before
        // publishing: readers never observe tempfile's default 0600 mode.
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(unix_mode.unwrap()))?;
    }
    temp.as_file().sync_all()?;
    expect_current(root, relative, expected, limit)?;
    if expected.is_none() {
        temp.persist_noclobber(&path).map_err(|e| e.error)?;
    } else {
        temp.persist(&path).map_err(|e| e.error)?;
    }
    Ok(())
}

fn move_to_backup(
    root: &Path,
    relative: &str,
    backup: &Path,
    expected: Option<&Entry>,
    limit: u64,
) -> Result<()> {
    expect_current(root, relative, expected, limit)?;
    check_chain(backup)?;
    if backup.try_exists()? {
        return refuse("backup destination already exists");
    }
    fs::rename(root.join(relative), backup)?;
    Ok(())
}

fn apply_inner(
    root: &Path,
    store: &Path,
    token: &str,
    after_write: &mut dyn FnMut(usize) -> Result<()>,
) -> Result<RestoreReport> {
    if !valid_hex(token, 32) {
        return refuse("invalid preview token");
    }
    let (root, store) = locations(root, store, false)?;
    let plan = load_plan(&root, &store, token)?;
    preflight(&root, &store, &plan.before, &plan.after, &plan.paths)?;
    let transaction = store.join("transactions").join(token);
    check_chain(&transaction)?;
    fs::create_dir(&transaction)?; // Also consumes the token; never reused.
    save_json(&transaction.join("journal.json"), &plan)?;
    // Durable copies of ALL overwritten/removed content precede ANY mutation.
    for (i, path) in plan.paths.iter().enumerate() {
        if let Some(entry) = plan.after.entries.get(path) {
            write_new(
                &transaction.join(format!("{i}.backup")),
                &blob(&store, entry)?,
            )?;
        }
    }
    preflight(&root, &store, &plan.before, &plan.after, &plan.paths)?;
    let mut touched = Vec::new();
    let execution = (|| {
        for (i, path) in plan.paths.iter().enumerate() {
            expect_current(
                &root,
                path,
                plan.after.entries.get(path),
                plan.after.limits.max_file_bytes,
            )?;
            touched.push(i); // Include the attempted operation in rollback checks.
            match plan.before.entries.get(path) {
                Some(entry @ Entry::File { .. }) => replace_bytes(
                    &root,
                    path,
                    &blob(&store, entry)?,
                    entry,
                    plan.after.entries.get(path),
                    plan.after.limits.max_file_bytes,
                )?,
                None => move_to_backup(
                    &root,
                    path,
                    &transaction.join(format!("{i}.moved")),
                    plan.after.entries.get(path),
                    plan.after.limits.max_file_bytes,
                )?,
                _ => return refuse("unexpected directory restore"),
            }
            after_write(i)?;
        }
        for path in &plan.paths {
            expect_current(
                &root,
                path,
                plan.before.entries.get(path),
                plan.before.limits.max_file_bytes,
            )?;
        }
        save_json(&transaction.join("complete.json"), &true)?;
        Ok(())
    })();
    if let Err(error) = execution {
        let mut rollback_errors = Vec::new();
        for i in touched.into_iter().rev() {
            let path = &plan.paths[i];
            let rollback = (|| {
                let current = current_entry(&root, path, plan.before.limits.max_file_bytes)?;
                if current.as_ref() == plan.after.entries.get(path) {
                    return Ok(());
                }
                if current.as_ref() != plan.before.entries.get(path) {
                    return Err(CheckpointError::Conflict(format!("rollback: {path}")));
                }
                match plan.after.entries.get(path) {
                    Some(
                        entry @ Entry::File {
                            hash: digest,
                            bytes,
                            ..
                        },
                    ) => {
                        let original =
                            read_bounded(&transaction.join(format!("{i}.backup")), *bytes)?;
                        if hash(&original) != *digest {
                            return refuse("rollback backup corrupt");
                        }
                        replace_bytes(
                            &root,
                            path,
                            &original,
                            entry,
                            plan.before.entries.get(path),
                            plan.before.limits.max_file_bytes,
                        )?;
                        expect_current(&root, path, Some(entry), plan.after.limits.max_file_bytes)
                    }
                    None => move_to_backup(
                        &root,
                        path,
                        &transaction.join(format!("{i}.rollback")),
                        plan.before.entries.get(path),
                        plan.before.limits.max_file_bytes,
                    ),
                    _ => refuse("unexpected rollback directory"),
                }
            })();
            if let Err(error) = rollback {
                rollback_errors.push(format!("{path}: {error}"));
            }
        }
        // Failure to record the outcome is itself an explicit recovery error.
        if let Err(error) = save_json(
            &transaction.join("failed.json"),
            &(error.to_string(), &rollback_errors),
        ) {
            rollback_errors.push(format!("failure journal: {error}"));
        }
        return Err(CheckpointError::ApplyFailed {
            cause: error.to_string(),
            uncertain: !rollback_errors.is_empty(),
            rollback_errors,
            backup_dir: transaction,
        });
    }
    Ok(RestoreReport {
        schema_version: SCHEMA_VERSION,
        paths: plan.paths,
        backup_dir: transaction,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        store: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            // Canonicalize the fixture parent: /var can itself be an OS symlink.
            let temp = tempfile::tempdir().unwrap();
            let parent = fs::canonicalize(temp.path()).unwrap();
            let root = parent.join("workspace");
            fs::create_dir(&root).unwrap();
            Self {
                _temp: temp,
                root,
                store: parent.join("data/session"),
            }
        }

        fn write(&self, path: &str, bytes: &[u8]) {
            let path = self.root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn capture(&self) -> Snapshot {
            let snapshot = capture(&self.root, &self.store, CaptureLimits::default()).unwrap();
            assert!(snapshot.complete, "{:?}", snapshot.issues);
            snapshot
        }

        fn plan(&self, before: &Snapshot, after: &Snapshot) -> RestorePreview {
            plan_restore(before, after, &self.root, &self.store).unwrap()
        }

        fn apply(&self, preview: &RestorePreview) -> Result<RestoreReport> {
            apply(&self.root, &self.store, &preview.preview_token)
        }
    }

    #[test]
    fn non_git_exact_bytes_new_deleted_modified_and_unrelated_files() {
        let f = Fixture::new();
        f.write("modified.bin", &[0, 255, 10, 13, 0, 128]);
        f.write("gone/nested.txt", b"old\r\n\0");
        f.write("unchanged.txt", b"same");
        let before = f.capture();
        f.write("modified.bin", b"shell wrote here\n");
        fs::remove_file(f.root.join("gone/nested.txt")).unwrap();
        fs::remove_dir(f.root.join("gone")).unwrap();
        f.write("new/sub.txt", b"new file");
        let after = f.capture();
        f.write("unchanged.txt", b"later manual edit");
        f.write("unrelated.txt", b"outside recorded span");
        let preview = f.plan(&before, &after);
        assert_eq!(
            preview.paths,
            ["gone/nested.txt", "modified.bin", "new/sub.txt"]
        );
        let report = f.apply(&preview).unwrap();
        assert_eq!(
            fs::read(f.root.join("modified.bin")).unwrap(),
            [0, 255, 10, 13, 0, 128]
        );
        assert_eq!(
            fs::read(f.root.join("gone/nested.txt")).unwrap(),
            b"old\r\n\0"
        );
        assert!(!f.root.join("new/sub.txt").exists());
        assert_eq!(
            fs::read(report.backup_dir.join("2.moved")).unwrap(),
            b"new file"
        );
        assert_eq!(
            fs::read(report.backup_dir.join("1.backup")).unwrap(),
            b"shell wrote here\n"
        );
        assert_eq!(
            fs::read(f.root.join("unchanged.txt")).unwrap(),
            b"later manual edit"
        );
        assert_eq!(
            fs::read(f.root.join("unrelated.txt")).unwrap(),
            b"outside recorded span"
        );
        assert!(!f.root.join(".git").exists());
        assert!(report.backup_dir.join("complete.json").exists());
        assert!(f.apply(&preview).is_err());
    }

    #[test]
    fn snapshots_roundtrip_deduplicate_and_finish_host_id_is_opaque() {
        let f = Fixture::new();
        f.write("a", b"equal");
        f.write("b", b"equal");
        let before = f.capture();
        let same = f.capture();
        assert_ne!(before.id, same.id);
        assert!(before.same_state(&same));
        assert_eq!(fs::read_dir(f.store.join("blobs")).unwrap().count(), 1);
        let roundtrip: Snapshot =
            serde_json::from_slice(&serde_json::to_vec(&before).unwrap()).unwrap();
        assert_eq!(roundtrip, before);
        let id = "host/session/turn:42";
        f.write("a", b"next");
        let checkpoint = finish(&before, id).unwrap();
        assert_eq!(checkpoint.checkpoint_id, id);
        assert_eq!(checkpoint.before, before);
        assert!(!checkpoint.before.same_state(&checkpoint.after));
        let saved: Checkpoint = load_json(
            &f.store
                .join("checkpoints")
                .join(format!("{}.json", hash(id.as_bytes()))),
        )
        .unwrap();
        assert_eq!(saved.after, checkpoint.after);
        assert!(finish(&before, id).is_err());
    }

    #[test]
    fn hidden_and_gitignored_sources_covered_git_index_and_head_untouched() {
        let f = Fixture::new();
        f.write(".git/index", &[0, 255, 42]);
        f.write(".git/HEAD", b"ref: refs/heads/main\n");
        f.write(".gitignore", b"ignored.txt\n");
        f.write("ignored.txt", b"before");
        f.write(".config/source", b"hidden before");
        for name in ["node_modules", "target", ".next", "buildcache"] {
            f.write(&format!("nested/{name}/file"), b"excluded before");
        }
        let before = f.capture();
        f.write("ignored.txt", b"after");
        f.write(".config/source", b"hidden after");
        f.write(".git/index", b"staged changes outside checkpoint");
        f.write(".git/HEAD", b"ref: refs/heads/other\n");
        for name in ["node_modules", "target", ".next", "buildcache"] {
            f.write(&format!("nested/{name}/file"), b"excluded after");
        }
        let after = f.capture();
        f.apply(&f.plan(&before, &after)).unwrap();
        assert_eq!(fs::read(f.root.join("ignored.txt")).unwrap(), b"before");
        assert_eq!(
            fs::read(f.root.join(".config/source")).unwrap(),
            b"hidden before"
        );
        assert_eq!(
            fs::read(f.root.join(".git/index")).unwrap(),
            b"staged changes outside checkpoint"
        );
        assert_eq!(
            fs::read(f.root.join(".git/HEAD")).unwrap(),
            b"ref: refs/heads/other\n"
        );
        for name in ["node_modules", "target", ".next", "buildcache"] {
            assert_eq!(
                fs::read(f.root.join(format!("nested/{name}/file"))).unwrap(),
                b"excluded after"
            );
        }
    }

    #[test]
    fn limits_fail_closed_and_incomplete_manifests_are_persisted() {
        for limits in [
            CaptureLimits {
                max_files: 0,
                ..Default::default()
            },
            CaptureLimits {
                max_entries: 0,
                ..Default::default()
            },
            CaptureLimits {
                max_file_bytes: 1,
                ..Default::default()
            },
            CaptureLimits {
                max_total_bytes: 1,
                ..Default::default()
            },
            CaptureLimits {
                max_depth: 0,
                ..Default::default()
            },
        ] {
            let f = Fixture::new();
            f.write("a", b"bytes");
            let incomplete = capture(&f.root, &f.store, limits).unwrap();
            assert!(!incomplete.complete);
            assert!(!incomplete.issues.is_empty());
            assert!(!incomplete.same_state(&incomplete));
            let persisted: Snapshot = load_json(
                &f.store
                    .join("snapshots")
                    .join(format!("{}.json", incomplete.id)),
            )
            .unwrap();
            assert_eq!(persisted, incomplete);
            assert!(plan_restore(&incomplete, &incomplete, &f.root, &f.store).is_err());
            assert_eq!(fs::read(f.root.join("a")).unwrap(), b"bytes");
        }
    }

    #[test]
    fn total_byte_budget_counts_duplicate_content() {
        let f = Fixture::new();
        f.write("a", b"123");
        f.write("b", b"123");
        let snapshot = capture(
            &f.root,
            &f.store,
            CaptureLimits {
                max_total_bytes: 5,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!snapshot.complete);
    }

    #[test]
    fn changed_path_external_edit_conflicts_before_any_write() {
        let f = Fixture::new();
        f.write("a", b"before a");
        f.write("z", b"before z");
        let before = f.capture();
        f.write("a", b"after a");
        f.write("z", b"after z");
        let after = f.capture();
        f.write("z", b"manual z");
        assert!(plan_restore(&before, &after, &f.root, &f.store).is_err());
        assert_eq!(fs::read(f.root.join("a")).unwrap(), b"after a");
        assert_eq!(
            fs::read_dir(f.store.join("transactions")).unwrap().count(),
            0
        );
    }

    #[test]
    fn stale_preview_and_recreated_deleted_file_are_refused() {
        let f = Fixture::new();
        f.write("a", b"before");
        let before = f.capture();
        fs::remove_file(f.root.join("a")).unwrap();
        let after = f.capture();
        let preview = f.plan(&before, &after);
        f.write("a", b"recreated manually");
        assert!(f.apply(&preview).is_err());
        assert_eq!(fs::read(f.root.join("a")).unwrap(), b"recreated manually");
        assert_eq!(
            fs::read_dir(f.store.join("transactions")).unwrap().count(),
            0
        );
    }

    #[test]
    fn modified_file_stale_preview_is_refused() {
        let f = Fixture::new();
        f.write("a", b"before");
        let before = f.capture();
        f.write("a", b"after");
        let after = f.capture();
        let preview = f.plan(&before, &after);
        f.write("a", b"manual edit");
        assert!(f.apply(&preview).is_err());
        assert_eq!(fs::read(f.root.join("a")).unwrap(), b"manual edit");
    }

    #[test]
    fn adjacent_span_restores_first_state_and_rejects_interturn_gap() {
        let f = Fixture::new();
        f.write("a", b"zero");
        let zero = f.capture();
        f.write("a", b"one");
        let one = f.capture();
        let start_two = f.capture();
        f.write("a", b"two");
        let two = f.capture();
        let chain = [
            CheckpointBoundary {
                before: zero.clone(),
                after: one.clone(),
            },
            CheckpointBoundary {
                before: start_two,
                after: two.clone(),
            },
        ];
        let preview = plan_restore_span(&chain, &f.root, &f.store).unwrap();
        f.apply(&preview).unwrap();
        assert_eq!(fs::read(f.root.join("a")).unwrap(), b"zero");
        // Even an unrelated inter-turn edit must make the full chain ineligible.
        f.write("a", b"one");
        f.write("manual", b"between turns");
        let gap = f.capture();
        f.write("a", b"two");
        let after_gap = f.capture();
        let chain = [
            CheckpointBoundary {
                before: zero,
                after: one,
            },
            CheckpointBoundary {
                before: gap,
                after: after_gap,
            },
        ];
        let error = plan_restore_span(&chain, &f.root, &f.store).unwrap_err();
        assert!(error.to_string().contains("inter-turn gap"));
        assert_eq!(fs::read(f.root.join("a")).unwrap(), b"two");
    }

    #[test]
    fn coverage_mismatch_and_wrong_session_or_root_are_refused() {
        let f = Fixture::new();
        let other = Fixture::new();
        let before = f.capture();
        let after = capture(
            &f.root,
            &f.store,
            CaptureLimits {
                max_files: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!before.same_state(&after));
        assert!(plan_restore(&before, &after, &f.root, &f.store).is_err());
        other.capture();
        assert!(plan_restore(&before, &before, &other.root, &other.store).is_err());
        let preview = f.plan(&before, &before);
        assert!(apply(&other.root, &f.store, &preview.preview_token).is_err());
        assert!(apply(&f.root, &f.store, "../../outside").is_err());
    }

    #[test]
    fn file_directory_transitions_in_both_directions_fail_closed() {
        for file_first in [true, false] {
            let f = Fixture::new();
            if file_first {
                f.write("a", b"file");
            } else {
                f.write("a/child", b"child");
            }
            let before = f.capture();
            if file_first {
                fs::remove_file(f.root.join("a")).unwrap();
                f.write("a/child", b"child");
            } else {
                fs::remove_file(f.root.join("a/child")).unwrap();
                fs::remove_dir(f.root.join("a")).unwrap();
                f.write("a", b"file");
            }
            let after = f.capture();
            assert!(plan_restore(&before, &after, &f.root, &f.store).is_err());
        }
    }

    #[test]
    fn current_directory_or_file_parent_conflict_refuses_entire_plan() {
        for block_parent in [false, true] {
            let f = Fixture::new();
            f.write("a", b"old a");
            f.write("dir/z", b"old z");
            let before = f.capture();
            f.write("a", b"new a");
            fs::remove_file(f.root.join("dir/z")).unwrap();
            let after = f.capture();
            let preview = f.plan(&before, &after);
            if block_parent {
                fs::remove_dir(f.root.join("dir")).unwrap();
                f.write("dir", b"blocking parent");
            } else {
                fs::create_dir(f.root.join("dir/z")).unwrap();
            }
            assert!(f.apply(&preview).is_err());
            assert_eq!(fs::read(f.root.join("a")).unwrap(), b"new a");
        }
    }

    #[test]
    fn unsafe_relative_paths_and_store_overlap_are_rejected_without_creation() {
        for path in [
            "../a",
            "/a",
            "a/../b",
            "a\\b",
            "a:stream",
            "a//b",
            "a/./b",
            ".git/index",
            "a/.GIT/x",
            "NUL",
            "COM1.txt",
            "name.",
            "name ",
        ] {
            assert!(valid_relative(path).is_err(), "{path}");
        }
        let f = Fixture::new();
        assert!(capture(
            &f.root,
            &f.root.join("new-store/deeper"),
            CaptureLimits::default()
        )
        .is_err());
        assert!(!f.root.join("new-store").exists());
        assert!(capture(&f.root, &f.root, CaptureLimits::default()).is_err());
        assert!(capture(&f.root, f.root.parent().unwrap(), CaptureLimits::default()).is_err());
        fs::create_dir(f.root.join(".git")).unwrap();
        assert!(capture(&f.root.join(".git"), &f.store, CaptureLimits::default()).is_err());
    }

    #[test]
    fn hardlinks_make_capture_incomplete_and_postpreview_hardlinks_refuse() {
        let f = Fixture::new();
        f.write("a", b"before");
        let before = f.capture();
        f.write("a", b"after");
        let after = f.capture();
        let preview = f.plan(&before, &after);
        let external = f.root.parent().unwrap().join("external-hardlink");
        fs::hard_link(f.root.join("a"), &external).unwrap();
        assert!(f.apply(&preview).is_err());
        assert_eq!(fs::read(&external).unwrap(), b"after");
        let incomplete = capture(&f.root, &f.store, CaptureLimits::default()).unwrap();
        assert!(!incomplete.complete);
        assert!(incomplete.issues[0].contains("hard link"));
    }

    // Directory junctions on Windows exercise reparse rejection without needing
    // developer mode or symlink privilege. Unix exercises real directory symlinks.
    fn directory_link(target: &Path, link: &Path) {
        #[cfg(windows)]
        junction::create(target, link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[test]
    #[cfg(any(windows, unix))]
    fn nested_directory_links_refuse_capture_and_stale_restore() {
        let f = Fixture::new();
        f.write("nested/dir/file", b"before");
        let before = f.capture();
        f.write("nested/dir/file", b"after");
        let after = f.capture();
        let preview = f.plan(&before, &after);
        let external = f.root.parent().unwrap().join("external-dir");
        fs::rename(f.root.join("nested/dir"), &external).unwrap();
        directory_link(&external, &f.root.join("nested/dir"));
        assert!(f.apply(&preview).is_err());
        assert_eq!(fs::read(external.join("file")).unwrap(), b"after");
        let incomplete = capture(&f.root, &f.store, CaptureLimits::default()).unwrap();
        assert!(!incomplete.complete);
    }

    #[test]
    #[cfg(any(windows, unix))]
    fn root_and_store_links_are_refused() {
        let f = Fixture::new();
        let linked_root = f.root.parent().unwrap().join("linked-root");
        directory_link(&f.root, &linked_root);
        assert!(capture(&linked_root, &f.store, CaptureLimits::default()).is_err());
        let real_store = f.root.parent().unwrap().join("real-store");
        fs::create_dir(&real_store).unwrap();
        let linked_store = f.root.parent().unwrap().join("linked-store");
        directory_link(&real_store, &linked_store);
        assert!(capture(
            &f.root,
            &linked_store.join("session"),
            CaptureLimits::default()
        )
        .is_err());
        assert!(!real_store.join("session").exists());
    }

    #[test]
    fn corrupt_blob_or_manifest_prevents_all_writes() {
        let f = Fixture::new();
        f.write("a", b"before");
        let before = f.capture();
        f.write("a", b"after");
        let after = f.capture();
        let preview = f.plan(&before, &after);
        fs::write(f.store.join("blobs").join(hash(b"before")), b"tamper").unwrap();
        assert!(f.apply(&preview).is_err());
        assert_eq!(fs::read(f.root.join("a")).unwrap(), b"after");
        let mut forged = before.clone();
        forged.entries.insert(
            "../outside".into(),
            Entry::File {
                hash: hash(b"before"),
                bytes: 6,
                unix_mode: if cfg!(unix) { Some(0o644) } else { None },
            },
        );
        forged.fingerprint = fingerprint(&forged.entries).unwrap();
        assert!(plan_restore(&forged, &after, &f.root, &f.store).is_err());
        fs::write(
            f.store.join("snapshots").join(format!("{}.json", after.id)),
            b"{}",
        )
        .unwrap();
        assert!(f.apply(&preview).is_err());
    }

    #[test]
    fn failed_apply_rolls_back_modified_new_and_deleted_bytes_and_keeps_backups() {
        let f = Fixture::new();
        f.write("a-modified", b"old");
        f.write("b-deleted", b"deleted");
        let before = f.capture();
        f.write("a-modified", b"new");
        fs::remove_file(f.root.join("b-deleted")).unwrap();
        f.write("c-created", b"created");
        let after = f.capture();
        let preview = f.plan(&before, &after);
        let _guard = lock().unwrap();
        let error = apply_inner(&f.root, &f.store, &preview.preview_token, &mut |i| {
            if i == 2 {
                refuse("injected write failure")
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        let CheckpointError::ApplyFailed {
            uncertain,
            rollback_errors,
            backup_dir,
            ..
        } = error
        else {
            panic!("unexpected error");
        };
        assert!(!uncertain, "{rollback_errors:?}");
        assert_eq!(fs::read(f.root.join("a-modified")).unwrap(), b"new");
        assert!(!f.root.join("b-deleted").exists());
        assert_eq!(fs::read(f.root.join("c-created")).unwrap(), b"created");
        assert_eq!(fs::read(backup_dir.join("1.rollback")).unwrap(), b"deleted");
        assert!(backup_dir.join("2.moved").exists());
        assert!(backup_dir.join("failed.json").exists());
    }

    #[test]
    fn rollback_refuses_external_edit_and_reports_uncertain() {
        let f = Fixture::new();
        f.write("a", b"before");
        let before = f.capture();
        f.write("a", b"after");
        let after = f.capture();
        let preview = f.plan(&before, &after);
        let _guard = lock().unwrap();
        let error = apply_inner(&f.root, &f.store, &preview.preview_token, &mut |_| {
            f.write("a", b"concurrent external edit");
            refuse("injected failure after external edit")
        })
        .unwrap_err();
        let CheckpointError::ApplyFailed {
            uncertain,
            rollback_errors,
            backup_dir,
            ..
        } = error
        else {
            panic!("unexpected error");
        };
        assert!(uncertain);
        assert!(!rollback_errors.is_empty());
        assert_eq!(
            fs::read(f.root.join("a")).unwrap(),
            b"concurrent external edit"
        );
        assert_eq!(fs::read(backup_dir.join("0.backup")).unwrap(), b"after");
    }

    #[test]
    fn snapshot_fingerprint_binds_bytes_paths_types_and_valid_coverage() {
        let f = Fixture::new();
        f.write("a", b"bytes");
        let before = f.capture();
        let mut fake = before.clone();
        fake.entries.insert("extra".into(), Entry::Directory);
        // Even a copied fingerprint cannot hide forged entries in same_state.
        assert!(!before.same_state(&fake));
        fake = before.clone();
        fake.issues.push("incomplete".into());
        assert!(!fake.same_state(&fake));
        fs::rename(f.root.join("a"), f.root.join("b")).unwrap();
        let renamed = f.capture();
        assert_ne!(before.fingerprint, renamed.fingerprint);
        fs::remove_file(f.root.join("b")).unwrap();
        fs::create_dir(f.root.join("b")).unwrap();
        let directory = f.capture();
        assert_ne!(renamed.fingerprint, directory.fingerprint);
    }

    #[test]
    fn preview_tampering_and_backup_hardlink_are_refused() {
        let f = Fixture::new();
        f.write("a", b"before");
        let before = f.capture();
        f.write("a", b"after");
        let after = f.capture();
        let preview = f.plan(&before, &after);
        let plan_path = f
            .store
            .join("previews")
            .join(format!("{}.json", preview.preview_token));
        let mut plan: Plan = load_json(&plan_path).unwrap();
        plan.paths.push("../outside".into());
        fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();
        assert!(f.apply(&preview).is_err());
        assert_eq!(fs::read(f.root.join("a")).unwrap(), b"after");
        let preview = f.plan(&before, &after);
        let blob_path = f.store.join("blobs").join(hash(b"before"));
        let external = f.root.parent().unwrap().join("blob-hardlink");
        fs::hard_link(&blob_path, &external).unwrap();
        assert!(f.apply(&preview).is_err());
        assert_eq!(fs::read(&external).unwrap(), b"before");
    }

    #[test]
    #[cfg(windows)]
    fn windows_denied_file_read_makes_capture_incomplete() {
        use std::os::windows::fs::OpenOptionsExt;
        let f = Fixture::new();
        f.write("locked", b"no shared read access");
        let handle = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(f.root.join("locked"))
            .unwrap();
        let incomplete = capture(&f.root, &f.store, CaptureLimits::default()).unwrap();
        assert!(!incomplete.complete);
        assert!(!incomplete.issues.is_empty());
        assert!(plan_restore(&incomplete, &incomplete, &f.root, &f.store).is_err());
        drop(handle);
        assert!(f.capture().complete);
    }

    #[test]
    #[cfg(windows)]
    fn windows_case_variant_cannot_put_store_in_workspace() {
        let f = Fixture::new();
        let different_case =
            PathBuf::from(f.root.to_string_lossy().to_uppercase()).join("bad-store");
        assert!(capture(&f.root, &different_case, CaptureLimits::default()).is_err());
        assert!(!f.root.join("bad-store").exists());
    }

    #[test]
    fn store_inside_external_git_metadata_is_refused_before_creation() {
        let f = Fixture::new();
        let store = f.root.parent().unwrap().join("other/.git/checkpoints");
        assert!(capture(&f.root, &store, CaptureLimits::default()).is_err());
        assert!(!f.root.parent().unwrap().join("other").exists());
    }

    #[test]
    fn old_schema_without_permissions_is_ineligible_for_restore() {
        let f = Fixture::new();
        f.write("a", b"before");
        let before = f.capture();
        let mut old = serde_json::to_value(&before).unwrap();
        old["schemaVersion"] = serde_json::json!(1);
        old["entries"]["a"]
            .as_object_mut()
            .unwrap()
            .remove("unixMode");
        let old: Snapshot = serde_json::from_value(old).unwrap();
        assert!(!old.same_state(&before));
        assert!(plan_restore(&old, &before, &f.root, &f.store).is_err());
        assert_eq!(fs::read(f.root.join("a")).unwrap(), b"before");
    }

    #[test]
    fn validate_restored_checks_live_bytes_and_ignores_unrelated_changes() {
        let f = Fixture::new();
        f.write("affected", b"before");
        f.write("unrelated", b"original unrelated");
        let before = f.capture();
        f.write("affected", b"after");
        let after = f.capture();
        let preview = f.plan(&before, &after);
        assert!(validate_restored(&f.root, &f.store, &preview.preview_token).is_err());
        let report = f.apply(&preview).unwrap();
        let check = || validate_restored(&f.root, &f.store, &preview.preview_token);
        check().unwrap();
        f.write("unrelated", b"manual unrelated edit");
        f.write("new-unrelated", b"keep this too");
        check().unwrap();
        f.write("affected", b"manual after restore");
        assert!(matches!(check(), Err(CheckpointError::Conflict(_))));
        assert_eq!(
            fs::read(f.root.join("affected")).unwrap(),
            b"manual after restore"
        );
        assert_eq!(
            fs::read(report.backup_dir.join("complete.json")).unwrap(),
            b"true"
        );
        assert!(!report.backup_dir.join("failed.json").exists());
        f.write("affected", b"before");
        check().unwrap();
        assert_eq!(
            fs::read(f.root.join("unrelated")).unwrap(),
            b"manual unrelated edit"
        );
    }

    #[test]
    fn validate_restored_rejects_missing_file_recreated_addition_and_type_conflict() {
        let f = Fixture::new();
        f.write("deleted", b"original deleted bytes");
        let before = f.capture();
        fs::remove_file(f.root.join("deleted")).unwrap();
        f.write("added", b"added bytes");
        let after = f.capture();
        let preview = f.plan(&before, &after);
        f.apply(&preview).unwrap();
        let check = || validate_restored(&f.root, &f.store, &preview.preview_token);
        check().unwrap();
        f.write("added", b"external recreation");
        assert!(check().is_err());
        assert_eq!(
            fs::read(f.root.join("added")).unwrap(),
            b"external recreation"
        );
        fs::remove_file(f.root.join("added")).unwrap();
        fs::remove_file(f.root.join("deleted")).unwrap();
        assert!(check().is_err());
        fs::create_dir(f.root.join("deleted")).unwrap();
        assert!(check().is_err());
        assert!(f.root.join("deleted").is_dir());
    }

    #[test]
    fn validate_restored_rejects_tampered_durable_evidence_and_wrong_root() {
        let f = Fixture::new();
        let other = Fixture::new();
        f.write("a", b"before");
        let before = f.capture();
        f.write("a", b"after");
        let after = f.capture();
        let preview = f.plan(&before, &after);
        let report = f.apply(&preview).unwrap();
        let check = || validate_restored(&f.root, &f.store, &preview.preview_token);
        check().unwrap();
        assert!(validate_restored(&other.root, &f.store, &preview.preview_token).is_err());
        assert!(validate_restored(&f.root, &f.store, "../outside").is_err());
        let preview_path = f
            .store
            .join("previews")
            .join(format!("{}.json", preview.preview_token));
        for path in [preview_path, report.backup_dir.join("journal.json")] {
            let original = fs::read(&path).unwrap();
            let mut plan: Plan = serde_json::from_slice(&original).unwrap();
            plan.schema_version = 1;
            fs::write(&path, serde_json::to_vec(&plan).unwrap()).unwrap();
            assert!(check().is_err());
            fs::write(&path, &original).unwrap();
            check().unwrap();
        }
        let marker = report.backup_dir.join("complete.json");
        for invalid in [b"false".as_slice(), b"{}", b"corrupt"] {
            fs::write(&marker, invalid).unwrap();
            assert!(check().is_err());
        }
        fs::remove_file(&marker).unwrap();
        assert!(check().is_err());
        fs::write(&marker, b"true").unwrap();
        check().unwrap();
        fs::write(report.backup_dir.join("failed.json"), b"failure").unwrap();
        assert!(check().is_err());
        fs::remove_file(report.backup_dir.join("failed.json")).unwrap();
        fs::write(f.store.join("blobs").join(hash(b"before")), b"tamper").unwrap();
        assert!(check().is_err());
        assert_eq!(fs::read(f.root.join("a")).unwrap(), b"before");
    }

    #[cfg(unix)]
    mod unix_permissions {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn chmod(f: &Fixture, path: &str, mode: u32) {
            fs::set_permissions(f.root.join(path), fs::Permissions::from_mode(mode)).unwrap();
        }

        fn mode(f: &Fixture, path: &str) -> u32 {
            fs::metadata(f.root.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        }

        #[test]
        fn validate_restored_rejects_external_chmod_without_mutation() {
            let f = Fixture::new();
            f.write("script", b"before");
            chmod(&f, "script", 0o751);
            let before = f.capture();
            f.write("script", b"after");
            let after = f.capture();
            let preview = f.plan(&before, &after);
            f.apply(&preview).unwrap();
            let check = || validate_restored(&f.root, &f.store, &preview.preview_token);
            check().unwrap();
            chmod(&f, "script", 0o600);
            assert!(matches!(check(), Err(CheckpointError::Conflict(_))));
            assert_eq!(mode(&f, "script"), 0o600);
            assert_eq!(fs::read(f.root.join("script")).unwrap(), b"before");
            chmod(&f, "script", 0o751);
            check().unwrap();
        }

        #[test]
        fn executable_and_deleted_script_restore_original_modes_after_json_roundtrip() {
            let f = Fixture::new();
            f.write("existing.sh", b"#!/bin/sh\nprintf 'original'\n");
            chmod(&f, "existing.sh", 0o751);
            f.write("deleted.sh", b"#!/bin/sh\nprintf 'deleted'\n");
            chmod(&f, "deleted.sh", 0o750);
            let before = f.capture();
            let before: Snapshot =
                serde_json::from_slice(&serde_json::to_vec(&before).unwrap()).unwrap();
            f.write("existing.sh", b"#!/bin/sh\nprintf 'changed'\n");
            fs::remove_file(f.root.join("deleted.sh")).unwrap();
            let after = f.capture();
            f.apply(&f.plan(&before, &after)).unwrap();
            assert_eq!(mode(&f, "existing.sh"), 0o751);
            assert_eq!(mode(&f, "deleted.sh"), 0o750);
            for (name, expected) in [
                ("existing.sh", b"original".as_slice()),
                ("deleted.sh", b"deleted".as_slice()),
            ] {
                let output = std::process::Command::new(f.root.join(name))
                    .output()
                    .unwrap();
                assert!(output.status.success());
                assert_eq!(output.stdout, expected);
            }
        }

        #[test]
        fn chmod_only_change_is_fingerprinted_and_restored() {
            let f = Fixture::new();
            f.write("script", b"same bytes");
            chmod(&f, "script", 0o755);
            let before = f.capture();
            chmod(&f, "script", 0o640);
            let after = f.capture();
            assert_ne!(before.fingerprint, after.fingerprint);
            assert!(!before.same_state(&after));
            assert_eq!(fs::read_dir(f.store.join("blobs")).unwrap().count(), 1);
            let preview = f.plan(&before, &after);
            assert_eq!(preview.paths, ["script"]);
            f.apply(&preview).unwrap();
            assert_eq!(mode(&f, "script"), 0o755);
        }

        #[test]
        fn external_chmod_rejects_stale_preview_and_interturn_gap() {
            let f = Fixture::new();
            f.write("script", b"before");
            chmod(&f, "script", 0o755);
            let before = f.capture();
            f.write("script", b"after");
            let after = f.capture();
            let preview = f.plan(&before, &after);
            chmod(&f, "script", 0o700);
            assert!(f.apply(&preview).is_err());
            assert_eq!(mode(&f, "script"), 0o700);
            assert_eq!(fs::read(f.root.join("script")).unwrap(), b"after");
            let next_before = f.capture();
            f.write("script", b"next turn");
            let next_after = f.capture();
            let error = plan_restore_span(
                &[
                    CheckpointBoundary { before, after },
                    CheckpointBoundary {
                        before: next_before,
                        after: next_after,
                    },
                ],
                &f.root,
                &f.store,
            )
            .unwrap_err();
            assert!(error.to_string().contains("inter-turn gap"));
        }

        #[test]
        fn rollback_restores_after_modes_for_overwritten_and_moved_files() {
            let f = Fixture::new();
            f.write("a-existing", b"before");
            chmod(&f, "a-existing", 0o751);
            let before = f.capture();
            f.write("a-existing", b"after");
            chmod(&f, "a-existing", 0o750);
            f.write("b-new", b"new executable");
            chmod(&f, "b-new", 0o711);
            let after = f.capture();
            let preview = f.plan(&before, &after);
            let _guard = lock().unwrap();
            let error = apply_inner(&f.root, &f.store, &preview.preview_token, &mut |i| {
                if i == 1 {
                    refuse("injected failure after moving new script")
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert!(matches!(
                error,
                CheckpointError::ApplyFailed {
                    uncertain: false,
                    ..
                }
            ));
            assert_eq!(mode(&f, "a-existing"), 0o750);
            assert_eq!(mode(&f, "b-new"), 0o711);
            assert_eq!(fs::read(f.root.join("a-existing")).unwrap(), b"after");
            assert_eq!(fs::read(f.root.join("b-new")).unwrap(), b"new executable");
        }

        #[test]
        fn missing_or_privileged_mode_fails_closed() {
            let f = Fixture::new();
            f.write("script", b"bytes");
            chmod(&f, "script", 0o755);
            let before = f.capture();
            let mut missing = before.clone();
            let Entry::File { unix_mode, .. } = missing.entries.get_mut("script").unwrap() else {
                unreachable!()
            };
            *unix_mode = None;
            missing.fingerprint = fingerprint(&missing.entries).unwrap();
            assert!(plan_restore(&missing, &before, &f.root, &f.store).is_err());
            chmod(&f, "script", 0o4755);
            let result = capture(&f.root, &f.store, CaptureLimits::default()).unwrap();
            chmod(&f, "script", 0o755);
            assert!(!result.complete);
            assert!(result.issues[0].contains("permission bits"));
        }
    }

    #[test]
    #[cfg(unix)]
    fn file_symlink_is_incomplete_and_special_file_is_never_opened() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let external = f.root.parent().unwrap().join("external");
        fs::write(&external, b"external bytes").unwrap();
        symlink(&external, f.root.join("link")).unwrap();
        assert!(
            !capture(&f.root, &f.store, CaptureLimits::default())
                .unwrap()
                .complete
        );
        fs::remove_file(f.root.join("link")).unwrap();
        let fifo = std::ffi::CString::new(f.root.join("fifo").to_str().unwrap()).unwrap();
        // SAFETY: valid NUL-terminated path in our isolated test fixture.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(
            !capture(&f.root, &f.store, CaptureLimits::default())
                .unwrap()
                .complete
        );
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_file_is_incomplete_when_os_denies_read() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        f.write("private", b"no read access");
        let path = f.root.join("private");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let denied = File::open(&path).is_err();
        let result = capture(&f.root, &f.store, CaptureLimits::default()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        if denied {
            assert!(!result.complete);
        }
    }
}
