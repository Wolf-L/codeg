"use client"

import { useTranslations } from "next-intl"
import { nativeRecord } from "@/lib/native-session"

const label = (key: string) =>
  key.replace(/([a-z])([A-Z])/g, "$1 $2").replace(/_/g, " ")

/** Readable structured fields/lists, with large nested values collapsed. */
export function NativeDataView({
  value,
  depth = 0,
}: {
  value: unknown
  depth?: number
}) {
  const t = useTranslations("NativeSession")
  if (value === null || value === undefined)
    return <span className="text-muted-foreground">{t("unknown")}</span>
  if (typeof value === "boolean") return <span>{t(value ? "yes" : "no")}</span>
  if (typeof value !== "object")
    return (
      <span className="whitespace-pre-wrap break-all">{String(value)}</span>
    )
  if (depth > 6)
    return (
      <details>
        <summary>{t("details")}</summary>
        <pre className="whitespace-pre-wrap break-all">
          {JSON.stringify(value, null, 2)}
        </pre>
      </details>
    )
  if (Array.isArray(value))
    return value.length ? (
      <ul className="space-y-2">
        {value.map((item, i) => (
          <li key={i} className="rounded border p-2">
            <NativeDataView value={item} depth={depth + 1} />
          </li>
        ))}
      </ul>
    ) : (
      <span className="text-muted-foreground">{t("empty")}</span>
    )
  return (
    <dl className="space-y-1">
      {Object.entries(nativeRecord(value)).map(([key, item]) => (
        <div key={key} className="min-w-0">
          {typeof item === "object" && item !== null ? (
            <details open={depth === 0}>
              <summary className="cursor-pointer font-medium">
                {label(key)}
                {Array.isArray(item) ? ` (${item.length})` : ""}
              </summary>
              <dd className="pl-3">
                <NativeDataView value={item} depth={depth + 1} />
              </dd>
            </details>
          ) : (
            <>
              <dt className="inline text-muted-foreground">{label(key)}: </dt>
              <dd className="inline">
                <NativeDataView value={item} depth={depth + 1} />
              </dd>
            </>
          )}
        </div>
      ))}
    </dl>
  )
}
