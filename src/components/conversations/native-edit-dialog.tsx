"use client"

import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react"
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
  const scope = useMemo(
    () => ({ disabled, restoreFiles, supportsFiles, turnId: turn.id }),
    [disabled, restoreFiles, supportsFiles, turn.id]
  )
  const [preview, setPreview] = useState<{
    scope: typeof scope
    target: MessageTurn
    data: Record<string, unknown>
  } | null>(null)
  const currentPreview = preview?.scope === scope ? preview : null
  const [previewing, setPreviewing] = useState<typeof scope | null>(null)
  const loadingPreview = previewing === scope
  const [fileResult, setFileResult] = useState<Record<string, unknown> | null>(
    null
  )
  const [fileOutcomeUnknown, setFileOutcomeUnknown] = useState(false)
  const outcomeUnknown = useRef(false)
  const restoredTurn = useRef<MessageTurn | null>(null)
  const draft = nativeEditDraft(turn, text)
  const mounted = useRef(false)
  const closed = useRef(false)
  const generation = useRef(0)
  const previewRequest = useRef(0)
  const previewTask = useRef<Promise<void> | null>(null)
  const latest = useRef({ scope, execute, resolveTurn })
  useLayoutEffect(() => {
    latest.current = { scope, execute, resolveTurn }
  }, [scope, execute, resolveTurn])
  useLayoutEffect(() => {
    mounted.current = true
    return () => {
      mounted.current = false
      generation.current += 1
    }
  }, [scope])

  const previewFiles = useCallback(() => {
    const { scope } = latest.current
    if (
      !mounted.current ||
      closed.current ||
      lock.current ||
      outcomeUnknown.current ||
      restoredTurn.current ||
      rewoundTurn.current ||
      scope.disabled ||
      !scope.restoreFiles ||
      !scope.supportsFiles
    )
      return
    const request = ++previewRequest.current
    const version = generation.current
    const isCurrent = () =>
      mounted.current &&
      !closed.current &&
      generation.current === version &&
      previewRequest.current === request
    setPreviewing(scope)
    setError(null)
    setPreview(null)
    // A superseded read may still occupy the native hook. Wait for it before
    // refreshing, but never let it publish into the new scope.
    const previous = previewTask.current
    previewTask.current = (async () => {
      try {
        await previous
        if (!isCurrent()) return
        const { execute, resolveTurn } = latest.current
        const target = await resolveTurn()
        if (!isCurrent()) return
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
        if (!isCurrent()) return
        setPreview({ scope, target, data })
      } catch (e) {
        if (isCurrent()) setError(String(e))
      } finally {
        if (isCurrent()) setPreviewing(null)
      }
    })()
  }, [])

  useEffect(() => {
    let cancelled = false
    // Defer past StrictMode's setup/cleanup replay. Callback identity and
    // preview completion are deliberately not triggers for another preview.
    void Promise.resolve().then(() => {
      if (!cancelled) previewFiles()
    })
    return () => {
      cancelled = true
    }
  }, [scope, previewFiles])

  function close() {
    closed.current = true
    generation.current += 1
    onClose()
  }
  async function confirm() {
    if (
      lock.current ||
      disabled ||
      closed.current ||
      !mounted.current ||
      loadingPreview ||
      outcomeUnknown.current
    )
      return
    const version = generation.current
    const isCurrent = () =>
      mounted.current && !closed.current && generation.current === version
    lock.current = true
    setBusy(true)
    setError(null)
    try {
      await previewTask.current
      if (!isCurrent()) return
      if (!rewound) {
        const target =
          restoredTurn.current ??
          (restoreFiles ? currentPreview?.target : await resolveTurn())
        if (!isCurrent()) return
        if (!target) throw new Error("Preview the file changes first")
        const turnId = nativeTurnId(target)
        if (!turnId) throw new Error("User message is not saved yet")
        if (restoreFiles && !restoredTurn.current) {
          if (
            currentPreview?.data.canRevert !== true ||
            typeof currentPreview.data.previewToken !== "string"
          )
            throw new Error("No verified file restore preview")
          // Conservatively block a second apply when the transport outcome is
          // unknown. A definitive refusal can be previewed again.
          outcomeUnknown.current = true
          setFileOutcomeUnknown(true)
          setPreview(null)
          const response = await execute("workspace_rewind_files", {
            turnId,
            expectedTurn: nativeExpectedTurn(target),
            dryRun: false,
            previewToken: currentPreview.data.previewToken,
          })
          if (!isCurrent()) return
          const data = nativeRecord(nativeResultData(response))
          setFileResult(data)
          if (
            data.uncertain === true ||
            String(data.reason ?? "").includes("outcome_unknown")
          )
            throw new Error(
              "File restore outcome is unknown; inspect files before continuing"
            )
          outcomeUnknown.current = false
          setFileOutcomeUnknown(false)
          requireNativeAck(response, "reverted")
          restoredTurn.current = target
        }
        if (!isCurrent()) return
        outcomeUnknown.current = true
        setFileOutcomeUnknown(true)
        const response = await execute("rewind", {
          turnId,
          expectedTurn: nativeExpectedTurn(target),
        })
        if (!isCurrent()) return
        const data = nativeRecord(nativeResultData(response))
        if (
          data.uncertain === true ||
          String(data.reason ?? "").includes("outcome_unknown")
        )
          throw new Error("History rewind outcome is unknown")
        outcomeUnknown.current = false
        setFileOutcomeUnknown(false)
        requireNativeAck(response, "rewound")
        rewoundTurn.current = target
        setRewound(true)
      }
      if (!rewoundTurn.current) throw new Error("Missing rewind target")
      await onReconcile(rewoundTurn.current)
      if (!isCurrent()) return
      onDraft(draft)
      close()
    } catch (e) {
      if (isCurrent()) setError(String(e))
    } finally {
      lock.current = false
      if (mounted.current && !closed.current) setBusy(false)
    }
  }
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !busy) close()
      }}
    >
      <DialogContent showCloseButton={!busy}>
        <DialogHeader>
          <DialogTitle>{t("editTitle")}</DialogTitle>
          <DialogDescription>{t("editHint")}</DialogDescription>
        </DialogHeader>
        <fieldset
          disabled={
            disabled ||
            busy ||
            rewound ||
            !!restoredTurn.current ||
            fileOutcomeUnknown
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
              disabled={
                disabled || busy || loadingPreview || fileOutcomeUnknown
              }
              onClick={() => void previewFiles()}
            >
              {t("preview")}
            </Button>
            {loadingPreview && (
              <p role="status" aria-live="polite">
                {t("filePreviewLoading")}
              </p>
            )}
            {currentPreview && (
              <div className="space-y-1 text-sm" role="status">
                <p>
                  {t(
                    currentPreview.data.canRevert === true
                      ? "filePreviewReady"
                      : "filePreviewUnavailable"
                  )}
                </p>
                {Array.isArray(currentPreview.data.paths) && (
                  <ul className="max-h-40 list-disc overflow-y-auto pl-5">
                    {currentPreview.data.paths
                      .filter(
                        (path): path is string => typeof path === "string"
                      )
                      .map((path) => (
                        <li key={path}>{path}</li>
                      ))}
                  </ul>
                )}
                {typeof currentPreview.data.reason === "string" && (
                  <p>{currentPreview.data.reason}</p>
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
              loadingPreview ||
              fileOutcomeUnknown ||
              (restoreFiles &&
                !restoredTurn.current &&
                !rewound &&
                (!supportsFiles ||
                  currentPreview?.data.canRevert !== true ||
                  typeof currentPreview.data.previewToken !== "string")) ||
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
              close()
            }}
          >
            {t("keepDraft")}
          </Button>
          <Button variant="ghost" disabled={busy} onClick={close}>
            {t("cancel")}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  )
}
