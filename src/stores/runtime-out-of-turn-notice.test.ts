/**
 * Coverage for the out-of-turn content notice — the pill that offers to
 * re-read the transcript after an agent ran a turn CODEG never started.
 *
 * The flag is armed from the wire (per streamed token, so idempotence is a
 * performance contract as much as a correctness one) and disarmed by the read
 * that covers it. The interesting half is the disarm: only a SUCCESSFUL detail
 * response has actually re-parsed the transcript, so a failed load must leave
 * the pill standing — it is the user's only route back to that content.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import type { DbConversationDetail, MessageTurn } from "@/lib/types"
import {
  resetConversationRuntimeStore,
  useConversationRuntimeStore,
  type ConversationRuntimeSession,
} from "@/stores/conversation-runtime-store"

vi.mock("@/lib/api", () => ({
  getFolderConversation: vi.fn(),
  getFolderConversationTurns: vi.fn(),
}))

const { getFolderConversation } = await import("@/lib/api")
const mockGet = vi.mocked(getFolderConversation)

const CID = 42

function detail(): DbConversationDetail {
  return {
    summary: {
      id: CID,
      folder_id: 1,
      agent_type: "code_buddy",
      title: null,
      title_locked: false,
      status: "completed",
      kind: "regular",
      model: null,
      git_branch: null,
      external_id: "ext-1",
      message_count: 1,
      child_count: 0,
      created_at: new Date(1_700_000_000_000).toISOString(),
      updated_at: new Date(1_700_000_001_000).toISOString(),
      pinned_at: null,
    },
    turns: [
      {
        id: "turn-0",
        role: "assistant",
        blocks: [{ type: "text", text: "drained" }],
        timestamp: new Date(1_700_000_001_000).toISOString(),
      },
    ],
    session_stats: null,
    transcript_watermark: null,
    in_flight_user_turn_id: null,
    turns_offset: 0,
    turns_total: 1,
    assistant_turns_before_offset: 0,
    prefix_hash: null,
    uncovered_prefix_max_ts: null,
  }
}

function emptySession(conversationId: number): ConversationRuntimeSession {
  return {
    conversationId,
    externalId: "ext-1",
    dbConversationId: null,
    detail: null,
    detailLoading: false,
    detailError: null,
    acpLoadError: null,
    localTurns: [],
    backgroundTurns: [],
    pendingBackgroundSettlements: [],
    optimisticTurns: [],
    liveMessage: null,
    syncState: "idle",
    activeTurnToken: null,
    lastTurnOwned: false,
    liveOwnsActiveTurn: false,
    delegationKickoffText: null,
    sessionStats: null,
    historyAssistantBaseline: null,
    batchBoundaryIndex: null,
    batchBoundaryPrefixHash: null,
    loadingOlderTurns: false,
    olderTurnsPrependEpoch: 0,
    pendingOutOfTurnContent: false,
    pendingCleanup: false,
  }
}

function seed(overrides: Partial<ConversationRuntimeSession> = {}): void {
  useConversationRuntimeStore.setState({
    byConversationId: new Map([[CID, { ...emptySession(CID), ...overrides }]]),
  })
}

function session(): ConversationRuntimeSession | undefined {
  return useConversationRuntimeStore.getState().byConversationId.get(CID)
}

function actions() {
  return useConversationRuntimeStore.getState().actions
}

async function flush(): Promise<void> {
  await Promise.resolve()
  await Promise.resolve()
  await Promise.resolve()
}

beforeEach(() => {
  resetConversationRuntimeStore()
  mockGet.mockReset()
})

describe("native rewind reconciliation", () => {
  const removed: MessageTurn = {
    id: "turn-2",
    role: "user",
    blocks: [{ type: "text", text: "UI_REMOVE" }],
    timestamp: new Date(1_700_000_002_000).toISOString(),
    agent_message_id: "native-remove",
  }
  const staleDetail = (): DbConversationDetail => ({
    ...detail(),
    turns: [...detail().turns, removed],
    turns_total: 2,
  })
  const seedStale = () => {
    seed({ detail: staleDetail(), localTurns: [removed] })
    return session()
  }
  const deferredRead = () => {
    let resolve!: (value: DbConversationDetail) => void
    const pending = new Promise<DbConversationDetail>((done) => {
      resolve = done
    })
    mockGet.mockReturnValueOnce(pending)
    return resolve
  }

  afterEach(() => {
    vi.useRealTimers()
  })

  it("replaces all stale overlays only after a fresh persisted read", async () => {
    const stale = removed
    seed({
      detail: detail(),
      localTurns: [stale],
      backgroundTurns: [{ turn: stale, watermark: 123 }],
      syncState: "awaiting_persist",
    })
    const next = { ...detail(), turns: [], turns_total: 0 }
    mockGet.mockResolvedValue(next)
    await actions().reconcileNativeRewind(CID, stale, () => true)
    expect(session()?.detail?.turns).toEqual([])
    expect(session()?.localTurns).toEqual([])
    expect(session()?.backgroundTurns).toEqual([])
    expect(session()?.optimisticTurns).toEqual([])
    expect(session()?.liveMessage).toBeNull()
    expect(session()?.syncState).toBe("idle")
    expect(session()?.externalId).toBe("ext-1")
  })

  it("keeps overlays on failed read and rejects a switched or newly active session", async () => {
    const stale = removed
    seed({ detail: detail(), localTurns: [stale] })
    mockGet.mockRejectedValueOnce(new Error("read failed"))
    await expect(
      actions().reconcileNativeRewind(CID, stale, () => true)
    ).rejects.toThrow("read failed")
    expect(session()?.localTurns).toEqual([stale])
    mockGet.mockResolvedValue(detail())
    await expect(
      actions().reconcileNativeRewind(CID, stale, () => false)
    ).rejects.toThrow("Session changed")
    mockGet.mockResolvedValue({
      ...detail(),
      in_flight_user_turn_id: "turn-new",
    })
    await expect(
      actions().reconcileNativeRewind(CID, stale, () => true)
    ).rejects.toThrow("new turn")
    expect(session()?.localTurns).toEqual([stale])
  })

  it("polls stale full reads without repainting, then commits the rewound prefix", async () => {
    vi.useFakeTimers()
    const before = seedStale()
    mockGet
      .mockResolvedValueOnce(staleDetail())
      .mockResolvedValueOnce(staleDetail())
      .mockResolvedValueOnce(detail())
    const pending = actions().reconcileNativeRewind(CID, removed, () => true)
    await flush()
    expect(session()).toBe(before)
    await vi.advanceTimersByTimeAsync(74)
    expect(mockGet).toHaveBeenCalledTimes(1)
    await vi.advanceTimersByTimeAsync(1)
    expect(mockGet).toHaveBeenCalledTimes(2)
    expect(session()).toBe(before)
    await vi.advanceTimersByTimeAsync(175)
    await pending
    expect(mockGet.mock.calls).toEqual([[CID], [CID], [CID]])
    expect(session()?.detail?.turns).toEqual(detail().turns)
    expect(session()?.localTurns).toEqual([])
    expect(vi.getTimerCount()).toBe(0)
  })

  it("times out after the bounded stale reads without clearing any overlays", async () => {
    vi.useFakeTimers()
    const before = seedStale()
    mockGet.mockResolvedValue(staleDetail())
    const pending = actions().reconcileNativeRewind(CID, removed, () => true)
    const rejection = expect(pending).rejects.toThrow(
      "Native history has not finished rewinding"
    )
    await vi.runAllTimersAsync()
    await rejection
    expect(mockGet).toHaveBeenCalledTimes(6)
    expect(session()).toBe(before)
    expect(vi.getTimerCount()).toBe(0)
  })

  it("matches a removed native ID even when its parser position changed", async () => {
    vi.useFakeTimers()
    seedStale()
    mockGet
      .mockResolvedValueOnce({
        ...detail(),
        turns: [{ ...removed, id: "turn-99" }],
      })
      .mockResolvedValueOnce(detail())
    const pending = actions().reconcileNativeRewind(CID, removed, () => true)
    await flush()
    expect(session()?.localTurns).toEqual([removed])
    await vi.runAllTimersAsync()
    await pending
    expect(mockGet).toHaveBeenCalledTimes(2)
  })

  it("uses source_turn_id for targets without a native ID and fetches the bound DB row", async () => {
    vi.useFakeTimers()
    const target = {
      ...removed,
      id: "live-42",
      source_turn_id: "turn-2",
      agent_message_id: undefined,
    }
    seed({ dbConversationId: 99, localTurns: [target] })
    const next = { ...detail(), summary: { ...detail().summary, id: 99 } }
    mockGet
      .mockResolvedValueOnce({
        ...next,
        turns: [{ ...removed, agent_message_id: undefined }],
      })
      .mockResolvedValueOnce(next)
    const pending = actions().reconcileNativeRewind(CID, target, () => true)
    await vi.runAllTimersAsync()
    await pending
    expect(mockGet.mock.calls).toEqual([[99], [99]])
    expect(session()?.dbConversationId).toBe(99)
  })

  it("rejects a session switch during a read without repainting or polling again", async () => {
    vi.useFakeTimers()
    const before = seedStale()
    const resolve = deferredRead()
    let current = true
    const pending = actions().reconcileNativeRewind(CID, removed, () => current)
    const rejection = expect(pending).rejects.toThrow("Session changed")
    current = false
    resolve(staleDetail())
    await rejection
    expect(session()).toBe(before)
    expect(mockGet).toHaveBeenCalledTimes(1)
    expect(vi.getTimerCount()).toBe(0)
  })

  it("stops before the next read when the session switches during backoff", async () => {
    vi.useFakeTimers()
    const before = seedStale()
    mockGet.mockResolvedValue(staleDetail())
    let current = true
    const pending = actions().reconcileNativeRewind(CID, removed, () => current)
    const rejection = expect(pending).rejects.toThrow("Session changed")
    await flush()
    current = false
    await vi.runAllTimersAsync()
    await rejection
    expect(mockGet).toHaveBeenCalledTimes(1)
    expect(session()).toBe(before)
  })

  it("abandons a superseded read immediately and preserves the newer detail", async () => {
    vi.useFakeTimers()
    seedStale()
    const resolve = deferredRead()
    const pending = actions().reconcileNativeRewind(CID, removed, () => true)
    const rejection = expect(pending).rejects.toThrow("Session changed")
    const newer = detail()
    mockGet.mockResolvedValueOnce(newer)
    actions().refetchDetail(CID, { preserveLive: true })
    await flush()
    const before = session()
    resolve(staleDetail())
    await rejection
    expect(session()).toBe(before)
    expect(session()?.detail).toBe(newer)
    expect(mockGet).toHaveBeenCalledTimes(2)
    expect(vi.getTimerCount()).toBe(0)
  })

  it("does not invalidate another pending fetch when already switched away", async () => {
    seedStale()
    const resolve = deferredRead()
    actions().refetchDetail(CID)
    await expect(
      actions().reconcileNativeRewind(CID, removed, () => false)
    ).rejects.toThrow("Session changed")
    const next = detail()
    resolve(next)
    await flush()
    expect(session()?.detail).toBe(next)
    expect(mockGet).toHaveBeenCalledTimes(1)
  })

  it("stops a superseded poll during backoff before making another request", async () => {
    vi.useFakeTimers()
    seedStale()
    mockGet.mockResolvedValueOnce(staleDetail()).mockResolvedValueOnce(detail())
    const pending = actions().reconcileNativeRewind(CID, removed, () => true)
    const rejection = expect(pending).rejects.toThrow("Session changed")
    await flush()
    actions().refetchDetail(CID, { preserveLive: true })
    await flush()
    const before = session()
    await vi.runAllTimersAsync()
    await rejection
    expect(mockGet).toHaveBeenCalledTimes(2)
    expect(session()).toBe(before)
  })

  it("does not resurrect a removed session", async () => {
    seedStale()
    const resolve = deferredRead()
    const pending = actions().reconcileNativeRewind(CID, removed, () => true)
    const rejection = expect(pending).rejects.toThrow("Session changed")
    actions().removeConversation(CID)
    resolve(detail())
    await rejection
    expect(session()).toBeUndefined()
  })

  it.each(["external", "binding"])(
    "rejects a runtime %s identity change during the read",
    async (kind) => {
      seedStale()
      const resolve = deferredRead()
      const pending = actions().reconcileNativeRewind(CID, removed, () => true)
      const rejection = expect(pending).rejects.toThrow("Session changed")
      if (kind === "external") actions().setExternalId(CID, "other-session")
      else actions().setDbConversationId(CID, 99)
      const before = session()
      resolve(detail())
      await rejection
      expect(session()).toBe(before)
    }
  )

  it("rejects an in-flight response even if the removed target is still present", async () => {
    vi.useFakeTimers()
    const before = seedStale()
    mockGet.mockResolvedValue({
      ...staleDetail(),
      in_flight_user_turn_id: "new-turn",
    })
    await expect(
      actions().reconcileNativeRewind(CID, removed, () => true)
    ).rejects.toThrow("new turn")
    expect(session()).toBe(before)
    expect(mockGet).toHaveBeenCalledTimes(1)
    expect(vi.getTimerCount()).toBe(0)
  })

  it("refuses an already active local turn before fetching", async () => {
    seed({ activeTurnToken: "running", localTurns: [removed] })
    const before = session()
    await expect(
      actions().reconcileNativeRewind(CID, removed, () => true)
    ).rejects.toThrow("new turn")
    expect(mockGet).not.toHaveBeenCalled()
    expect(session()).toBe(before)
  })

  it("preserves background turns received while the read is outstanding", async () => {
    seedStale()
    const resolve = deferredRead()
    const pending = actions().reconcileNativeRewind(CID, removed, () => true)
    const rejection = expect(pending).rejects.toThrow("new turn")
    actions().applyBackgroundActivity(CID, detail().turns, 500)
    const before = session()
    resolve(detail())
    await rejection
    expect(session()).toBe(before)
  })

  it.each([false, true])(
    "preserves a new local turn despite a stale idle HTTP response (completed=%s)",
    async (completed) => {
      seedStale()
      const resolve = deferredRead()
      const pending = actions().reconcileNativeRewind(CID, removed, () => true)
      const rejection = expect(pending).rejects.toThrow("new turn")
      actions().appendOptimisticTurn(
        CID,
        { ...removed, id: "optimistic-new" },
        "token-new"
      )
      if (completed) actions().completeTurn(CID)
      const before = session()
      resolve(detail())
      await rejection
      expect(session()).toBe(before)
    }
  )

  it.each([
    { ...detail(), summary: { ...detail().summary, external_id: "other" } },
    { ...detail(), summary: { ...detail().summary, id: 99 } },
  ])("rejects history belonging to another session", async (candidate) => {
    const before = seedStale()
    mockGet.mockResolvedValue(candidate)
    await expect(
      actions().reconcileNativeRewind(CID, removed, () => true)
    ).rejects.toThrow("Session changed")
    expect(session()).toBe(before)
  })

  it.each([
    { ...detail(), turns_offset: 10, turns_total: 11 },
    { ...detail(), turns_offset: 0, turns_total: 10 },
  ])(
    "does not treat absence from a partial window as a rewind",
    async (candidate) => {
      const before = seedStale()
      mockGet.mockResolvedValue(candidate)
      await expect(
        actions().reconcileNativeRewind(CID, removed, () => true)
      ).rejects.toThrow("Full history")
      expect(session()).toBe(before)
    }
  )
})

describe("markOutOfTurnContent", () => {
  it("arms the pill", () => {
    seed()
    actions().markOutOfTurnContent(CID)
    expect(session()?.pendingOutOfTurnContent).toBe(true)
  })

  it("is referentially idempotent once armed", () => {
    seed()
    actions().markOutOfTurnContent(CID)
    const armed = useConversationRuntimeStore.getState().byConversationId

    // Called once per streamed token for the whole drain: a repeat must not
    // allocate a new map/session, or every token re-renders the thread.
    actions().markOutOfTurnContent(CID)
    expect(useConversationRuntimeStore.getState().byConversationId).toBe(armed)
  })

  it("does not resurrect a session whose tab already closed", () => {
    resetConversationRuntimeStore()
    actions().markOutOfTurnContent(CID)
    expect(
      useConversationRuntimeStore.getState().byConversationId.has(CID)
    ).toBe(false)
  })
})

describe("disarming", () => {
  it("clears once the refetch it offers lands", async () => {
    seed({ pendingOutOfTurnContent: true })
    mockGet.mockResolvedValue(detail())

    actions().refetchDetail(CID, { preserveLive: true })
    await flush()

    expect(session()?.pendingOutOfTurnContent).toBe(false)
    expect(session()?.detail?.turns.map((t) => t.id)).toEqual(["turn-0"])
  })

  it("stays armed when the refetch fails", async () => {
    seed({ pendingOutOfTurnContent: true })
    mockGet.mockRejectedValue(new Error("transcript unreadable"))

    actions().refetchDetail(CID, { preserveLive: true })
    await flush()

    // Nothing was re-parsed, so the content is still unrendered and the pill
    // is the only way back to it.
    expect(session()?.detailError).toBe("transcript unreadable")
    expect(session()?.pendingOutOfTurnContent).toBe(true)
  })

  it("survives the draft→real row migration from either side", () => {
    const armedOnDraft = { ...emptySession(-9), pendingOutOfTurnContent: true }
    const plainTarget = emptySession(CID)
    useConversationRuntimeStore.setState({
      byConversationId: new Map([
        [-9, armedOnDraft],
        [CID, plainTarget],
      ]),
    })
    actions().migrateConversation(-9, CID)
    expect(session()?.pendingOutOfTurnContent).toBe(true)

    // And the other way round: the merge spreads `from` over `to`, so without
    // an explicit OR the target's own pill would be dropped on the floor.
    useConversationRuntimeStore.setState({
      byConversationId: new Map([
        [-9, emptySession(-9)],
        [CID, { ...emptySession(CID), pendingOutOfTurnContent: true }],
      ]),
    })
    actions().migrateConversation(-9, CID)
    expect(session()?.pendingOutOfTurnContent).toBe(true)
  })

  it("clears on any successful load, not just the one the pill triggered", async () => {
    seed({ pendingOutOfTurnContent: true })
    mockGet.mockResolvedValue(detail())

    // A cold open re-parses the same transcript, so it covers the same bytes.
    actions().fetchDetail(CID)
    await flush()

    expect(session()?.pendingOutOfTurnContent).toBe(false)
  })
})
