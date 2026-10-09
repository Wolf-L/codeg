import { act, renderHook, waitFor } from "@testing-library/react"
import { beforeEach, describe, expect, it, vi } from "vitest"
import {
  acpNativeCapabilities,
  acpNativeOperation,
} from "@/lib/native-session-api"
import { StaleNativeResult, useNativeSession } from "./use-native-session"
vi.mock("@/lib/native-session-api", () => ({
  acpNativeCapabilities: vi.fn(),
  acpNativeOperation: vi.fn(),
}))
const caps = {
  runtime: {
    version: 1,
    readMethod: "_session/runtime/read",
    controlMethod: "_session/runtime/control",
    reads: ["context"],
    controls: ["reloadSkills"],
  },
  sessionRewind: { version: 1, method: "_session/rewind" },
}
const capabilities = vi.mocked(acpNativeCapabilities)
const operation = vi.mocked(acpNativeOperation)
beforeEach(() => {
  vi.resetAllMocks()
  capabilities.mockResolvedValue(caps)
})
describe("native session scope and admission", () => {
  it("drops a read after switching sessions on the same connection", async () => {
    let resolve!: (v: unknown) => void
    operation.mockImplementation(
      () =>
        new Promise((r) => {
          resolve = r
        })
    )
    const { result, rerender } = renderHook(
      ({ session }) => useNativeSession("c", session, "connected", false),
      { initialProps: { session: "a" } }
    )
    await waitFor(() => expect(result.current.caps).toEqual(caps))
    let request!: Promise<unknown>
    act(() => {
      request = result.current.execute("runtime_read", { resource: "context" })
    })
    const rejected = expect(request).rejects.toBeInstanceOf(StaleNativeResult)
    rerender({ session: "b" })
    await act(async () => {
      resolve({ data: { model: "old" } })
      await rejected
    })
  })
  it("hides stale capabilities and ignores late capability replies", async () => {
    let resolve!: (v: typeof caps) => void
    capabilities
      .mockImplementationOnce(
        () =>
          new Promise((r) => {
            resolve = r
          })
      )
      .mockResolvedValueOnce({})
    const { result, rerender } = renderHook(
      ({ id }) => useNativeSession(id, "s", "connected", false),
      { initialProps: { id: "old" } }
    )
    rerender({ id: "new" })
    await act(async () => {
      resolve(caps)
    })
    expect(result.current.caps).toEqual({})
  })
  it("refuses viewer mutations and busy rewind without dispatch", async () => {
    const { result, rerender } = renderHook(
      ({ viewer }) => useNativeSession("c", "s", "prompting", viewer),
      { initialProps: { viewer: true } }
    )
    await waitFor(() => expect(result.current.caps).toEqual(caps))
    await expect(
      result.current.execute("runtime_control", { action: "reloadSkills" })
    ).rejects.toThrow("viewer")
    rerender({ viewer: false })
    await expect(
      result.current.execute("rewind", {
        turnId: "turn-1",
        expectedTurn: { timestamp: "", agentMessageId: null, blocks: [] },
      })
    ).rejects.toThrow("Stop")
    expect(operation).not.toHaveBeenCalled()
  })
  it("single flights mutations and never retries failed operations", async () => {
    let reject!: (e: Error) => void
    operation.mockImplementation(
      () =>
        new Promise((_, r) => {
          reject = r
        })
    )
    const { result } = renderHook(() =>
      useNativeSession("c", "s", "connected", false)
    )
    await waitFor(() => expect(result.current.caps).toEqual(caps))
    let request!: Promise<unknown>
    act(() => {
      request = result.current.execute("rewind", {
        turnId: "turn-1",
        expectedTurn: { timestamp: "", agentMessageId: null, blocks: [] },
      })
    })
    const rejected = expect(request).rejects.toThrow("lost ACK")
    await expect(
      result.current.execute("rewind", {
        turnId: "turn-1",
        expectedTurn: { timestamp: "", agentMessageId: null, blocks: [] },
      })
    ).rejects.toThrow("pending")
    await act(async () => {
      reject(new Error("lost ACK"))
      await rejected
    })
    expect(operation).toHaveBeenCalledTimes(1)
  })
  it("reconciles the native queue when a turn completes even without an open panel", async () => {
    capabilities.mockResolvedValue({
      queue: { version: 1, method: "_session/queue", actions: ["list"] },
    })
    operation
      .mockResolvedValueOnce({
        status: "ok",
        result: { data: [{ id: "pending" }], nextCursor: null },
      })
      .mockResolvedValueOnce({
        status: "ok",
        result: { data: [], nextCursor: null },
      })
    const { result, rerender } = renderHook(
      ({ active }) =>
        useNativeSession("c", "s", active ? "prompting" : "connected", false),
      { initialProps: { active: true } }
    )
    await waitFor(() => expect(operation).toHaveBeenCalledTimes(1))
    expect(result.current.nativeQueueBlocked).toBe(true)
    rerender({ active: false })
    await waitFor(() => expect(result.current.nativeQueueBlocked).toBe(false))
    expect(operation).toHaveBeenCalledTimes(2)
  })
})
