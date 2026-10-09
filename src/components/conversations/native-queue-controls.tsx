"use client"

import { useRef, useState } from "react"
import { useTranslations } from "next-intl"
import { Button } from "@/components/ui/button"
import {
  editableQueueText,
  nativeRecord,
  nativeResultData,
  nativeSupports,
  replaceQueueText,
  requireNativeAck,
  type NativeCapabilities,
  type NativeExecute,
  type NativeQueuedSubmission,
} from "@/lib/native-session"
import { randomUUID } from "@/lib/utils"
import { NativeDataView } from "./native-data-view"

export interface NativeControlProps {
  caps: NativeCapabilities
  execute: NativeExecute
  disabled: boolean
  viewer: boolean
  idle: boolean
}

export function NativeQueueControls({
  caps,
  execute,
  disabled,
  viewer,
  localQueueCount,
  queueSnapshot,
}: NativeControlProps & {
  localQueueCount: number
  queueSnapshot?: Record<string, unknown>
}) {
  const t = useTranslations("NativeSession")
  const [localItems, setItems] = useState<NativeQueuedSubmission[]>([])
  const [loaded, setLoaded] = useState(false)
  const [complete, setComplete] = useState(false)
  const [text, setText] = useState("")
  const [editing, setEditing] = useState<NativeQueuedSubmission | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const lock = useRef(false)
  const items =
    queueSnapshot && Array.isArray(queueSnapshot.data)
      ? (queueSnapshot.data as NativeQueuedSubmission[])
      : localItems
  const can = (action: string) => nativeSupports(caps, "queue", action)
  async function list() {
    const data = nativeRecord(
      nativeResultData(await execute("queue", { action: "list", limit: 100 }))
    )
    if (!Array.isArray(data.data)) throw new Error(t("invalidResult"))
    setItems(
      data.data.filter(
        (item): item is NativeQueuedSubmission =>
          typeof nativeRecord(item).id === "string" &&
          Array.isArray(nativeRecord(item).input)
      )
    )
    setComplete(data.nextCursor == null)
    setLoaded(true)
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
  const blocked = disabled || busy
  const writeBlocked = blocked || viewer
  if (!can("list")) return null
  return (
    <section className="space-y-2 rounded border p-3">
      <h3 className="font-medium">{t("nativeQueue")}</h3>
      <p className="text-xs text-muted-foreground">{t("nativeQueueHint")}</p>
      <p className="text-xs">
        {t("localQueueCount", { count: localQueueCount })}
      </p>
      <Button
        size="sm"
        variant="outline"
        disabled={blocked}
        onClick={() => void run(list)}
      >
        {t("refresh")}
      </Button>
      {error && (
        <p role="alert" className="text-destructive">
          {error} {t("noRetry")}
        </p>
      )}
      {loaded && !items.length && <p>{t("empty")}</p>}
      {loaded && !complete && <p>{t("queuePartial")}</p>}
      <ol className="space-y-2">
        {items.map((item, index) => (
          <li key={item.id} className="rounded border p-2">
            <p className="whitespace-pre-wrap break-all">
              {item.input
                .filter((i) => i.type === "text")
                .map((i) => (i.type === "text" ? i.text : ""))
                .join("\n") || item.id}
            </p>
            <details>
              <summary>{t("details")}</summary>
              <NativeDataView value={item} />
            </details>
            {!viewer && (
              <div className="flex flex-wrap gap-1">
                {can("update") && (
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={
                      writeBlocked || editableQueueText(item.input) === null
                    }
                    onClick={() => {
                      setEditing(item)
                      setText(editableQueueText(item.input) ?? "")
                    }}
                  >
                    {t("edit")}
                  </Button>
                )}
                {can("delete") && (
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={writeBlocked}
                    onClick={() =>
                      void run(async () => {
                        requireNativeAck(
                          await execute("queue", {
                            action: "delete",
                            queuedSubmissionId: item.id,
                          }),
                          "deleted"
                        )
                        await list()
                      })
                    }
                  >
                    {t("delete")}
                  </Button>
                )}
                {can("start") && (
                  <Button
                    size="sm"
                    disabled={writeBlocked || localQueueCount > 0}
                    onClick={() =>
                      void run(async () => {
                        const result = nativeRecord(
                          nativeResultData(
                            await execute("queue", {
                              action: "start",
                              queuedSubmissionId: item.id,
                            })
                          )
                        )
                        if (!result.turn) throw new Error(t("invalidResult"))
                        await list()
                      })
                    }
                  >
                    {t("start")}
                  </Button>
                )}
                {can("reorder") &&
                  [-1, 1].map((direction) => (
                    <Button
                      key={direction}
                      size="sm"
                      variant="outline"
                      aria-label={t(direction < 0 ? "moveUp" : "moveDown")}
                      disabled={
                        writeBlocked ||
                        !(queueSnapshot
                          ? queueSnapshot.nextCursor == null
                          : complete) ||
                        index + direction < 0 ||
                        index + direction >= items.length
                      }
                      onClick={() =>
                        void run(async () => {
                          const ids = items.map((i) => i.id)
                          ;[ids[index], ids[index + direction]] = [
                            ids[index + direction],
                            ids[index],
                          ]
                          nativeResultData(
                            await execute("queue", {
                              action: "reorder",
                              queuedSubmissionIds: ids,
                            })
                          )
                          await list()
                        })
                      }
                    >
                      {direction < 0 ? "↑" : "↓"}
                    </Button>
                  ))}
              </div>
            )}
          </li>
        ))}
      </ol>
      {!viewer && (can("add") || editing) && (
        <div className="space-y-2">
          <label className="block">
            {t("queueDraft")}
            <textarea
              className="mt-1 w-full rounded border bg-background p-2"
              value={text}
              onChange={(e) => setText(e.target.value)}
              disabled={blocked}
            />
          </label>
          <Button
            size="sm"
            disabled={
              writeBlocked || !text.trim() || (!editing && localQueueCount > 0)
            }
            onClick={() =>
              void run(async () => {
                const result = nativeRecord(
                  nativeResultData(
                    editing
                      ? await execute("queue", {
                          action: "update",
                          queuedSubmissionId: editing.id,
                          input: replaceQueueText(editing.input, text),
                        })
                      : await execute("queue", {
                          action: "add",
                          input: [{ type: "text", text, text_elements: [] }],
                          clientUserMessageId: randomUUID(),
                        })
                  )
                )
                if (!result.queuedSubmission)
                  throw new Error(t("invalidResult"))
                setEditing(null)
                setText("")
                await list()
              })
            }
          >
            {t(editing ? "save" : "addQueue")}
          </Button>
          {editing && (
            <Button
              size="sm"
              variant="ghost"
              disabled={blocked}
              onClick={() => {
                setEditing(null)
                setText("")
              }}
            >
              {t("cancel")}
            </Button>
          )}
        </div>
      )}
    </section>
  )
}
