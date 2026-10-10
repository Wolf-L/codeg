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

The Edit icon shares the hover action row with Copy and Create task when the
current adapter advertises same-session rewind. It remains visible in that row
during generation but is disabled until work stops and pending queues clear.
It becomes usable in the same view without switching tabs or refreshing. Confirming rewinds the native history in the same session,
refreshes the transcript and places the edited text/images back in the composer.
Review and send normally. It does not create an edit fork or send automatically.
Messages sent in the current connection can be edited without reopening the
conversation. A client-only message is matched to a fresh full transcript by its
exact content and send time before dispatch; missing or repeated matches are refused.

The host compares the selected message with a fresh parse before dispatch and
uses native identity/fingerprints. A stale, ambiguous or unsupported target is
refused. Codex raw text is read through the current native rollout lineage so
whitespace is not hashed from a normalized display. Repeated text and attachments
without a provable native message ID may require a different target; the host
never guesses an ID. A lost acknowledgement requires reconnecting and inspecting
history, not retrying the mutation.

## Restore files while editing

The edit dialog defaults to **Messages and files** when Codeg advertises workspace
checkpoints. Opening the dialog automatically previews the affected paths;
confirm when the preview is ready. Switching back to Messages and files refreshes
the preview. A failed preview can be retried explicitly. Files are restored before
native history is rewound. **Messages only** explicitly keeps the current files.
Codeg captures regular file bytes before and after completed host prompts, so
command-line writes in a non-Git directory are covered too. Snapshots stay local.

Only captured turns can restore files. Old turns, canceled/failed turns, attachment
identities that cannot be proven, background/queued work and incomplete captures
are refused. Manual changes between turns or conflicting later changes prevent a
restore. Unrelated current files are retained. Capture is limited to 5,000 files,
10,000 entries, 8 MiB per file and 64 MiB total. At every depth, `.git`,
`node_modules`, `target`, `.next` and `buildcache` are excluded. Links, hard links,
special files and unreadable files make coverage incomplete. Files outside the
working directory, ACLs, timestamps, Git index/HEAD and empty-directory
removal are not restored. Unix file permission bits are captured and restored. A restore spanning more than 256 prompts is refused. Each session has a 1 GiB checkpoint-storage ceiling; new captures stop before exhausting it, retaining existing evidence. There is no automatic deletion or global quota across sessions. Moving a newly created file to its backup currently requires the checkpoint store and workspace to be on the same filesystem; otherwise restoration fails and rolls back.

Restoration keeps a local journal and backups; it is not an atomic filesystem
snapshot. If application fails, Codeg attempts to undo its own writes and reports
an uncertain outcome when that cannot be verified. Do not interpret a history-only
rewind as evidence that files were restored. Existing messages cannot acquire a
historical checkpoint retroactively. A durable recovery gate prevents new prompts in overlapping workspaces between file restoration and the matching history rewind. Completed restoration is rechecked against current file contents when recovering after reconnection. Concurrent or canceled work without a confirmed settled outcome invalidates checkpoint coverage.

If a canceled request loses its final response, Codeg keeps its writer fence:
reconnecting alone does not prove that tools stopped. File/history restoration
stays unavailable in overlapping directories until the outcome is confirmed.
The in-memory fence resets when Codeg restarts; confirm that detached tools have
stopped before restarting. Durable restore journals remain across restarts.

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
- The separate native file-tool panel remains available: Claude uses its native
  checkpoint; Codex reverses one recorded text-patch tool in Git. These controls
  do not rewind history and have narrower coverage than the edit dialog's host
  workspace checkpoints. Preview first and inspect affected/skipped results.
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
