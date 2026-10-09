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
  nativeTurnId,
  requireNativeAck,
  type NativeExecute,
} from "@/lib/native-session"
import type { MessageTurn, PromptDraft } from "@/lib/types"

export function NativeEditDialog({
  turn,
  execute,
  disabled,
  onReconcile,
  onDraft,
  onClose,
}: {
  turn: MessageTurn
  execute: NativeExecute
  disabled: boolean
  onReconcile: () => Promise<void>
  onDraft: (draft: PromptDraft) => void
  onClose: () => void
}) {
  const t = useTranslations("NativeSession")
  const [text, setText] = useState(() => nativeEditDraft(turn).displayText)
  const [error, setError] = useState<string | null>(null)
  const [rewound, setRewound] = useState(false)
  const [busy, setBusy] = useState(false)
  const lock = useRef(false)
  const draft = nativeEditDraft(turn, text)
  async function confirm() {
    if (lock.current || disabled) return
    const turnId = nativeTurnId(turn)
    if (!turnId) return
    lock.current = true
    setBusy(true)
    setError(null)
    try {
      if (!rewound) {
        requireNativeAck(
          await execute("rewind", {
            turnId,
            expectedTurn: nativeExpectedTurn(turn),
          }),
          "rewound"
        )
        setRewound(true)
      }
      await onReconcile()
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
              disabled || busy || (!text.trim() && draft.blocks.length === 1)
            }
            onClick={() => void confirm()}
          >
            {t(rewound ? "reconcile" : "confirmEdit")}
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
