import type { MessageTurn, PromptDraft, PromptInputBlock } from "./types"

/** Sanitized initialize._meta. Never infer support from an agent's name. */
export type NativeCapabilities = Record<string, unknown>
export type NativeQueueInput =
  | { type: "text"; text: string; text_elements?: unknown[] }
  | { type: "image"; url?: string; fileId?: string; detail?: string | null }
  | { type: "localImage"; path: string; detail?: string | null }
  | { type: "audio"; url: string }
  | { type: "localAudio"; path: string }
  | { type: "skill" | "mention"; name: string; path: string }

export interface NativeQueuedSubmission {
  id: string
  input: NativeQueueInput[]
  clientUserMessageId: string
}

export interface NativeExpectedTurn {
  timestamp: MessageTurn["timestamp"]
  agentMessageId: string | null
  blocks: MessageTurn["blocks"]
}
export function nativeExpectedTurn(turn: MessageTurn): NativeExpectedTurn {
  return {
    timestamp: turn.timestamp,
    agentMessageId: turn.agent_message_id ?? null,
    blocks: turn.blocks,
  }
}

export interface NativeOperationParams {
  runtime_read: { resource: string; detail?: "summary" | "full" }
  runtime_control: {
    action: string
    serverName?: string
    enabled?: boolean
    toolUseId?: string
    messageId?: string
    holdOnCacheImpact?: boolean
  }
  rewind: { turnId: string; expectedTurn: NativeExpectedTurn }
  rewind_files: {
    turnId: string
    expectedTurn: NativeExpectedTurn
    dryRun: boolean
  }
  workspace_rewind_files: {
    turnId: string
    expectedTurn: NativeExpectedTurn
    dryRun: boolean
    previewToken?: string
  }
  file_revert: { toolCallId: string; dryRun: boolean; previewToken?: string }
  queue:
    | { action: "list"; cursor?: string; limit?: number }
    | { action: "add"; input: NativeQueueInput[]; clientUserMessageId: string }
    | {
        action: "update"
        queuedSubmissionId: string
        input: NativeQueueInput[]
      }
    | { action: "delete" | "start"; queuedSubmissionId: string }
    | { action: "reorder"; queuedSubmissionIds: string[] }
  mcp_state: Record<string, never>
  mcp_set: { expectedRevision: number; mode: "reloadConfigured" }
  archive: Record<string, never>
  unarchive: Record<string, never>
  search: {
    searchTerm: string
    cursor?: string
    limit?: number
    archived?: boolean
  }
  attachments:
    | { action: "list"; cursor?: string; limit?: number }
    | {
        action: "add"
        attachmentType: string
        identityKey: string
        payload: { url?: string; path?: string; title?: string }
      }
    | { action: "remove"; attachmentType: string; identityKey: string }
  goal: { action: "set" | "pause" | "resume" | "clear"; objective?: string }
}
export type NativeOperation = keyof NativeOperationParams
export type NativeExecute = <O extends NativeOperation>(
  operation: O,
  params: NativeOperationParams[O]
) => Promise<unknown>

export function nativeRecord(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {}
}
export function nativeStrings(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((v): v is string => typeof v === "string")
    : []
}
export function nativeAir(caps: NativeCapabilities) {
  return nativeRecord(nativeRecord(caps.jetbrains).air)
}
function method(cap: unknown, field: string, expected: string) {
  const c = nativeRecord(cap)
  return c.version === 1 && c[field] === expected
}
export function nativeSupports(
  caps: NativeCapabilities,
  operation: NativeOperation,
  action?: string
): boolean {
  const runtime = nativeRecord(caps.runtime)
  switch (operation) {
    case "runtime_read":
      return (
        method(runtime, "readMethod", "_session/runtime/read") &&
        nativeStrings(runtime.reads).includes(action ?? "")
      )
    case "runtime_control":
      return (
        method(runtime, "controlMethod", "_session/runtime/control") &&
        nativeStrings(runtime.controls).includes(action ?? "")
      )
    case "rewind":
      return (
        method(caps.sessionRewind, "method", "_session/rewind") ||
        (nativeAir(caps).version === 1 &&
          nativeStrings(nativeAir(caps).capabilities).includes("sessionRewind"))
      )
    case "rewind_files":
      return (
        method(caps.sessionRewindFiles, "method", "_session/rewind_files") &&
        nativeRecord(caps.sessionRewindFiles).dryRun === true
      )
    case "workspace_rewind_files":
      return (
        method(
          caps.workspaceRewindFiles,
          "method",
          "codeg/workspace/rewind_files"
        ) &&
        nativeRecord(caps.workspaceRewindFiles).dryRun === true &&
        nativeRecord(caps.workspaceRewindFiles).previewTokenRequired === true
      )
    case "file_revert":
      return (
        method(caps.fileRevert, "method", "_session/files/revert") &&
        nativeRecord(caps.fileRevert).dryRun === true &&
        nativeRecord(caps.fileRevert).previewTokenRequired === true
      )
    case "queue":
      return (
        method(caps.queue, "method", "_session/queue") &&
        nativeStrings(nativeRecord(caps.queue).actions).includes(action ?? "")
      )
    case "mcp_state":
      return method(caps.sessionMcp, "stateMethod", "_session/mcp/state")
    case "mcp_set":
      return method(caps.sessionMcp, "setMethod", "_session/mcp/set")
    case "archive":
      return method(caps.archive, "archiveMethod", "_session/archive")
    case "unarchive":
      return method(caps.archive, "unarchiveMethod", "_session/unarchive")
    case "search":
      return method(caps.discovery, "searchMethod", "_session/search")
    case "attachments":
      return (
        method(caps.discovery, "attachmentMethod", "_session/attachments") &&
        nativeStrings(nativeRecord(caps.discovery).attachmentActions).includes(
          action ?? ""
        )
      )
    case "goal":
      return (
        method(nativeAir(caps).goal, "controlMethod", "_session/goal") &&
        nativeStrings(nativeRecord(nativeAir(caps).goal).actions).includes(
          action ?? ""
        )
      )
  }
}
export function nativeIsMutation(
  operation: NativeOperation,
  params: object
): boolean {
  const p = nativeRecord(params)
  return (
    !["runtime_read", "mcp_state", "search"].includes(operation) &&
    !(operation === "queue" && p.action === "list") &&
    !(operation === "attachments" && p.action === "list") &&
    !(
      ["file_revert", "rewind_files", "workspace_rewind_files"].includes(
        operation
      ) && p.dryRun === true
    )
  )
}
export function nativeNeedsIdle(
  operation: NativeOperation,
  params: object
): boolean {
  const p = nativeRecord(params)
  if (operation === "runtime_control")
    return !["backgroundTask", "cancelQueuedMessage"].includes(String(p.action))
  if (operation === "attachments") return p.action !== "list"
  return [
    "goal",
    "rewind",
    "rewind_files",
    "workspace_rewind_files",
    "file_revert",
    "mcp_set",
    "archive",
    "unarchive",
  ].includes(operation)
}
/** Preserve the native refusal, including false ACKs, as an actionable error. */
export function nativeResultData(value: unknown): unknown {
  const result = nativeRecord(value)
  if (
    typeof result.status === "string" &&
    !["ok", "partial"].includes(result.status)
  ) {
    throw new Error(String(result.reason ?? result.status))
  }
  return result.data ?? result.result ?? value
}
export function requireNativeAck(value: unknown, key: string): void {
  const data = nativeRecord(nativeResultData(value))
  if (data[key] !== true)
    throw new Error(String(data.reason ?? `${key}: not confirmed`))
}
/** Positional parser IDs are accepted; optimistic IDs never become guessed targets. */
export function nativeTurnId(turn: MessageTurn): string | null {
  const id = turn.source_turn_id ?? turn.id
  return /^(?:optimistic-|live-|viewer-)/.test(id) || !id ? null : id
}
/** Resolve client-only user turns against a fresh full transcript, never by position. */
export function resolveNativeEditTurn(
  turn: MessageTurn,
  persisted: MessageTurn[]
): MessageTurn {
  if (turn.role !== "user") throw new Error("Only user messages can be edited")
  if (nativeTurnId(turn)) return turn
  const sentAt = Date.parse(turn.timestamp)
  const payload = JSON.stringify(turn.blocks)
  const matches = persisted.filter(
    (candidate) =>
      candidate.role === "user" &&
      nativeTurnId(candidate) &&
      JSON.stringify(candidate.blocks) === payload
  )
  const match = matches.length === 1 ? matches[0] : undefined
  if (
    !match ||
    !Number.isFinite(sentAt) ||
    !(Date.parse(match.timestamp) >= sentAt) ||
    (turn.agent_message_id && turn.agent_message_id !== match.agent_message_id)
  )
    throw new Error("Cannot uniquely identify the saved user message")
  return match
}
export function nativeEditDraft(turn: MessageTurn, text?: string): PromptDraft {
  const original = turn.blocks
    .filter((b) => b.type === "text")
    .map((b) => b.text)
    .join("\n")
  const displayText = text ?? original
  const images: PromptInputBlock[] = turn.blocks
    .filter((b) => b.type === "image")
    .map((b) => ({ ...b }))
  return {
    displayText,
    blocks: [{ type: "text", text: displayText }, ...images],
  }
}
/** Retain non-text native inputs. Text with byte-range annotations needs its native editor. */
export function editableQueueText(input: NativeQueueInput[]): string | null {
  const text = input.filter((i) => i.type === "text")
  if (
    text.length > 1 ||
    text.some((i) => i.type === "text" && (i.text_elements?.length ?? 0) > 0)
  )
    return null
  return text.map((i) => (i.type === "text" ? i.text : "")).join("")
}
export function replaceQueueText(
  input: NativeQueueInput[],
  text: string
): NativeQueueInput[] {
  if (editableQueueText(input) === null)
    throw new Error("Annotated native text cannot be flattened")
  let replaced = false
  const next = input.map((i) => {
    if (i.type !== "text") return i
    replaced = true
    return { ...i, text, text_elements: [] }
  })
  return replaced ? next : [{ type: "text", text, text_elements: [] }, ...next]
}
