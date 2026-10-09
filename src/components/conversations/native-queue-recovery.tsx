"use client"

import { useTranslations } from "next-intl"
import { Button } from "@/components/ui/button"

/** Lives outside the persisted-conversation gate so a failed initial read is recoverable. */
export function NativeQueueRecovery({
  error,
  disabled,
  onRefresh,
}: {
  error: string | null
  disabled: boolean
  onRefresh: () => void
}) {
  const t = useTranslations("NativeSession")
  if (!error) return null
  return (
    <div className="space-y-2 border-b px-4 py-2 text-sm">
      <p role="alert">
        {t("queueReadFailed")} {error}
      </p>
      <Button
        size="sm"
        variant="outline"
        disabled={disabled}
        onClick={onRefresh}
      >
        {t("refreshNativeQueue")}
      </Button>
    </div>
  )
}
