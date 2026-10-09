import { describe, expect, it } from "vitest"
import {
  editableQueueText,
  nativeEditDraft,
  nativeIsMutation,
  nativeSupports,
  nativeTurnId,
  replaceQueueText,
  resolveNativeEditTurn,
  requireNativeAck,
} from "./native-session"
import type { MessageTurn } from "./types"

describe("native session contracts", () => {
  it("requires version, exact methods and per-action declarations", () => {
    const queue = { version: 1, method: "_session/queue", actions: ["list"] }
    expect(nativeSupports({ queue }, "queue", "list")).toBe(true)
    expect(nativeSupports({ queue }, "queue", "add")).toBe(false)
    expect(
      nativeSupports({ queue: { ...queue, version: 2 } }, "queue", "list")
    ).toBe(false)
    expect(
      nativeSupports(
        { queue: { ...queue, method: "arbitrary/rpc" } },
        "queue",
        "list"
      )
    ).toBe(false)
    expect(nativeSupports({}, "rewind")).toBe(false)
    expect(
      nativeSupports(
        { jetbrains: { air: { version: 1, capabilities: ["sessionRewind"] } } },
        "rewind"
      )
    ).toBe(true)
  })
  it("never treats false or missing acknowledgements as success", () => {
    for (const result of [
      { rewound: false },
      {},
      { status: "unsupported" },
      { status: "ok", data: { cancelled: false } },
    ])
      expect(() => requireNativeAck(result, "cancelled")).toThrow()
    expect(() =>
      requireNativeAck({ status: "ok", data: { cancelled: true } }, "cancelled")
    ).not.toThrow()
  })
  it("preserves edited text and images without guessing live turn identities", () => {
    const turn: MessageTurn = {
      id: "turn-3",
      role: "user",
      timestamp: "",
      blocks: [
        { type: "text", text: "same" },
        { type: "image", data: "abc", mime_type: "image/png", uri: "pic.png" },
      ],
    }
    expect(nativeEditDraft(turn, "new")).toEqual({
      displayText: "new",
      blocks: [{ type: "text", text: "new" }, turn.blocks[1]],
    })
    expect(nativeTurnId({ ...turn, id: "optimistic-123" })).toBeNull()
    expect(
      nativeTurnId({ ...turn, id: "live-123", source_turn_id: "turn-3" })
    ).toBe("turn-3")
  })
  it("resolves a live user independently of assistant sub-turns and refuses ambiguity", () => {
    const live: MessageTurn = {
      id: "optimistic-current",
      role: "user",
      timestamp: "2026-10-09T07:00:00.000Z",
      blocks: [{ type: "text", text: "edit this" }],
    }
    const saved = {
      ...live,
      id: "turn-7",
      timestamp: "2026-10-09T07:00:00.010Z",
    }
    const assistant: MessageTurn = {
      ...saved,
      role: "assistant",
      id: "turn-6",
    }
    expect(resolveNativeEditTurn(live, [assistant, saved])).toBe(saved)
    expect(() => resolveNativeEditTurn(live, [])).toThrow()
    expect(() =>
      resolveNativeEditTurn(live, [saved, { ...saved, id: "turn-10" }])
    ).toThrow()
    expect(() =>
      resolveNativeEditTurn(live, [
        { ...saved, timestamp: "2026-10-09T06:59:00.000Z" },
      ])
    ).toThrow()
    expect(() =>
      resolveNativeEditTurn(live, [
        { ...saved, blocks: [{ type: "text", text: "edit this " }] },
      ])
    ).toThrow()
    expect(() =>
      resolveNativeEditTurn({ ...live, agent_message_id: "native-a" }, [
        { ...saved, agent_message_id: "native-b" },
      ])
    ).toThrow()
  })
  it("keeps native queue image/mention inputs and refuses flattening annotated text", () => {
    const image = { type: "localImage" as const, path: "C:/pic.png" }
    expect(
      replaceQueueText(
        [{ type: "text", text: "old", text_elements: [] }, image],
        "new"
      )
    ).toEqual([{ type: "text", text: "new", text_elements: [] }, image])
    expect(
      editableQueueText([
        {
          type: "text",
          text: "x",
          text_elements: [{ byteRange: { start: 0, end: 1 } }],
        },
      ])
    ).toBeNull()
    expect(() =>
      replaceQueueText(
        [
          { type: "text", text: "a" },
          { type: "text", text: "b" },
        ],
        "c"
      )
    ).toThrow()
  })
  it("viewer preview is a read; apply and all queue writes are mutations", () => {
    expect(nativeIsMutation("file_revert", { dryRun: true })).toBe(false)
    expect(nativeIsMutation("file_revert", { dryRun: false })).toBe(true)
    expect(nativeIsMutation("queue", { action: "list" })).toBe(false)
    expect(nativeIsMutation("queue", { action: "start" })).toBe(true)
  })
})
