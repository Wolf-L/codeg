"use client"

import { useRef, useState } from "react"
import { useTranslations } from "next-intl"
import { Button } from "@/components/ui/button"
import {
  nativeExpectedTurn,
  nativeRecord,
  nativeResultData,
  nativeSupports,
  nativeTurnId,
  requireNativeAck,
} from "@/lib/native-session"
import type { MessageTurn } from "@/lib/types"
import type { NativeControlProps } from "./native-queue-controls"
import { NativeDataView } from "./native-data-view"

export function NativeFileRestore({
  caps,
  execute,
  disabled,
  viewer,
  idle,
  turns,
}: NativeControlProps & { turns: MessageTurn[] }) {
  const t = useTranslations("NativeSession")
  const claude = nativeSupports(caps, "rewind_files")
  const codex = nativeSupports(caps, "file_revert")
  const [target, setTarget] = useState("")
  const [selectedTurn, setSelectedTurn] = useState<MessageTurn | null>(null)
  const [preview, setPreview] = useState<{
    target: string
    data: Record<string, unknown>
  } | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [done, setDone] = useState(false)
  const [applyResult, setApplyResult] = useState<Record<
    string,
    unknown
  > | null>(null)
  const [busy, setBusy] = useState(false)
  const lock = useRef(false)
  if (!claude && !codex) return null
  const candidates = claude
    ? turns
        .filter((turn) => turn.role === "user" && nativeTurnId(turn))
        .map((turn) => ({
          id: nativeTurnId(turn)!,
          label: turn.blocks
            .filter((b) => b.type === "text")
            .map((b) => b.text)
            .join(" ")
            .slice(0, 100),
        }))
    : turns.flatMap((turn) =>
        turn.blocks.flatMap((b) =>
          b.type === "tool_use" &&
          b.tool_use_id &&
          ["apply_patch", "fileChange"].includes(b.tool_name) &&
          (b.status === "completed" ||
            turn.blocks.some(
              (r) =>
                r.type === "tool_result" &&
                r.tool_use_id === b.tool_use_id &&
                !r.is_error
            ))
            ? [
                {
                  id: b.tool_use_id,
                  label:
                    `${b.tool_name}: ${b.input_preview ?? b.tool_use_id}`.slice(
                      0,
                      120
                    ),
                },
              ]
            : []
        )
      )
  const eligible = candidates.some((c) => c.id === target)
  const canRestore =
    preview?.target === target &&
    (claude
      ? preview.data.canRewind === true
      : preview.data.canRevert === true &&
        typeof preview.data.previewToken === "string")
  async function run(apply: boolean) {
    if (
      lock.current ||
      !eligible ||
      (claude && !selectedTurn) ||
      (apply && (!canRestore || viewer))
    )
      return
    lock.current = true
    setBusy(true)
    setError(null)
    setDone(false)
    setApplyResult(null)
    const token = preview?.data.previewToken
    setPreview(null)
    try {
      const response = claude
        ? await execute("rewind_files", {
            turnId: target,
            expectedTurn: nativeExpectedTurn(selectedTurn!),
            dryRun: !apply,
          })
        : await execute("file_revert", {
            toolCallId: target,
            dryRun: !apply,
            ...(apply && typeof token === "string"
              ? { previewToken: token }
              : {}),
          })
      const data = nativeRecord(nativeResultData(response))
      if (apply) {
        setApplyResult(data)
        requireNativeAck(response, claude ? "canRewind" : "reverted")
        setDone(true)
      } else setPreview({ target, data })
    } catch (e) {
      setError(String(e))
    } finally {
      lock.current = false
      setBusy(false)
    }
  }
  return (
    <section className="space-y-2 rounded border p-3">
      <h3 className="font-medium">{t("restoreFiles")}</h3>
      <p className="text-xs text-muted-foreground">
        {t(claude ? "checkpointHint" : "fileRevertHint")}
      </p>
      <label className="block">
        {t(claude ? "checkpoint" : "fileTool")}
        <select
          className="w-full rounded border bg-background p-2"
          value={target}
          disabled={busy || disabled}
          onChange={(e) => {
            setTarget(e.target.value)
            setSelectedTurn(
              turns.find((turn) => nativeTurnId(turn) === e.target.value) ??
                null
            )
            setPreview(null)
            setDone(false)
            setApplyResult(null)
          }}
        >
          <option value="">{t("selectTarget")}</option>
          {candidates.map((c, i) => (
            <option key={`${c.id}-${i}`} value={c.id}>
              {c.id} · {c.label}
            </option>
          ))}
        </select>
      </label>
      <Button
        size="sm"
        variant="outline"
        disabled={disabled || busy || !idle || !eligible}
        onClick={() => void run(false)}
      >
        {t("preview")}
      </Button>
      {preview && (
        <>
          <NativeDataView value={preview.data} />
          {!viewer && (
            <>
              <p>{t("restoreConfirm")}</p>
              <Button
                size="sm"
                variant="destructive"
                disabled={disabled || busy || !idle || !canRestore}
                onClick={() => void run(true)}
              >
                {t("confirmRestore")}
              </Button>
            </>
          )}
        </>
      )}
      {error && (
        <p role="alert" className="text-destructive">
          {error} {t("noRetry")}
        </p>
      )}
      {done && (
        <p role="status">
          {t(
            typeof applyResult?.skippedLinks === "number" &&
              applyResult.skippedLinks > 0
              ? "restoredPartial"
              : "restored"
          )}
        </p>
      )}
      {applyResult && <NativeDataView value={applyResult} />}
    </section>
  )
}
