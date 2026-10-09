import { useEffect } from "react"
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react"
import { afterEach, describe, expect, it, vi } from "vitest"
import { readFileSync } from "node:fs"
import { resolve } from "node:path"
import { NativeQueueRecovery } from "./native-queue-recovery"
import { NativeFileRestore } from "./native-file-restore"
import { useNativeSession } from "@/hooks/use-native-session"
import { useMessageQueue } from "@/hooks/use-message-queue"
import {
  acpNativeCapabilities,
  acpNativeOperation,
} from "@/lib/native-session-api"
import type { MessageTurn, PromptDraft } from "@/lib/types"

vi.mock("next-intl", () => ({ useTranslations: () => (key: string) => key }))
vi.mock("@/lib/native-session-api", () => ({
  acpNativeCapabilities: vi.fn(),
  acpNativeOperation: vi.fn(),
}))
afterEach(() => {
  cleanup()
  vi.resetAllMocks()
})

describe("native queue read recovery", () => {
  it("keeps first draft blocked until explicit read refresh then drains it exactly once", async () => {
    vi.mocked(acpNativeCapabilities).mockResolvedValue({
      queue: { version: 1, method: "_session/queue", actions: ["list"] },
    })
    vi.mocked(acpNativeOperation)
      .mockRejectedValueOnce(new Error("temporary read failure"))
      .mockResolvedValue({
        status: "ok",
        result: { data: [], nextCursor: null },
      })
    const send = vi.fn()
    const draft: PromptDraft = {
      displayText: "first message",
      blocks: [{ type: "text", text: "first message" }],
    }
    function NewSession() {
      const native = useNativeSession("new-c", "new-s", "connected", false)
      const { queue, enqueue, dequeue } = useMessageQueue()
      useEffect(() => {
        if (native.nativeQueueBlocked || !queue.length) return
        const timer = setTimeout(() => {
          const next = dequeue()
          if (next) send(next.draft)
        }, 0)
        return () => clearTimeout(timer)
      }, [native.nativeQueueBlocked, queue.length, dequeue])
      return (
        <>
          <NativeQueueRecovery
            error={native.queueReadError}
            disabled={native.pending}
            onRefresh={native.refreshQueue}
          />
          <button onClick={() => enqueue(draft, null)}>send first</button>
          <span>local {queue.length}</span>
        </>
      )
    }
    const view = render(<NewSession />)
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "temporary read failure"
    )
    fireEvent.click(screen.getByText("send first"))
    expect(screen.getByText("local 1")).toBeInTheDocument()
    view.rerender(<NewSession />)
    expect(send).not.toHaveBeenCalled()
    expect(acpNativeOperation).toHaveBeenCalledTimes(1)
    fireEvent.click(screen.getByText("refreshNativeQueue"))
    await waitFor(() => expect(send).toHaveBeenCalledTimes(1))
    expect(send).toHaveBeenCalledWith(draft)
    expect(screen.getByText("local 0")).toBeInTheDocument()
    expect(screen.queryByRole("alert")).not.toBeInTheDocument()
    view.rerender(<NewSession />)
    expect(send).toHaveBeenCalledTimes(1)
    expect(vi.mocked(acpNativeOperation).mock.calls).toEqual([
      ["new-c", "queue", { action: "list", limit: 100 }],
      ["new-c", "queue", { action: "list", limit: 100 }],
    ])
  })
  it("places recovery in the real panel before its persisted-session gate", () => {
    const source = readFileSync(
      resolve(
        process.cwd(),
        "src/components/conversations/conversation-detail-panel.tsx"
      ),
      "utf8"
    )
    const banner = source.slice(
      source.indexOf("      topBanner={"),
      source.indexOf("          <SessionConfigStaleBanner")
    )
    expect(banner.indexOf("<NativeQueueRecovery")).toBeGreaterThan(0)
    expect(banner.indexOf("<NativeQueueRecovery")).toBeLessThan(
      banner.indexOf("{hasPersistedConversation &&")
    )
    expect(banner).toContain("error={native.queueReadError}")
    expect(banner).toContain("onRefresh={refreshNativeQueue}")
  })
})

describe("native file apply result", () => {
  const turn: MessageTurn = {
    id: "turn-1",
    timestamp: "stamp",
    role: "user",
    blocks: [{ type: "text", text: "prompt" }],
  }
  const tool: MessageTurn = {
    id: "turn-2",
    timestamp: "stamp",
    role: "assistant",
    blocks: [
      {
        type: "tool_use",
        tool_use_id: "patch1",
        tool_name: "apply_patch",
        input_preview: "x",
        status: "completed",
      },
    ],
  }
  it.each([1, 0])(
    "shows actual paths and skippedLinks=%i with the correct confirmation",
    async (skippedLinks) => {
      const execute = vi
        .fn()
        .mockResolvedValueOnce({
          canRewind: true,
          dryRun: true,
          filesChanged: ["preview-only.txt"],
        })
        .mockResolvedValueOnce({
          canRewind: true,
          dryRun: false,
          filesChanged: ["actually-restored.txt"],
          skippedLinks,
        })
      render(
        <NativeFileRestore
          caps={{
            sessionRewindFiles: {
              version: 1,
              method: "_session/rewind_files",
              dryRun: true,
            },
          }}
          execute={execute}
          disabled={false}
          viewer={false}
          idle
          turns={[turn]}
        />
      )
      fireEvent.change(screen.getByRole("combobox"), {
        target: { value: turn.id },
      })
      fireEvent.click(screen.getByText("preview"))
      await screen.findByText("confirmRestore")
      fireEvent.click(screen.getByText("confirmRestore"))
      expect(await screen.findByRole("status")).toHaveTextContent(
        skippedLinks ? "restoredPartial" : "restored"
      )
      if (skippedLinks)
        expect(
          screen.queryByText("restored", { exact: true })
        ).not.toBeInTheDocument()
      expect(screen.getByText("actually-restored.txt")).toBeInTheDocument()
      expect(screen.getByText("skipped Links:")).toBeInTheDocument()
      expect(
        screen.getByText(String(skippedLinks), { exact: true })
      ).toBeInTheDocument()
      expect(screen.queryByText("preview-only.txt")).not.toBeInTheDocument()
      expect(execute).toHaveBeenCalledTimes(2)
    }
  )
  it.each(["claude", "codex"])(
    "%s false apply ACK never shows success and retains refusal details",
    async (agent) => {
      const claude = agent === "claude"
      const execute = vi
        .fn()
        .mockResolvedValueOnce(
          claude
            ? { canRewind: true }
            : { canRevert: true, previewToken: "token" }
        )
        .mockResolvedValueOnce(
          claude
            ? { canRewind: false, reason: "checkpoint_missing" }
            : { reverted: false, reason: "patch_conflict" }
        )
      const caps = claude
        ? {
            sessionRewindFiles: {
              version: 1,
              method: "_session/rewind_files",
              dryRun: true,
            },
          }
        : {
            fileRevert: {
              version: 1,
              method: "_session/files/revert",
              dryRun: true,
              previewTokenRequired: true,
            },
          }
      render(
        <NativeFileRestore
          caps={caps}
          execute={execute}
          disabled={false}
          viewer={false}
          idle
          turns={[claude ? turn : tool]}
        />
      )
      fireEvent.change(screen.getByRole("combobox"), {
        target: { value: claude ? turn.id : "patch1" },
      })
      fireEvent.click(screen.getByText("preview"))
      await screen.findByText("confirmRestore")
      fireEvent.click(screen.getByText("confirmRestore"))
      await screen.findByRole("alert")
      expect(screen.queryByRole("status")).not.toBeInTheDocument()
      expect(
        screen.getByText(claude ? "checkpoint_missing" : "patch_conflict", {
          exact: true,
        })
      ).toBeInTheDocument()
      expect(execute).toHaveBeenCalledTimes(2)
    }
  )
})
