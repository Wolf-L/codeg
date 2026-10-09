"use client"

import { useRef, useState } from "react"
import { useTranslations } from "next-intl"
import { Button } from "@/components/ui/button"
import {
  nativeRecord,
  nativeResultData,
  nativeSupports,
  nativeStrings,
  requireNativeAck,
  type NativeOperation,
  type NativeOperationParams,
} from "@/lib/native-session"
import type { MessageTurn } from "@/lib/types"
import { NativeDataView } from "./native-data-view"
import {
  NativeQueueControls,
  type NativeControlProps,
} from "./native-queue-controls"
import { NativeFileRestore } from "./native-file-restore"

const RESOURCES = [
  "context",
  "usage",
  "mcp",
  "commands",
  "agents",
  "plugins",
  "queuedMessages",
] as const
const RELOADS = [
  "reloadSkills",
  "reloadPlugins",
  "reloadOutputStyles",
  "reconnectMcp",
] as const

export function NativeSessionTools(
  props: NativeControlProps & {
    turns: MessageTurn[]
    localQueueCount: number
    queueSnapshot?: Record<string, unknown>
  }
) {
  const { caps, execute, disabled, viewer, idle, turns, localQueueCount } =
    props
  const t = useTranslations("NativeSession")
  const [open, setOpen] = useState(false)
  const [results, setResults] = useState<Record<string, unknown>>({})
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [serverName, setServerName] = useState("")
  const [toolUseId, setToolUseId] = useState("")
  const [objective, setObjective] = useState("")
  const [searchTerm, setSearchTerm] = useState("")
  const [archivedSearch, setArchivedSearch] = useState(false)
  const [attachmentType, setAttachmentType] = useState("pull_request")
  const [attachmentValue, setAttachmentValue] = useState("")
  const [confirmArchive, setConfirmArchive] = useState(false)
  const lock = useRef(false)
  const can = (operation: NativeOperation, action?: string) =>
    nativeSupports(caps, operation, action)
  const resources = RESOURCES.filter((resource) =>
    can("runtime_read", resource)
  )
  const full = nativeStrings(
    nativeRecord(nativeRecord(caps.runtime).context).details
  ).includes("full")
  const perServerMcp = can("runtime_control", "toggleMcp")
  const blocked = disabled || busy
  const writeBlocked = blocked || viewer
  async function read(resource: string, detail?: "full") {
    const response = await execute("runtime_read", {
      resource,
      ...(detail ? { detail } : {}),
    })
    setResults((previous) => ({
      ...previous,
      [resource]: nativeResultData(response),
    }))
  }
  async function run(work: () => Promise<void>) {
    if (lock.current) return
    lock.current = true
    setBusy(true)
    setError(null)
    try {
      await work()
    } catch (e) {
      setError(String(e))
    } finally {
      lock.current = false
      setBusy(false)
    }
  }
  async function perform<O extends NativeOperation>(
    operation: O,
    params: NativeOperationParams[O]
  ) {
    const response = await execute(operation, params)
    const data = nativeResultData(response)
    setResults((previous) => ({ ...previous, [operation]: data }))
    return response
  }
  const supported =
    resources.length ||
    ["queue", "attachments"].some((op) => can(op as NativeOperation, "list")) ||
    [
      "file_revert",
      "rewind_files",
      "search",
      "archive",
      "unarchive",
      "mcp_state",
    ].some((op) => can(op as NativeOperation)) ||
    can("goal", "set")
  if (!supported) return null
  const pendingMessages = nativeRecord(results.queuedMessages).messages
  const attachmentsData = nativeRecord(results.attachments)
  const attachments = Array.isArray(results.attachments)
    ? results.attachments
    : Array.isArray(attachmentsData.data)
      ? attachmentsData.data
      : Array.isArray(attachmentsData.attachments)
        ? attachmentsData.attachments
        : []
  return (
    <div className="shrink-0 border-b px-4 py-2 text-sm">
      <Button
        variant="outline"
        size="sm"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        {t("title")}
      </Button>
      {open && (
        <div className="mt-2 max-h-[55vh] space-y-3 overflow-auto pr-2">
          {viewer && <p>{t("viewer")}</p>}
          {!idle && (
            <p className="text-xs text-muted-foreground">{t("busy")}</p>
          )}
          {error && (
            <p role="alert" className="text-destructive">
              {error} {t("noRetry")}
            </p>
          )}
          {resources.length > 0 && (
            <section className="space-y-2 rounded border p-3">
              <h3 className="font-medium">{t("runtime")}</h3>
              <div className="flex flex-wrap gap-1">
                {resources.map((resource) => (
                  <Button
                    key={resource}
                    variant="outline"
                    size="sm"
                    disabled={blocked}
                    onClick={() => void run(() => read(resource))}
                  >
                    {t(`resources.${resource}`)}
                  </Button>
                ))}
              </div>
              {can("runtime_read", "context") && full && (
                <div>
                  <p className="text-xs text-muted-foreground">
                    {t("fullHint")}
                  </p>
                  <Button
                    variant="outline"
                    size="sm"
                    disabled={blocked}
                    onClick={() => void run(() => read("context", "full"))}
                  >
                    {t("fullContext")}
                  </Button>
                </div>
              )}
              {resources
                .filter((r) => r in results)
                .map((r) => (
                  <details key={r} open>
                    <summary className="cursor-pointer font-medium">
                      {t(`resources.${r}`)}
                    </summary>
                    <NativeDataView value={results[r]} />
                  </details>
                ))}
              {!viewer && (
                <div className="flex flex-wrap gap-1">
                  {RELOADS.filter(
                    (a) =>
                      can("runtime_control", a) &&
                      !(a === "reconnectMcp" && perServerMcp)
                  ).map((action) => (
                    <Button
                      key={action}
                      size="sm"
                      variant="outline"
                      disabled={writeBlocked || !idle}
                      onClick={() =>
                        void run(async () => {
                          await perform("runtime_control", { action })
                          for (const resource of action === "reloadSkills"
                            ? ["commands"]
                            : action === "reloadPlugins"
                              ? ["commands", "plugins", "agents"]
                              : action === "reconnectMcp"
                                ? ["mcp"]
                                : [])
                            if (can("runtime_read", resource))
                              await read(resource)
                        })
                      }
                    >
                      {t(`controls.${action}`)}
                    </Button>
                  ))}
                </div>
              )}
              {!viewer && perServerMcp && (
                <div className="space-y-1">
                  <label>
                    {t("serverName")}
                    <input
                      className="ml-2 rounded border bg-background p-1"
                      value={serverName}
                      onChange={(e) => setServerName(e.target.value)}
                    />
                  </label>
                  <div className="flex flex-wrap gap-1">
                    {can("runtime_control", "reconnectMcp") && (
                      <Button
                        size="sm"
                        variant="outline"
                        disabled={writeBlocked || !idle || !serverName.trim()}
                        onClick={() =>
                          void run(async () => {
                            await perform("runtime_control", {
                              action: "reconnectMcp",
                              serverName,
                            })
                            if (can("runtime_read", "mcp")) await read("mcp")
                          })
                        }
                      >
                        {t("controls.reconnectMcp")}
                      </Button>
                    )}
                    {[true, false].map((enabled) => (
                      <Button
                        key={String(enabled)}
                        size="sm"
                        variant="outline"
                        disabled={writeBlocked || !idle || !serverName.trim()}
                        onClick={() =>
                          void run(async () => {
                            await perform("runtime_control", {
                              action: "toggleMcp",
                              serverName,
                              enabled,
                            })
                            if (can("runtime_read", "mcp")) await read("mcp")
                          })
                        }
                      >
                        {t(enabled ? "enable" : "disable")}
                      </Button>
                    ))}
                  </div>
                </div>
              )}
              {!viewer && can("runtime_control", "backgroundTask") && (
                <div>
                  <label>
                    {t("toolId")}
                    <input
                      className="ml-2 rounded border bg-background p-1"
                      value={toolUseId}
                      onChange={(e) => setToolUseId(e.target.value)}
                    />
                  </label>
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={writeBlocked || !toolUseId.trim()}
                    onClick={() =>
                      void run(async () => {
                        await perform("runtime_control", {
                          action: "backgroundTask",
                          toolUseId,
                        })
                      })
                    }
                  >
                    {t("backgroundTask")}
                  </Button>
                </div>
              )}
              {!viewer &&
                can("runtime_control", "cancelQueuedMessage") &&
                Array.isArray(pendingMessages) &&
                pendingMessages
                  .map((m) => nativeRecord(m))
                  .filter((m) => typeof m.messageId === "string")
                  .map((m) => (
                    <div
                      key={String(m.messageId)}
                      className="flex items-center gap-2"
                    >
                      <span>{String(m.messageId)}</span>
                      <Button
                        size="sm"
                        variant="outline"
                        disabled={writeBlocked}
                        onClick={() =>
                          void run(async () => {
                            const response = await execute("runtime_control", {
                              action: "cancelQueuedMessage",
                              messageId: String(m.messageId),
                            })
                            requireNativeAck(response, "cancelled")
                            await read("queuedMessages")
                          })
                        }
                      >
                        {t("cancelPending")}
                      </Button>
                    </div>
                  ))}
              {"runtime_control" in results && (
                <NativeDataView value={results.runtime_control} />
              )}
            </section>
          )}
          <NativeQueueControls
            {...props}
            disabled={blocked}
            localQueueCount={localQueueCount}
          />
          <NativeFileRestore {...props} disabled={blocked} turns={turns} />
          {can("mcp_state") && (
            <section className="space-y-2 rounded border p-3">
              <h3>{t("mcpOwnership")}</h3>
              <Button
                size="sm"
                variant="outline"
                disabled={blocked}
                onClick={() =>
                  void run(async () => {
                    await perform("mcp_state", {})
                  })
                }
              >
                {t("refresh")}
              </Button>
              {"mcp_state" in results && (
                <NativeDataView value={results.mcp_state} />
              )}
              {!viewer && can("mcp_set") && (
                <Button
                  size="sm"
                  variant="outline"
                  disabled={writeBlocked || !idle}
                  onClick={() =>
                    void run(async () => {
                      const state = nativeRecord(
                        nativeResultData(await execute("mcp_state", {}))
                      )
                      if (
                        state.uncertain === true ||
                        !Number.isSafeInteger(state.revision) ||
                        Number(state.revision) < 0
                      )
                        throw new Error(t("invalidResult"))
                      const result = await perform("mcp_set", {
                        expectedRevision: Number(state.revision),
                        mode: "reloadConfigured",
                      })
                      if (
                        !["ok", "partial"].includes(
                          String(nativeRecord(result).status)
                        )
                      )
                        throw new Error(t("invalidResult"))
                      await perform("mcp_state", {})
                      if (can("runtime_read", "mcp")) await read("mcp")
                    })
                  }
                >
                  {t("applyConfiguredMcp")}
                </Button>
              )}
              {"mcp_set" in results && (
                <NativeDataView value={results.mcp_set} />
              )}
              <p className="text-xs text-muted-foreground">
                {t("mcpOwnershipHint")}
              </p>
            </section>
          )}
          {["set", "pause", "resume", "clear"].some((a) => can("goal", a)) &&
            !viewer && (
              <section className="space-y-2 rounded border p-3">
                <h3>{t("goal")}</h3>
                {can("goal", "set") && (
                  <label className="block">
                    {t("objective")}
                    <textarea
                      className="w-full rounded border bg-background p-2"
                      value={objective}
                      onChange={(e) => setObjective(e.target.value)}
                    />
                  </label>
                )}
                <div className="flex flex-wrap gap-1">
                  {(["set", "pause", "resume", "clear"] as const)
                    .filter((a) => can("goal", a))
                    .map((action) => (
                      <Button
                        key={action}
                        size="sm"
                        variant="outline"
                        disabled={
                          writeBlocked ||
                          !idle ||
                          (action === "set" && !objective.trim())
                        }
                        onClick={() =>
                          void run(async () => {
                            await perform("goal", {
                              action,
                              ...(action === "set" ? { objective } : {}),
                            })
                          })
                        }
                      >
                        {t(`goalActions.${action}`)}
                      </Button>
                    ))}
                </div>
                {"goal" in results && <NativeDataView value={results.goal} />}
              </section>
            )}
          {can("search") && (
            <section className="space-y-2 rounded border p-3">
              <h3>{t("search")}</h3>
              <label>
                {t("searchTerm")}
                <input
                  className="ml-2 rounded border bg-background p-1"
                  value={searchTerm}
                  onChange={(e) => setSearchTerm(e.target.value)}
                />
              </label>
              <label className="ml-2">
                <input
                  type="checkbox"
                  checked={archivedSearch}
                  onChange={(e) => setArchivedSearch(e.target.checked)}
                />{" "}
                {t("archived")}
              </label>
              <Button
                size="sm"
                variant="outline"
                disabled={blocked || !searchTerm.trim()}
                onClick={() =>
                  void run(async () => {
                    await perform("search", {
                      searchTerm,
                      archived: archivedSearch,
                      limit: 50,
                    })
                  })
                }
              >
                {t("search")}
              </Button>
              {"search" in results && <NativeDataView value={results.search} />}
            </section>
          )}
          {can("attachments", "list") && (
            <section className="space-y-2 rounded border p-3">
              <h3>{t("attachments")}</h3>
              <p className="text-xs text-muted-foreground">
                {t("attachmentsHint")}
              </p>
              <Button
                size="sm"
                variant="outline"
                disabled={blocked}
                onClick={() =>
                  void run(async () => {
                    await perform("attachments", { action: "list", limit: 100 })
                  })
                }
              >
                {t("refresh")}
              </Button>
              {"attachments" in results && (
                <NativeDataView value={results.attachments} />
              )}
              {!viewer && can("attachments", "add") && (
                <div className="space-y-1">
                  <select
                    aria-label={t("attachmentType")}
                    className="rounded border bg-background p-1"
                    value={attachmentType}
                    onChange={(e) => setAttachmentType(e.target.value)}
                  >
                    <option value="pull_request">{t("pullRequest")}</option>
                    <option value="worktree">{t("worktree")}</option>
                  </select>
                  <label className="block">
                    {t("attachmentValue")}
                    <input
                      className="w-full rounded border bg-background p-1"
                      value={attachmentValue}
                      onChange={(e) => setAttachmentValue(e.target.value)}
                    />
                  </label>
                  <Button
                    size="sm"
                    disabled={writeBlocked || !idle || !attachmentValue.trim()}
                    onClick={() =>
                      void run(async () => {
                        await perform("attachments", {
                          action: "add",
                          attachmentType,
                          identityKey: attachmentValue,
                          payload:
                            attachmentType === "pull_request"
                              ? { url: attachmentValue }
                              : { path: attachmentValue },
                        })
                        setAttachmentValue("")
                        await perform("attachments", {
                          action: "list",
                          limit: 100,
                        })
                      })
                    }
                  >
                    {t("attach")}
                  </Button>
                </div>
              )}
              {!viewer &&
                can("attachments", "remove") &&
                attachments
                  .map((a) => nativeRecord(a))
                  .filter(
                    (a) =>
                      typeof a.identityKey === "string" &&
                      typeof a.attachmentType === "string"
                  )
                  .map((a) => (
                    <Button
                      key={`${a.attachmentType}:${a.identityKey}`}
                      size="sm"
                      variant="outline"
                      disabled={writeBlocked || !idle}
                      onClick={() =>
                        void run(async () => {
                          await perform("attachments", {
                            action: "remove",
                            attachmentType: String(a.attachmentType),
                            identityKey: String(a.identityKey),
                          })
                          await perform("attachments", {
                            action: "list",
                            limit: 100,
                          })
                        })
                      }
                    >
                      {t("unlink")} {String(a.identityKey)}
                    </Button>
                  ))}
            </section>
          )}
          {!viewer && (can("archive") || can("unarchive")) && (
            <section className="space-y-2 rounded border p-3">
              <p>{t("archiveHint")}</p>
              {can("archive") && (
                <Button
                  size="sm"
                  variant="outline"
                  disabled={writeBlocked || !idle}
                  onClick={() => setConfirmArchive(true)}
                >
                  {t("archive")}
                </Button>
              )}
              {confirmArchive && (
                <div>
                  <p>{t("archiveConfirm")}</p>
                  <Button
                    size="sm"
                    variant="destructive"
                    disabled={writeBlocked || !idle}
                    onClick={() =>
                      void run(async () => {
                        await perform("archive", {})
                        setConfirmArchive(false)
                      })
                    }
                  >
                    {t("confirmArchive")}
                  </Button>
                </div>
              )}
              {can("unarchive") && (
                <Button
                  size="sm"
                  variant="outline"
                  disabled={writeBlocked || !idle}
                  onClick={() =>
                    void run(async () => {
                      await perform("unarchive", {})
                    })
                  }
                >
                  {t("unarchive")}
                </Button>
              )}
              {"archive" in results && (
                <NativeDataView value={results.archive} />
              )}
              {"unarchive" in results && (
                <NativeDataView value={results.unarchive} />
              )}
            </section>
          )}
        </div>
      )}
    </div>
  )
}
