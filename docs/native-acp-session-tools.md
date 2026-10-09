# Native ACP session tools

Community build based on official Codeg v0.34.0. This build consumes optional
extensions from the full enhanced Codex and Claude ACP releases. It keeps the
standard ACP path for other adapters and does not assume capability from a
package version.

## Install the adapters

Download the matching Windows x64 or Linux ARM64 portable adapter from:

- https://github.com/Wolf-L/codex-acp/releases/tag/full-enhancements-20261009
- https://github.com/Wolf-L/claude-agent-acp/releases/tag/full-enhancements-20261009-r2

Verify the accompanying SHA256 and keep each extracted directory intact. Windows
launchers are at the package root; Linux launchers are under `bin`. Make those
launchers available to Codeg's process PATH as `codex-acp` and `claude-agent-acp`,
or use an existing global installation of these full builds. Start a new ACP
connection after replacement. Do not run the official adapter update action to
install the custom build: the official registry still points at upstream releases.
Credentials/provider settings remain your existing configuration. ACP packages
include Node; Git and platform system libraries remain OS prerequisites.

## Edit a previous user message

The Edit action is available on eligible persisted messages when the current
adapter advertises same-session rewind. Finish or stop active work and resolve
pending queues first. Confirming rewinds the native history in the same session,
refreshes the transcript and places the edited text/images back in the composer.
Review and send normally. It does not create an edit fork or send automatically.

The host compares the selected message with a fresh parse before dispatch and
uses native identity/fingerprints. A stale, ambiguous or unsupported target is
refused. Codex raw text is read through the current native rollout lineage so
whitespace is not hashed from a normalized display. Repeated text and attachments
without a provable native message ID may require a different target; the host
never guesses an ID. A lost acknowledgement requires reconnecting and inspecting
history, not retrying the mutation.

## Native session tools

- Context summary, usage, MCP, commands, agents and plugins appear only when
  advertised. Claude full context is an explicit action and may call the configured
  provider's token-count endpoint.
- Codex native queue supports add/edit/delete/reorder/start. Native storage and
  dispatch own these entries; they are distinct from Codeg's local composer queue.
  Adding to an idle native thread may start immediately. Cancel preserves pending
  entries; explicitly start to resume. The panel displays the first 100 entries
  and disables reorder for partial lists. Annotated native text is not flattened.
- Claude's pending list exposes IDs and cancellation of one pending prompt. It
  is not a durable queue; a false cancellation reply is shown as a refusal.
- File restore is separate from history rewind. Claude requires an existing
  checkpoint; new and reopened Claude connections enable checkpoint capture for
  future file-tool changes. This cannot reconstruct checkpoints for earlier changes.
  Codex reverses one recorded text patch tool in Git, not a whole
  workspace. Preview first, confirm explicitly, and inspect actual affected and
  skipped results. No cross-history/file transaction or automatic retry exists.
- Supported refresh/reconnect/toggle/background controls are available. Applying
  saved MCP configuration preserves Codeg's injected companion entries and uses
  the adapter's revision/ownership checks; arbitrary request payloads are not exposed.
- Native archive/unarchive, search, attachment metadata and goal controls are
  gated by their advertised actions. Native archive is distinct from local hide
  and permanent deletion. Unarchive requires reconnecting to reopen native routing.

## Delivery and verification

Windows and Linux ARM64 builds are community prereleases, not official Codeg
updates. Their version includes `+native.acp.20261009`. They preserve the official
update trust configuration and do not publish official updater signatures; a later
official upgrade can replace these custom features. Release notes identify final
commits, checksums, platform verification and any outstanding test limitations.

Automated native tests use real installed adapters/CLIs with isolated directories
and loopback model fixtures. No live account or existing user session is needed.
ARM64 execution under QEMU is distinct from physical hardware testing. The build
does not claim all proprietary Desktop features or cloud services are implemented.
