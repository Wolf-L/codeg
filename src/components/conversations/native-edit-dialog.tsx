"use client"

import { useRef, useState } from "react"
import { useTranslations } from "next-intl"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import {
  nativeEditDraft,
  nativeExpectedTurn,
  nativeRecord,
  nativeResultData,
  nativeSupports,
  nativeTurnId,
  requireNativeAck,
  type NativeExecute,
  type NativeCapabilities,
} from "@/lib/native-session"
import type { MessageTurn, PromptDraft } from "@/lib/types"

export function NativeEditDialog({
  turn,
  caps = {},
  execute,
  disabled,
  resolveTurn,
  onReconcile,
  onDraft,
  onClose,
}: {
  turn: MessageTurn
  caps?: NativeCapabilities
  execute: NativeExecute
  disabled: boolean
  resolveTurn: () => Promise<MessageTurn>
  onReconcile: (rewoundTurn: MessageTurn) => Promise<void>
  onDraft: (draft: PromptDraft) => void
  onClose: () => void
}) {
  const t = useTranslations("NativeSession")
  const [text, setText] = useState(() => nativeEditDraft(turn).displayText)
  const [error, setError] = useState<string | null>(null)
  const [rewound, setRewound] = useState(false)
  const [busy, setBusy] = useState(false)
  const lock = useRef(false)
  const rewoundTurn = useRef<MessageTurn | null>(null)
  const supportsFiles = nativeSupports(caps, "workspace_rewind_files")
  const [restoreFiles, setRestoreFiles] = useState(supportsFiles)
  const [preview, setPreview] = useState<{
    target: MessageTurn
    data: Record<string, unknown>
  } | null>(null)
  const [fileResult, setFileResult] = useState<Record<string, unknown> | null>(
    null
  )
  const [fileOutcomeUnknown, setFileOutcomeUnknown] = useState(false)
  const restoredTurn = useRef<MessageTurn | null>(null)
  const draft = nativeEditDraft(turn, text)
  async function previewFiles() {
    if (lock.current || disabled || !supportsFiles) return
    lock.current = true
    setBusy(true)
    setError(null)
    setPreview(null)
    try {
      const target = await resolveTurn()
      const turnId = nativeTurnId(target)
      if (!turnId) throw new Error("User message is not saved yet")
      const data = nativeRecord(
        nativeResultData(
          await execute("workspace_rewind_files", {
            turnId,
            expectedTurn: nativeExpectedTurn(target),
            dryRun: true,
          })
        )
      )
      setPreview({ target, data })
    } catch (e) {
      setError(String(e))
    } finally {
      lock.current = false
      setBusy(false)
    }
  }
  async function confirm() {
    if (lock.current || disabled || fileOutcomeUnknown) return
    lock.current = true
    setBusy(true)
    setError(null)
    try {
      if (!rewound) {
        const target =
          restoredTurn.current ??
          (restoreFiles ? preview?.target : await resolveTurn())
        if (!target) throw new Error("Preview the file changes first")
        const turnId = nativeTurnId(target)
        if (!turnId) throw new Error("User message is not saved yet")
        if (restoreFiles && !restoredTurn.current) {
          if (
            preview?.data.canRevert !== true ||
            typeof preview.data.previewToken !== "string"
          )
            throw new Error("No verified file restore preview")
          // Conservatively block a second apply when the transport outcome is
          // unknown. A definitive refusal can be previewed again.
          setFileOutcomeUnknown(true)
          const response = await execute("workspace_rewind_files", {
            turnId,
            expectedTurn: nativeExpectedTurn(target),
            dryRun: false,
            previewToken: preview.data.previewToken,
          })
          const data = nativeRecord(nativeResultData(response))
          setFileResult(data)
          if (
            data.uncertain === true ||
            String(data.reason ?? "").includes("outcome_unknown")
          )
            throw new Error(
              "File restore outcome is unknown; inspect files before continuing"
            )
          if (
            data.uncertain !== true &&
            !String(data.reason ?? "").includes("outcome_unknown")
          )
            setFileOutcomeUnknown(false)
          if (data.reverted !== true) setPreview(null)
          requireNativeAck(response, "reverted")
          restoredTurn.current = target
        }
        requireNativeAck(
          await execute("rewind", {
            turnId,
            expectedTurn: nativeExpectedTurn(target),
          }),
          "rewound"
        )
        rewoundTurn.current = target
        setRewound(true)
      }
      if (!rewoundTurn.current) throw new Error("Missing rewind target")
      await onReconcile(rewoundTurn.current)
      onDraft(draft)
      onClose()
    } catch (e) {
      setError(String(e))
    } finally {
      lock.current = false
      setBusy(false)
    }
  }
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !busy) onClose()
      }}
    >
      <DialogContent showCloseButton={!busy}>
        <DialogHeader>
          <DialogTitle>{t("editTitle")}</DialogTitle>
          <DialogDescription>{t("editHint")}</DialogDescription>
        </DialogHeader>
        <fieldset
          disabled={
            busy || rewound || !!restoredTurn.current || fileOutcomeUnknown
          }
          className="space-y-2"
        >
          <legend>{t("rewindScope")}</legend>
          <label className="flex items-center gap-2">
            <input
              type="radio"
              name="rewind-scope"
              checked={!restoreFiles}
              onChange={() => {
                setRestoreFiles(false)
                setPreview(null)
              }}
            />
            {t("historyOnly")}
          </label>
          <label className="flex items-center gap-2">
            <input
              type="radio"
              name="rewind-scope"
              checked={restoreFiles}
              disabled={!supportsFiles}
              onChange={() => setRestoreFiles(true)}
            />
            {t("historyAndFiles")}
          </label>
        </fieldset>
        {restoreFiles && !rewound && !restoredTurn.current && (
          <div className="space-y-2">
            <p className="text-xs text-muted-foreground">
              {t("workspaceCheckpointHint")}
            </p>
            <Button
              variant="outline"
              disabled={disabled || busy || fileOutcomeUnknown}
              onClick={() => void previewFiles()}
            >
              {t("preview")}
            </Button>
            {preview && (
              <div className="space-y-1 text-sm" role="status">
                <p>
                  {t(
                    preview.data.canRevert === true
                      ? "filePreviewReady"
                      : "filePreviewUnavailable"
                  )}
                </p>
                {Array.isArray(preview.data.paths) && (
                  <ul className="max-h-40 list-disc overflow-y-auto pl-5">
                    {preview.data.paths
                      .filter(
                        (path): path is string => typeof path === "string"
                      )
                      .map((path) => (
                        <li key={path}>{path}</li>
                      ))}
                  </ul>
                )}
                {typeof preview.data.reason === "string" && (
                  <p>{preview.data.reason}</p>
                )}
              </div>
            )}
          </div>
        )}
        {!restoreFiles && <p className="text-sm">{t("historyOnlyHint")}</p>}
        {fileResult && (
          <p role="status">
            {t(
              fileResult.reverted === true
                ? "workspaceFilesRestored"
                : "workspaceFilesFailed"
            )}
          </p>
        )}
        {restoredTurn.current && !rewound && (
          <p role="status">{t("filesBeforeHistory")}</p>
        )}
        <label>
          {t("draft")}
          <textarea
            className="mt-1 min-h-32 w-full rounded border bg-background p-2"
            value={text}
            disabled={busy}
            onChange={(e) => setText(e.target.value)}
          />
        </label>
        {draft.blocks
          .filter((b) => b.type === "image")
          .map((b, index) => (
            <p key={index} className="text-xs">
              {t("imagePreserved", { index: index + 1 })} (
              {b.type === "image" ? b.mime_type : ""})
            </p>
          ))}
        {error && (
          <p role="alert" className="text-destructive">
            {error} {t("draftKept")} {t("noRetry")}
          </p>
        )}
        {rewound && <p>{t("rewoundHint")}</p>}
        <div className="flex flex-wrap gap-2">
          <Button
            disabled={
              disabled ||
              busy ||
              fileOutcomeUnknown ||
              (restoreFiles &&
                !restoredTurn.current &&
                !rewound &&
                (preview?.data.canRevert !== true ||
                  typeof preview.data.previewToken !== "string")) ||
              (!text.trim() && draft.blocks.length === 1)
            }
            onClick={() => void confirm()}
          >
            {t(
              rewound
                ? "reconcile"
                : restoreFiles && !restoredTurn.current
                  ? "confirmFilesAndHistory"
                  : "confirmEdit"
            )}
          </Button>
          <Button
            variant="outline"
            disabled={busy}
            onClick={() => {
              onDraft(draft)
              onClose()
            }}
          >
            {t("keepDraft")}
          </Button>
          <Button variant="ghost" disabled={busy} onClick={onClose}>
            {t("cancel")}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  )
}
