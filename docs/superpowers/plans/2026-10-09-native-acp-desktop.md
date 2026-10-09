# Native ACP Desktop Implementation Plan

**Goal:** Integrate the completed native ACP extensions into official Codeg v0.34.0 and deliver verified Windows/Linux ARM64 packages.

**Architecture:** Fixed, capability-gated backend operations shared across Tauri/Web; native transcript/queue authority; user-facing conversation editing and session tools. Separate history and file restoration.

**Tech Stack:** Rust/Tokio/ACP/Tauri 2, React/TypeScript/Next.js, Vitest, Windows NSIS and Linux ARM64 Debian packaging.

**Spec:** `docs/superpowers/specs/2026-10-09-native-acp-desktop-design.md`

## Global constraints

- Base 592131c72e2a05478e7f8398a91cb653c8e3996f; no edits to the original partially cleared checkout.
- Preserve same session ID; no edit-as-fork or native transcript writes.
- Capability negotiation, mutation fences, explicit full context, no automatic mutation retries.
- Preserve user configuration and data; package provenance and honest test limitations.

## 1. Protocol and backend

Files: new `src-tauri/src/acp/native_session.rs`; modify `connection.rs`, `manager.rs`, `session_state.rs`, `mod.rs`, `commands/acp.rs`, `web/handlers/acp.rs`, `web/router.rs`, `lib.rs`.

- [ ] Define sanitized capability extraction and `NativeOperation` fixed operation vocabulary.
- [ ] Add capability/operation command variants with oneshot replies; install metadata at initialize; implement active and idle arms.
- [ ] Validate bound identity and supported actions, cap input size, enforce timeout/recovery fences. Preserve native unavailable/refusal responses.
- [ ] Consume queue changed/turn notifications and map lifecycle before enabling queue writes. Keep cancel/approval servicing available during operations.
- [ ] Expose Tauri/Web APIs with identical shapes; tests for unknown operation, unsupported metadata, injected sessionId, malformed/version mismatch, stale turn IDs and pending queue lifecycle.

## 2. Persisted history correctness

Files: new focused Claude rewind-chain module and tests, `src-tauri/src/parsers/claude.rs`; reuse Codex lineage parser and shared fingerprint helper.

- [ ] Read native explicit last-prompt anchor; retain parent chain, empty-history anchor and post-rewind continuation.
- [ ] Refuse cyclic/missing chain rather than show stale suffix; preserve no-anchor behavior and bounded memory.
- [ ] Preserve human user native IDs and complete text used by ACP fingerprints.
- [ ] Test first/historical/latest rewind, duplicate text, thinking/tool-only boundaries and existing parser regressions.

## 3. Typed UI and session controls

Files: new `src/lib/native-session.ts`, API module, hooks and `src/components/conversations/native-session-tools.tsx` plus tests; integrate conversation detail panel and i18n messages.

- [ ] Define `acpNativeCapabilities(connectionId)` and `acpNativeOperation(connectionId,operation,params)` with the exact backend vocabulary.
- [ ] Load capability for current connection only; hide unsupported controls and discard stale read results.
- [ ] Show structured runtime reads, explicit full diagnostic, supported runtime controls, native queues and previewed restore.
- [ ] Codex native queue editor sends native text input with unique clientUserMessageId; uses server pending IDs for update/delete/reorder/start; no duplicate local dispatch.
- [ ] Claude cancel checks native boolean ACK and refreshes; no false success or invented durable queue.
- [ ] Add bilingual labels and useful busy/unavailable/reconnect outcomes.

## 4. Edit integration

Files: message-list edit action/dialog, conversation-detail-panel, native-session helpers/tests, runtime-store reset seam if needed.

- [ ] Resolve persisted selected user turn and previous retained assistant; refuse unproven optimistic IDs.
- [ ] Preserve text/images in draft; call native rewind then reconcile history and normal send exactly once.
- [ ] Keep draft on failure; no fork fallback. File preview/restore remains separate.
- [ ] Test capability absence, duplicate submit, stale selection, failed rewind and failed resend.

## 5. Validation and release

- [ ] Run targeted tests first, then frontend full tests/lint/build and appropriate Rust tests/check/clippy.
- [ ] Exercise full flow with real enhanced adapters against fake providers and isolated homes.
- [ ] Review code for race/identity/queue double-dispatch and fix findings.
- [ ] Build Windows and Linux ARM64 artifacts, verify versions/hashes/launch with isolated data.
- [ ] Back up current installation/database, install Windows locally, verify installed files and launch.
- [ ] Publish source and binary release with bilingual notes/checksums, then verify GitHub assets.
