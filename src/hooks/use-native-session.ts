"use client"

import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react"
import {
  acpNativeCapabilities,
  acpNativeOperation,
} from "@/lib/native-session-api"
import {
  nativeIsMutation,
  nativeNeedsIdle,
  nativeRecord,
  nativeResultData,
  nativeSupports,
  type NativeCapabilities,
  type NativeExecute,
} from "@/lib/native-session"
import type { ConnectionStatus } from "@/lib/types"

export class StaleNativeResult extends Error {
  constructor() {
    super("Session changed; the previous result was discarded")
  }
}

export function useNativeSession(
  connectionId: string | null,
  sessionId: string | null,
  status: ConnectionStatus | null,
  viewer: boolean | undefined
) {
  const scope = `${connectionId ?? ""}:${sessionId ?? ""}`
  const ready =
    !!connectionId && (status === "connected" || status === "prompting")
  const [loaded, setLoaded] = useState<{
    scope: string
    caps: NativeCapabilities
    error: string | null
  } | null>(null)
  const [pending, setPending] = useState(false)
  const [revision, setRevision] = useState(0)
  const [queueRevision, setQueueRevision] = useState(0)
  const [queueState, setQueueState] = useState<{
    scope: string
    blocked: boolean
    data?: Record<string, unknown>
    error?: string
  } | null>(null)
  const queueGeneration = useRef(0)
  const lifetime = useRef({ scope, active: true, busy: false, status, viewer })
  useLayoutEffect(() => {
    const current = { scope, active: true, busy: false, status, viewer }
    lifetime.current = current
    setPending(false)
    return () => {
      current.active = false
    }
    // A status change updates admission without invalidating an in-flight queue operation.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scope])
  useLayoutEffect(() => {
    lifetime.current.status = status
    lifetime.current.viewer = viewer
  }, [status, viewer])
  useEffect(() => {
    if (!ready || !connectionId) return
    let stale = false
    acpNativeCapabilities(connectionId)
      .then((caps) => {
        if (!stale) setLoaded({ scope, caps, error: null })
      })
      .catch((error) => {
        if (!stale) setLoaded({ scope, caps: {}, error: String(error) })
      })
    return () => {
      stale = true
    }
  }, [connectionId, scope, ready, revision])
  const caps = loaded?.scope === scope ? loaded.caps : EMPTY_CAPABILITIES
  const hasQueue = nativeSupports(caps, "queue", "list")
  const refreshQueue = useCallback(() => {
    if (!hasQueue) return
    ++queueGeneration.current
    setQueueState({ scope, blocked: true })
    setQueueRevision((v) => v + 1)
  }, [hasQueue, scope])
  useEffect(() => {
    if (!hasQueue || !ready || !connectionId) return
    let stale = false
    const generation = ++queueGeneration.current
    // Reconcile even when the tools panel is closed. Native turns own scheduling.
    acpNativeOperation(connectionId, "queue", { action: "list", limit: 100 })
      .then((response) => {
        const result = nativeRecord(nativeResultData(response))
        if (!Array.isArray(result.data))
          throw new Error("Invalid native queue list response")
        if (!stale && generation === queueGeneration.current)
          setQueueState({
            scope,
            data: result,
            blocked:
              !Array.isArray(result.data) ||
              result.data.length > 0 ||
              result.nextCursor != null,
          })
      })
      .catch((error: unknown) => {
        if (!stale && generation === queueGeneration.current)
          setQueueState({ scope, blocked: true, error: String(error) })
      })
    return () => {
      stale = true
    }
  }, [connectionId, scope, status, ready, hasQueue, queueRevision])
  const execute: NativeExecute = useCallback(
    async (operation, params) => {
      const owner = lifetime.current
      if (!owner.active || owner.scope !== scope || !connectionId)
        throw new StaleNativeResult()
      const p = nativeRecord(params)
      if (
        !nativeSupports(caps, operation, String(p.action ?? p.resource ?? ""))
      )
        throw new Error("Capability not advertised")
      if (owner.viewer && nativeIsMutation(operation, params))
        throw new Error("Read-only viewer")
      if (!["connected", "prompting"].includes(owner.status ?? ""))
        throw new Error("Reconnect this session before continuing")
      if (nativeNeedsIdle(operation, params) && owner.status !== "connected")
        throw new Error("Stop the active turn first")
      if (owner.busy) throw new Error("Another session operation is pending")
      owner.busy = true
      if (operation === "queue") ++queueGeneration.current
      setPending(true)
      try {
        const result = await acpNativeOperation(connectionId, operation, params)
        if (!owner.active || lifetime.current !== owner)
          throw new StaleNativeResult()
        if (operation === "queue") {
          if (p.action === "list") {
            const data = nativeRecord(nativeResultData(result))
            if (!Array.isArray(data.data))
              throw new Error("Invalid native queue list response")
            setQueueState({
              scope,
              data,
              blocked:
                !Array.isArray(data.data) ||
                data.data.length > 0 ||
                data.nextCursor != null,
            })
          } else {
            setQueueState({ scope, blocked: true })
            setQueueRevision((v) => v + 1)
          }
        }
        return result
      } catch (error) {
        if (operation === "queue" && owner.active && lifetime.current === owner)
          setQueueState({
            scope,
            blocked: true,
            ...(p.action === "list" ? { error: String(error) } : {}),
          })
        throw error
      } finally {
        owner.busy = false
        if (owner.active && lifetime.current === owner) setPending(false)
      }
    },
    [caps, connectionId, scope]
  )
  return {
    caps,
    execute,
    pending,
    nativeQueueBlocked:
      hasQueue && (queueState?.scope !== scope || queueState.blocked),
    queueSnapshot: queueState?.scope === scope ? queueState.data : undefined,
    queueReadError:
      queueState?.scope === scope ? (queueState.error ?? null) : null,
    refreshQueue,
    error: loaded?.scope === scope ? loaded.error : null,
    refreshCapabilities: () => setRevision((v) => v + 1),
  }
}
const EMPTY_CAPABILITIES: NativeCapabilities = Object.freeze({})
