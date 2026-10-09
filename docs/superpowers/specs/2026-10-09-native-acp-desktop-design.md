# Native ACP desktop integration

Based on official Codeg v0.34.0, commit 592131c72e2a05478e7f8398a91cb653c8e3996f. This implements the user's approved direction: native same-session editing and the completed adapters' capabilities, preserving existing Codeg behavior and local data.

## Boundary

The host consumes the installed full enhanced Codex and Claude adapters. It must negotiate initialize metadata, not infer features from package versions. Existing adapters remain usable; unsupported controls stay absent. No raw native RPC tunnel, fork-based edit fallback, transcript rewriting or custom workspace backup implementation.

## Shared backend contract

Tauri and HTTP expose `acp_native_capabilities({connectionId})` returning the sanitized advertised metadata object, and `acp_native_operation({connectionId,operation,params})` returning the native JSON result. `operation` is a closed allowlist: runtime_read, runtime_control, rewind, rewind_files, file_revert, queue, mcp_state, mcp_set, archive, unarchive, search, attachments, goal. Params must be an object with bounded size; the backend injects the bound sessionId and rejects supplied alternate session identity. Each operation maps to one fixed extension and checks its actual advertised version/method/actions. Rewind receives a Codeg turn ID, resolves it against freshly parsed persisted history, and constructs beforeMessage/resumeAtMessage guards itself. No text/timestamp guess identifies optimistic turns.

Idle and in-prompt command loops both route supported controls. Mutations are serialized with normal prompt admission and connection lifecycle; mutations whose response is lost block further destructive/send operations until reconnection. Never automatically retry mutations. Native refusal remains refusal, not host success. Queue change and turn notifications are consumed before queue writes are enabled; native active/completed/interrupted/failed boundaries feed existing state and approval routing without synthesizing another prompt.

## Editing and history

An Edit action on persisted user messages opens a text/image-preserving editor. Native rewind removes the target and following suffix but keeps session ID; after confirmed success reload persisted history, clear stale live overlays, and submit the edited draft through the existing prompt flow only once. If submission fails, preserve the draft. First/latest/historical and repeated text identities are checked. Busy users can stop normally, then retry the explicit edit; an uncertain result requests reconnection rather than resubmission.

Claude parser must honor explicit last-prompt anchors and their parent chain, including an empty first-message rewind and subsequent continuation. Codex already handles history_base/thread-revert lineage; validate its legacy rollback path and user fingerprint projection. A parser must never display a discarded suffix as current history.

## Files

File restore is a separate explicit operation. Claude previews a selected user checkpoint and restores only when canRewind; no checkpoint means unavailable. Codex restores one completed fileChange tool with a preview token; not a whole-session snapshot. Show affected paths, skipped links and conflicts. Re-preview on target changes. Do not combine history+file mutation into a claimed transaction.

## Session controls

A capability-gated session tools panel offers context/usage/MCP/commands/agents/plugins reads and supported refresh/reconnect/toggle/background operations. Context summary is default; full diagnostics requires an explicit click with visible network/counting implications. Native response data is presented as readable fields/lists with expandable structured details, not a user-authored arbitrary request editor.

Codex native queue offers list/add/edit/delete/reorder/start, using exact pending IDs and native UserInput. Native queue and host queue are visibly distinct and cannot both dispatch the same draft. Claude lists native pending IDs and supports one cancellation only. A native false ACK is not cancellation. Native automatic queue start changes connection lifecycle so ordinary composer submissions are not silently duplicated.

MCP replacement applies only the Codeg-owned ACP set with expectedRevision, preserving internal companion entries and ownership; native archive differs from local hide/permanent delete. Search and attachments retain their native metadata semantics. Goal actions use negotiated vocabulary. No new claims of complete proprietary Desktop parity.

## Delivery

Keep old source checkout untouched. Build and publish a clearly identified community prerelease for Windows x64 and Linux ARM64 on Wolf-L/codeg after adapter packages are published. Back up installed app and consistent SQLite data before local replacement. Preserve credentials, provider settings, sessions and original upstream PR branches. No official updater signature claims or silent switch of update trust.

## Verification

Targeted backend tests cover capability refusal, bound identity, pending/active queue transitions, stale/unknown mutation outcomes and history guards. UI tests cover unsupported/busy states, draft preservation, preview confirmation and queue actions. Run frontend checks/build, Rust checks/tests/clippy appropriate to changed paths, and isolated real-ACP/native flows. Verify packaged artifacts separately; Linux ARM64 QEMU is not physical hardware evidence. Record failures accurately.
