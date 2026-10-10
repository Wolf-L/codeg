import { StrictMode } from "react"
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react"
import { afterEach, describe, expect, it, vi } from "vitest"
import { NativeSessionTools } from "./native-session-tools"
import { NativeEditDialog } from "./native-edit-dialog"
import { NativeFileRestore } from "./native-file-restore"
import { NativeQueueControls } from "./native-queue-controls"
import type { NativeCapabilities } from "@/lib/native-session"
import type { MessageTurn } from "@/lib/types"
vi.mock("next-intl", () => ({ useTranslations: () => (key: string) => key }))
afterEach(cleanup)
const turn: MessageTurn = {
  id: "turn-1",
  role: "user",
  timestamp: "stamp",
  agent_message_id: "u1",
  blocks: [
    { type: "text", text: "old" },
    { type: "image", data: "abc", mime_type: "image/png" },
  ],
}
const expectedTurn = {
  timestamp: turn.timestamp,
  agentMessageId: "u1",
  blocks: turn.blocks,
}
const runtime = {
  version: 1,
  readMethod: "_session/runtime/read",
  controlMethod: "_session/runtime/control",
  reads: ["context", "queuedMessages"],
  controls: ["cancelQueuedMessage", "reloadSkills"],
  context: { details: ["summary", "full"], fullMayUseNetwork: true },
}
function panel(caps: NativeCapabilities, execute = vi.fn(), viewer = false) {
  return render(
    <NativeSessionTools
      caps={caps}
      execute={execute}
      disabled={false}
      viewer={viewer}
      idle
      turns={[]}
      localQueueCount={0}
    />
  )
}
describe("native session UI", () => {
  it("lists and unlinks the adapter's nested attachment result", async () => {
    const execute = vi.fn().mockResolvedValue({
      version: 1,
      data: {
        data: [
          {
            attachmentType: "pull_request",
            identityKey: "https://example.test/pr/1",
            payload: {},
          },
        ],
        nextCursor: null,
      },
    })
    panel(
      {
        discovery: {
          version: 1,
          attachmentMethod: "_session/attachments",
          attachmentActions: ["list", "remove"],
        },
      },
      execute
    )
    fireEvent.click(screen.getByText("title"))
    fireEvent.click(screen.getByText("refresh"))
    const unlink = await screen.findByText("unlink https://example.test/pr/1")
    await waitFor(() => expect(unlink).not.toBeDisabled())
    fireEvent.click(unlink)
    await waitFor(() =>
      expect(execute).toHaveBeenCalledWith("attachments", {
        action: "remove",
        attachmentType: "pull_request",
        identityKey: "https://example.test/pr/1",
      })
    )
  })

  it("hides unsupported controls and never reads full automatically", async () => {
    const execute = vi
      .fn()
      .mockResolvedValue({ status: "ok", data: { totalTokens: 3 } })
    const { unmount } = panel({}, execute)
    expect(screen.queryByText("title")).not.toBeInTheDocument()
    unmount()
    panel({ runtime }, execute)
    fireEvent.click(screen.getByText("title"))
    expect(execute).not.toHaveBeenCalled()
    fireEvent.click(screen.getByText("resources.context"))
    await waitFor(() =>
      expect(execute).toHaveBeenCalledWith("runtime_read", {
        resource: "context",
      })
    )
    await waitFor(() =>
      expect(screen.getByText("fullContext")).not.toBeDisabled()
    )
    fireEvent.click(screen.getByText("fullContext"))
    await waitFor(() =>
      expect(execute).toHaveBeenCalledWith("runtime_read", {
        resource: "context",
        detail: "full",
      })
    )
    expect(execute).toHaveBeenCalledTimes(2)
  })
  it("viewer has no mutation buttons", () => {
    panel(
      { runtime, archive: { version: 1, archiveMethod: "_session/archive" } },
      vi.fn(),
      true
    )
    fireEvent.click(screen.getByText("title"))
    expect(screen.queryByText("controls.reloadSkills")).not.toBeInTheDocument()
    expect(screen.queryByText("archive")).not.toBeInTheDocument()
  })
  it("Claude false pending cancellation stays an error and is not retried", async () => {
    const execute = vi
      .fn()
      .mockResolvedValueOnce({
        status: "ok",
        data: { messages: [{ messageId: "p1" }] },
      })
      .mockResolvedValueOnce({
        status: "ok",
        data: { messageId: "p1", cancelled: false },
      })
    panel({ runtime }, execute)
    fireEvent.click(screen.getByText("title"))
    fireEvent.click(screen.getByText("resources.queuedMessages"))
    await screen.findByText("cancelPending")
    await waitFor(() =>
      expect(screen.getByText("cancelPending")).not.toBeDisabled()
    )
    fireEvent.click(screen.getByText("cancelPending"))
    expect(await screen.findByRole("alert")).toHaveTextContent("not confirmed")
    expect(execute).toHaveBeenCalledTimes(2)
  })
  it("MCP apply reads a fresh revision and sends only the host mode", async () => {
    const execute = vi
      .fn()
      .mockResolvedValueOnce({
        version: 1,
        revision: 8,
        uncertain: false,
        servers: [],
      })
      .mockResolvedValueOnce({
        version: 1,
        status: "partial",
        revision: 9,
        failed: ["offline"],
      })
      .mockResolvedValueOnce({
        version: 1,
        revision: 9,
        uncertain: false,
        servers: [],
      })
    panel(
      {
        sessionMcp: {
          version: 1,
          stateMethod: "_session/mcp/state",
          setMethod: "_session/mcp/set",
        },
      },
      execute
    )
    fireEvent.click(screen.getByText("title"))
    fireEvent.click(screen.getByText("applyConfiguredMcp"))
    await waitFor(() => expect(execute).toHaveBeenCalledTimes(3))
    expect(execute.mock.calls[1]).toEqual([
      "mcp_set",
      { expectedRevision: 8, mode: "reloadConfigured" },
    ])
    expect(screen.getByText("offline")).toBeInTheDocument()
  })
  it("does not apply MCP with uncertain state", async () => {
    const execute = vi.fn().mockResolvedValue({
      version: 1,
      revision: 8,
      uncertain: true,
      servers: [],
    })
    panel(
      {
        sessionMcp: {
          version: 1,
          stateMethod: "_session/mcp/state",
          setMethod: "_session/mcp/set",
        },
      },
      execute
    )
    fireEvent.click(screen.getByText("title"))
    fireEvent.click(screen.getByText("applyConfiguredMcp"))
    await screen.findByRole("alert")
    expect(execute).toHaveBeenCalledTimes(1)
  })
})
describe("native edit", () => {
  function deferred<T>() {
    let resolve!: (value: T) => void
    let reject!: (reason: Error) => void
    const promise = new Promise<T>((res, rej) => {
      resolve = res
      reject = rej
    })
    return { promise, resolve, reject }
  }
  const workspaceCaps = {
    workspaceRewindFiles: {
      version: 1,
      method: "codeg/workspace/rewind_files",
      dryRun: true,
      previewTokenRequired: true,
    },
  }
  const readyPreview = {
    canRevert: true,
    previewToken: "sha256:preview",
    paths: ["answer.txt"],
  }
  function editProps() {
    return {
      turn,
      caps: workspaceCaps,
      execute: vi.fn().mockResolvedValue(readyPreview),
      resolveTurn: vi.fn().mockResolvedValue(turn),
      disabled: false,
      onReconcile: vi.fn().mockResolvedValue(undefined),
      onDraft: vi.fn(),
      onClose: vi.fn(),
    }
  }
  it("automatically previews once in StrictMode despite inline callbacks and only applies on confirmation", async () => {
    const props = editProps()
    const pending = deferred<typeof readyPreview>()
    props.execute
      .mockReturnValueOnce(pending.promise)
      .mockResolvedValueOnce({ reverted: true })
      .mockResolvedValueOnce({ rewound: true })
    const view = () => (
      <StrictMode>
        <NativeEditDialog {...props} resolveTurn={async () => turn} />
      </StrictMode>
    )
    const { rerender } = render(view())
    expect(screen.getByText("confirmFilesAndHistory")).toBeDisabled()
    await screen.findByText("filePreviewLoading")
    await waitFor(() => expect(props.execute).toHaveBeenCalledTimes(1))
    rerender(view())
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "edited while previewing" },
    })
    await act(async () => pending.resolve(readyPreview))
    expect(screen.queryByText("filePreviewLoading")).not.toBeInTheDocument()
    expect(screen.getByText("confirmFilesAndHistory")).not.toBeDisabled()
    rerender(view())
    expect(props.execute).toHaveBeenCalledTimes(1)
    expect(props.execute).toHaveBeenNthCalledWith(1, "workspace_rewind_files", {
      turnId: turn.id,
      expectedTurn,
      dryRun: true,
    })
    fireEvent.click(screen.getByText("confirmFilesAndHistory"))
    fireEvent.click(screen.getByText("confirmFilesAndHistory"))
    await waitFor(() => expect(props.onClose).toHaveBeenCalledTimes(1))
    expect(props.execute).toHaveBeenCalledTimes(3)
    expect(props.execute).toHaveBeenNthCalledWith(2, "workspace_rewind_files", {
      turnId: turn.id,
      expectedTurn,
      dryRun: false,
      previewToken: readyPreview.previewToken,
    })
    expect(props.onDraft).toHaveBeenCalledWith(
      expect.objectContaining({ displayText: "edited while previewing" })
    )
  })

  it.each(["reject", "refuse", "missing token"])(
    "does not loop after preview %s; a manual retry enables confirmation",
    async (failure) => {
      const props = editProps()
      if (failure === "reject")
        props.execute.mockRejectedValueOnce(new Error("preview failed"))
      else
        props.execute.mockResolvedValueOnce(
          failure === "refuse"
            ? { canRevert: false, reason: "checkpoint_missing" }
            : { canRevert: true }
        )
      const { rerender } = render(<NativeEditDialog {...props} />)
      await waitFor(() => expect(props.execute).toHaveBeenCalledTimes(1))
      await waitFor(() =>
        expect(screen.queryByText("filePreviewLoading")).not.toBeInTheDocument()
      )
      expect(screen.getByText("confirmFilesAndHistory")).toBeDisabled()
      rerender(<NativeEditDialog {...props} resolveTurn={async () => turn} />)
      await act(async () => undefined)
      expect(props.execute).toHaveBeenCalledTimes(1)
      fireEvent.click(screen.getByText("preview"))
      await waitFor(() =>
        expect(screen.getByText("confirmFilesAndHistory")).not.toBeDisabled()
      )
      expect(props.execute).toHaveBeenCalledTimes(2)
    }
  )

  it.each(["cancel", "keepDraft", "unmount", "disabled", "scope"])(
    "discards unresolved target after %s without issuing a dryRun or mutation",
    async (change) => {
      const props = editProps()
      const pending = deferred<MessageTurn>()
      props.resolveTurn.mockReturnValueOnce(pending.promise)
      const { rerender, unmount } = render(<NativeEditDialog {...props} />)
      await waitFor(() => expect(props.resolveTurn).toHaveBeenCalledTimes(1))
      if (change === "unmount") unmount()
      else if (change === "disabled")
        rerender(<NativeEditDialog {...props} disabled />)
      else if (change === "scope")
        fireEvent.click(screen.getByRole("radio", { name: "historyOnly" }))
      else fireEvent.click(screen.getByText(change))
      await act(async () => pending.resolve(turn))
      expect(props.execute).not.toHaveBeenCalled()
      expect(props.onReconcile).not.toHaveBeenCalled()
      expect(props.onDraft).toHaveBeenCalledTimes(
        change === "keepDraft" ? 1 : 0
      )
    }
  )

  it.each(["resolve", "reject"])(
    "refreshes on returning to files and ignores an old preview %s",
    async (result) => {
      const props = editProps()
      const old = deferred<typeof readyPreview>()
      const fresh = deferred<typeof readyPreview>()
      props.execute
        .mockReturnValueOnce(old.promise)
        .mockReturnValueOnce(fresh.promise)
      render(<NativeEditDialog {...props} />)
      await waitFor(() => expect(props.execute).toHaveBeenCalledTimes(1))
      fireEvent.click(screen.getByRole("radio", { name: "historyOnly" }))
      fireEvent.click(screen.getByRole("radio", { name: "historyAndFiles" }))
      await screen.findByText("filePreviewLoading")
      expect(props.execute).toHaveBeenCalledTimes(1)
      await act(async () => {
        if (result === "resolve") old.resolve(readyPreview)
        else old.reject(new Error("obsolete failure"))
      })
      await waitFor(() => expect(props.execute).toHaveBeenCalledTimes(2))
      expect(screen.queryByText("answer.txt")).not.toBeInTheDocument()
      expect(screen.queryByRole("alert")).not.toBeInTheDocument()
      expect(screen.getByText("confirmFilesAndHistory")).toBeDisabled()
      await act(async () =>
        fresh.resolve({ ...readyPreview, paths: ["fresh.txt"] })
      )
      expect(screen.getByText("fresh.txt")).toBeVisible()
      expect(screen.getByText("confirmFilesAndHistory")).not.toBeDisabled()
    }
  )

  it("waits while externally disabled and refreshes after re-enabling without reviving a stale preview", async () => {
    const props = editProps()
    const pending = deferred<typeof readyPreview>()
    props.execute.mockReturnValueOnce(pending.promise)
    const { rerender } = render(<NativeEditDialog {...props} disabled />)
    await act(async () => undefined)
    expect(props.resolveTurn).not.toHaveBeenCalled()
    rerender(<NativeEditDialog {...props} />)
    await waitFor(() => expect(props.execute).toHaveBeenCalledTimes(1))
    rerender(<NativeEditDialog {...props} disabled />)
    await act(async () => pending.resolve(readyPreview))
    expect(screen.queryByText("answer.txt")).not.toBeInTheDocument()
    expect(screen.getByText("confirmFilesAndHistory")).toBeDisabled()
    rerender(<NativeEditDialog {...props} />)
    await screen.findByText("answer.txt")
    expect(props.execute).toHaveBeenCalledTimes(2)
  })

  it.each(["cancel", "unmount"])(
    "does not publish a dryRun result after %s",
    async (change) => {
      const props = editProps()
      const pending = deferred<typeof readyPreview>()
      props.execute.mockReturnValueOnce(pending.promise)
      const { unmount } = render(<NativeEditDialog {...props} />)
      await waitFor(() => expect(props.execute).toHaveBeenCalledTimes(1))
      if (change === "unmount") unmount()
      else fireEvent.click(screen.getByText("cancel"))
      await act(async () => pending.resolve(readyPreview))
      expect(screen.queryByText("answer.txt")).not.toBeInTheDocument()
      expect(props.execute).toHaveBeenCalledTimes(1)
      expect(props.onReconcile).not.toHaveBeenCalled()
    }
  )

  it.each(["unmount", "disabled"])(
    "does not rewind history or reuse the token after %s during file apply",
    async (change) => {
      const props = editProps()
      const pending = deferred<{ reverted: boolean }>()
      props.execute
        .mockResolvedValueOnce(readyPreview)
        .mockReturnValueOnce(pending.promise)
      const { rerender, unmount } = render(<NativeEditDialog {...props} />)
      await screen.findByText("answer.txt")
      fireEvent.click(screen.getByText("confirmFilesAndHistory"))
      await waitFor(() => expect(props.execute).toHaveBeenCalledTimes(2))
      if (change === "unmount") unmount()
      else {
        rerender(<NativeEditDialog {...props} disabled />)
        rerender(<NativeEditDialog {...props} />)
      }
      await act(async () => pending.resolve({ reverted: true }))
      expect(props.execute).toHaveBeenCalledTimes(2)
      expect(props.onReconcile).not.toHaveBeenCalled()
      expect(props.onDraft).not.toHaveBeenCalled()
      if (change === "disabled") {
        expect(screen.getByText("confirmFilesAndHistory")).toBeDisabled()
        expect(screen.getByText("preview")).toBeDisabled()
      }
    }
  )

  it.each(["transport", "uncertain", "outcome_unknown"])(
    "does not automatically retry an unknown file outcome (%s) on scope effects",
    async (failure) => {
      const props = editProps()
      props.execute.mockResolvedValueOnce(readyPreview)
      if (failure === "transport")
        props.execute.mockRejectedValueOnce(new Error("lost acknowledgement"))
      else
        props.execute.mockResolvedValueOnce({
          reverted: false,
          uncertain: failure === "uncertain",
          reason: failure,
        })
      const { rerender } = render(<NativeEditDialog {...props} />)
      await screen.findByText("answer.txt")
      fireEvent.click(screen.getByText("confirmFilesAndHistory"))
      await screen.findByRole("alert")
      expect(screen.getByRole("radio", { name: "historyOnly" })).toBeDisabled()
      rerender(<NativeEditDialog {...props} disabled />)
      rerender(<NativeEditDialog {...props} />)
      await act(async () => undefined)
      expect(screen.getByText("preview")).toBeDisabled()
      expect(screen.getByText("confirmFilesAndHistory")).toBeDisabled()
      expect(props.execute).toHaveBeenCalledTimes(2)
    }
  )

  it.each(["unmount", "disabled"])(
    "does not start history mutation when target resolution finishes after %s",
    async (change) => {
      const props = { ...editProps(), caps: {} }
      const pending = deferred<MessageTurn>()
      props.resolveTurn.mockReturnValueOnce(pending.promise)
      const { rerender, unmount } = render(<NativeEditDialog {...props} />)
      fireEvent.click(screen.getByText("confirmEdit"))
      await waitFor(() => expect(props.resolveTurn).toHaveBeenCalledTimes(1))
      if (change === "unmount") unmount()
      else rerender(<NativeEditDialog {...props} disabled />)
      await act(async () => pending.resolve(turn))
      expect(props.execute).not.toHaveBeenCalled()
      expect(props.onDraft).not.toHaveBeenCalled()
    }
  )

  it("does not publish a reconciled draft after unmount", async () => {
    const props = { ...editProps(), caps: {} }
    const pending = deferred<void>()
    props.execute.mockResolvedValue({ rewound: true })
    props.onReconcile.mockReturnValueOnce(pending.promise)
    const { unmount } = render(<NativeEditDialog {...props} />)
    fireEvent.click(screen.getByText("confirmEdit"))
    await waitFor(() => expect(props.onReconcile).toHaveBeenCalledTimes(1))
    unmount()
    await act(async () => pending.resolve(undefined))
    expect(props.onDraft).not.toHaveBeenCalled()
    expect(props.onClose).not.toHaveBeenCalled()
  })

  it.each(["transport", "uncertain", "outcome_unknown"])(
    "never retries an unknown history outcome (%s) after files were restored",
    async (failure) => {
      const props = editProps()
      props.execute
        .mockResolvedValueOnce(readyPreview)
        .mockResolvedValueOnce({ reverted: true })
      if (failure === "transport")
        props.execute.mockRejectedValueOnce(new Error("lost history ack"))
      else
        props.execute.mockResolvedValueOnce({
          rewound: false,
          uncertain: failure === "uncertain",
          reason: failure,
        })
      const { rerender } = render(<NativeEditDialog {...props} />)
      await screen.findByText("answer.txt")
      fireEvent.click(screen.getByText("confirmFilesAndHistory"))
      await screen.findByRole("alert")
      expect(screen.getByText("filesBeforeHistory")).toBeVisible()
      expect(screen.getByText("confirmEdit")).toBeDisabled()
      rerender(<NativeEditDialog {...props} disabled />)
      rerender(<NativeEditDialog {...props} />)
      fireEvent.click(screen.getByText("confirmEdit"))
      await act(async () => undefined)
      expect(props.execute.mock.calls.map((call) => call[0])).toEqual([
        "workspace_rewind_files",
        "workspace_rewind_files",
        "rewind",
      ])
      expect(props.onReconcile).not.toHaveBeenCalled()
      expect(props.onDraft).not.toHaveBeenCalled()
    }
  )

  it.each(["unmount", "disabled"])(
    "does not reconcile or publish a history response after %s",
    async (change) => {
      const props = { ...editProps(), caps: {} }
      const pending = deferred<{ rewound: boolean }>()
      props.execute.mockReturnValueOnce(pending.promise)
      const { rerender, unmount } = render(<NativeEditDialog {...props} />)
      fireEvent.click(screen.getByText("confirmEdit"))
      await waitFor(() => expect(props.execute).toHaveBeenCalledTimes(1))
      if (change === "unmount") unmount()
      else {
        rerender(<NativeEditDialog {...props} disabled />)
        rerender(<NativeEditDialog {...props} />)
      }
      await act(async () => pending.resolve({ rewound: true }))
      expect(props.execute).toHaveBeenCalledTimes(1)
      expect(props.onReconcile).not.toHaveBeenCalled()
      expect(props.onDraft).not.toHaveBeenCalled()
      if (change === "disabled")
        expect(screen.getByText("confirmEdit")).toBeDisabled()
    }
  )

  it("previews and restores files before rewinding history, without reapplying after a history refusal", async () => {
    const execute = vi
      .fn()
      .mockResolvedValueOnce({
        canRevert: true,
        previewToken: "sha256:preview",
        paths: ["answer.txt"],
      })
      .mockResolvedValueOnce({ reverted: true, paths: ["answer.txt"] })
      .mockResolvedValueOnce({ rewound: false, reason: "stale history" })
      .mockResolvedValueOnce({ rewound: true })
    const onDraft = vi.fn()
    render(
      <NativeEditDialog
        turn={turn}
        caps={workspaceCaps}
        execute={execute}
        resolveTurn={async () => turn}
        disabled={false}
        onReconcile={async () => undefined}
        onDraft={onDraft}
        onClose={vi.fn()}
      />
    )
    expect(screen.getByText("confirmFilesAndHistory")).toBeDisabled()
    await screen.findByText("answer.txt")
    fireEvent.click(screen.getByText("confirmFilesAndHistory"))
    await screen.findByRole("alert")
    expect(execute.mock.calls.map((c) => c[0])).toEqual([
      "workspace_rewind_files",
      "workspace_rewind_files",
      "rewind",
    ])
    expect(execute.mock.calls[1][1]).toEqual({
      turnId: turn.id,
      expectedTurn,
      dryRun: false,
      previewToken: "sha256:preview",
    })
    expect(onDraft).not.toHaveBeenCalled()
    expect(screen.getByText("filesBeforeHistory")).toBeVisible()
    fireEvent.click(screen.getByText("confirmEdit"))
    await waitFor(() => expect(onDraft).toHaveBeenCalledTimes(1))
    expect(execute.mock.calls.map((c) => c[0])).toEqual([
      "workspace_rewind_files",
      "workspace_rewind_files",
      "rewind",
      "rewind",
    ])
  })
  it("does not rewind history after a file conflict or an unknown file outcome", async () => {
    const execute = vi
      .fn()
      .mockResolvedValueOnce({
        canRevert: true,
        previewToken: "sha256:preview",
        paths: ["answer.txt"],
      })
      .mockResolvedValueOnce({ reverted: false, reason: "file_conflict" })
      .mockResolvedValueOnce({
        canRevert: true,
        previewToken: "sha256:fresh",
        paths: ["answer.txt"],
      })
      .mockRejectedValueOnce(new Error("transport lost"))
    render(
      <NativeEditDialog
        turn={turn}
        caps={workspaceCaps}
        execute={execute}
        resolveTurn={async () => turn}
        disabled={false}
        onReconcile={vi.fn()}
        onDraft={vi.fn()}
        onClose={vi.fn()}
      />
    )
    await waitFor(() =>
      expect(screen.getByText("confirmFilesAndHistory")).not.toBeDisabled()
    )
    fireEvent.click(screen.getByText("confirmFilesAndHistory"))
    await screen.findByRole("alert")
    expect(execute).toHaveBeenCalledTimes(2)
    expect(screen.getByText("confirmFilesAndHistory")).toBeDisabled()
    fireEvent.click(screen.getByText("preview"))
    await waitFor(() =>
      expect(screen.getByText("confirmFilesAndHistory")).not.toBeDisabled()
    )
    fireEvent.click(screen.getByText("confirmFilesAndHistory"))
    await screen.findByRole("alert")
    expect(execute).toHaveBeenCalledTimes(4)
    expect(screen.getByText("confirmFilesAndHistory")).toBeDisabled()
    expect(
      execute.mock.calls.every((c) => c[0] === "workspace_rewind_files")
    ).toBe(true)
  })
  it("allows explicitly keeping files when a historical checkpoint is missing", async () => {
    const execute = vi
      .fn()
      .mockResolvedValueOnce({ canRevert: false, reason: "checkpoint_missing" })
      .mockResolvedValueOnce({ rewound: true })
    const onDraft = vi.fn()
    render(
      <NativeEditDialog
        turn={turn}
        caps={workspaceCaps}
        execute={execute}
        resolveTurn={async () => turn}
        disabled={false}
        onReconcile={async () => undefined}
        onDraft={onDraft}
        onClose={vi.fn()}
      />
    )
    await screen.findByText("checkpoint_missing")
    expect(screen.getByText("confirmFilesAndHistory")).toBeDisabled()
    fireEvent.click(screen.getByRole("radio", { name: "historyOnly" }))
    fireEvent.click(screen.getByText("confirmEdit"))
    await waitFor(() => expect(onDraft).toHaveBeenCalledTimes(1))
    expect(execute.mock.calls.map((c) => c[0])).toEqual([
      "workspace_rewind_files",
      "rewind",
    ])
  })
  it("resolves a client turn before rewind and keeps the draft when resolution fails", async () => {
    const execute = vi.fn().mockResolvedValue({ rewound: true })
    const resolveTurn = vi.fn().mockRejectedValue(new Error("not saved"))
    render(
      <NativeEditDialog
        turn={{ ...turn, id: "optimistic-current" }}
        execute={execute}
        resolveTurn={resolveTurn}
        disabled={false}
        onReconcile={async () => undefined}
        onDraft={vi.fn()}
        onClose={vi.fn()}
      />
    )
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "edited" },
    })
    fireEvent.click(screen.getByText("confirmEdit"))
    await screen.findByRole("alert")
    expect(resolveTurn).toHaveBeenCalledTimes(1)
    expect(execute).not.toHaveBeenCalled()
    expect(screen.getByRole("textbox")).toHaveValue("edited")
    resolveTurn.mockResolvedValue(turn)
    fireEvent.click(screen.getByText("confirmEdit"))
    await waitFor(() =>
      expect(execute).toHaveBeenCalledWith("rewind", {
        turnId: "turn-1",
        expectedTurn,
      })
    )
  })
  it("retains draft/images on refusal without reconciliation or send", async () => {
    const execute = vi
      .fn()
      .mockResolvedValue({ rewound: false, reason: "stale" })
    const onDraft = vi.fn()
    const reconcile = vi.fn()
    render(
      <NativeEditDialog
        turn={turn}
        execute={execute}
        resolveTurn={async () => turn}
        disabled={false}
        onReconcile={async () => reconcile()}
        onDraft={onDraft}
        onClose={vi.fn()}
      />
    )
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "edited" },
    })
    fireEvent.click(screen.getByText("confirmEdit"))
    await screen.findByRole("alert")
    expect(screen.getByRole("textbox")).toHaveValue("edited")
    expect(reconcile).not.toHaveBeenCalled()
    expect(onDraft).not.toHaveBeenCalled()
    expect(execute).toHaveBeenCalledWith("rewind", {
      turnId: "turn-1",
      expectedTurn,
    })
    fireEvent.click(screen.getByText("keepDraft"))
    expect(onDraft).toHaveBeenCalledWith({
      displayText: "edited",
      blocks: [{ type: "text", text: "edited" }, turn.blocks[1]],
    })
  })
  it("rewinds once when reconciliation fails, then retries only reconciliation", async () => {
    const execute = vi.fn().mockResolvedValue({ rewound: true })
    const reconcile = vi
      .fn()
      .mockRejectedValueOnce(new Error("read failed"))
      .mockResolvedValueOnce(undefined)
    const onDraft = vi.fn()
    render(
      <NativeEditDialog
        turn={turn}
        execute={execute}
        resolveTurn={async () => turn}
        disabled={false}
        onReconcile={async () => reconcile()}
        onDraft={onDraft}
        onClose={vi.fn()}
      />
    )
    fireEvent.click(screen.getByText("confirmEdit"))
    fireEvent.click(screen.getByText("confirmEdit"))
    await screen.findByRole("alert")
    expect(execute).toHaveBeenCalledTimes(1)
    fireEvent.click(screen.getByText("reconcile"))
    await waitFor(() => expect(onDraft).toHaveBeenCalledTimes(1))
    expect(execute).toHaveBeenCalledTimes(1)
    expect(reconcile).toHaveBeenCalledTimes(2)
  })
})
describe("file restore and queue", () => {
  it("native queue update/delete/reorder/start use exact pending IDs and refresh", async () => {
    const items = ["q1", "q2"].map((id) => ({
      id,
      clientUserMessageId: `client-${id}`,
      input: [{ type: "text", text: id, text_elements: [] }],
    }))
    const execute = vi.fn().mockImplementation(async (_operation, params) => {
      if (params.action === "list")
        return { status: "ok", result: { data: items, nextCursor: null } }
      if (params.action === "delete")
        return { status: "ok", result: { deleted: true } }
      if (params.action === "start")
        return { status: "ok", result: { turn: { id: "native-turn" } } }
      if (params.action === "update")
        return { status: "ok", result: { queuedSubmission: items[0] } }
      return { status: "ok", result: {} }
    })
    render(
      <NativeQueueControls
        caps={{
          queue: {
            version: 1,
            method: "_session/queue",
            actions: ["list", "add", "update", "delete", "reorder", "start"],
          },
        }}
        execute={execute}
        disabled={false}
        viewer={false}
        idle
        localQueueCount={0}
      />
    )
    fireEvent.click(screen.getByText("refresh"))
    await screen.findAllByText("edit")
    await waitFor(() =>
      expect(screen.getAllByText("edit")[0]).not.toBeDisabled()
    )
    fireEvent.click(screen.getAllByText("edit")[0])
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "revised" },
    })
    fireEvent.click(screen.getByText("save"))
    await waitFor(() => expect(execute).toHaveBeenCalledTimes(3))
    expect(execute.mock.calls[1]).toEqual([
      "queue",
      {
        action: "update",
        queuedSubmissionId: "q1",
        input: [{ type: "text", text: "revised", text_elements: [] }],
      },
    ])
    fireEvent.click(screen.getAllByLabelText("moveDown")[0])
    await waitFor(() => expect(execute).toHaveBeenCalledTimes(5))
    expect(execute.mock.calls[3]).toEqual([
      "queue",
      { action: "reorder", queuedSubmissionIds: ["q2", "q1"] },
    ])
    fireEvent.click(screen.getAllByText("start")[1])
    await waitFor(() => expect(execute).toHaveBeenCalledTimes(7))
    expect(execute.mock.calls[5]).toEqual([
      "queue",
      { action: "start", queuedSubmissionId: "q2" },
    ])
    fireEvent.click(screen.getAllByText("delete")[0])
    await waitFor(() => expect(execute).toHaveBeenCalledTimes(9))
    expect(execute.mock.calls[7]).toEqual([
      "queue",
      { action: "delete", queuedSubmissionId: "q1" },
    ])
  })

  it("cannot restore when the native preview refuses or start native work over the local queue", async () => {
    const execute = vi
      .fn()
      .mockResolvedValue({ canRewind: false, reason: "checkpoint_missing" })
    const { unmount } = render(
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
    expect(await screen.findByText("confirmRestore")).toBeDisabled()
    unmount()
    render(
      <NativeQueueControls
        caps={{
          queue: {
            version: 1,
            method: "_session/queue",
            actions: ["list", "add"],
          },
        }}
        execute={execute}
        disabled={false}
        viewer={false}
        idle
        localQueueCount={1}
      />
    )
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "draft" },
    })
    expect(screen.getByText("addQueue")).toBeDisabled()
    expect(execute).toHaveBeenCalledTimes(1)
  })
  it("Claude freezes expected turn at selection and requires a canRewind preview", async () => {
    const execute = vi
      .fn()
      .mockResolvedValueOnce({
        canRewind: true,
        paths: ["a.txt"],
        dryRun: true,
      })
      .mockResolvedValueOnce({ canRewind: true, dryRun: false })
    const props = {
      caps: {
        sessionRewindFiles: {
          version: 1,
          method: "_session/rewind_files",
          dryRun: true,
        },
      },
      execute,
      disabled: false,
      viewer: false,
      idle: true,
    }
    const { rerender } = render(<NativeFileRestore {...props} turns={[turn]} />)
    fireEvent.change(screen.getByRole("combobox"), {
      target: { value: turn.id },
    })
    rerender(
      <NativeFileRestore
        {...props}
        turns={[{ ...turn, blocks: [{ type: "text", text: "different" }] }]}
      />
    )
    expect(screen.queryByText("confirmRestore")).not.toBeInTheDocument()
    fireEvent.click(screen.getByText("preview"))
    await screen.findByText("confirmRestore")
    await waitFor(() =>
      expect(screen.getByText("confirmRestore")).not.toBeDisabled()
    )
    fireEvent.click(screen.getByText("confirmRestore"))
    await waitFor(() => expect(execute).toHaveBeenCalledTimes(2))
    expect(execute.mock.calls[0]).toEqual([
      "rewind_files",
      { turnId: turn.id, expectedTurn, dryRun: true },
    ])
    expect(execute.mock.calls[1]).toEqual([
      "rewind_files",
      { turnId: turn.id, expectedTurn, dryRun: false },
    ])
  })
  it("Codex requires and returns the preview token for the selected tool", async () => {
    const tool: MessageTurn = {
      id: "turn-2",
      role: "assistant",
      timestamp: "",
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
    const execute = vi
      .fn()
      .mockResolvedValueOnce({
        canRevert: true,
        previewToken: "sha256:token",
        paths: ["x"],
      })
      .mockResolvedValueOnce({ reverted: true })
    render(
      <NativeFileRestore
        caps={{
          fileRevert: {
            version: 1,
            method: "_session/files/revert",
            dryRun: true,
            previewTokenRequired: true,
          },
        }}
        execute={execute}
        disabled={false}
        viewer={false}
        idle
        turns={[tool]}
      />
    )
    fireEvent.change(screen.getByRole("combobox"), {
      target: { value: "patch1" },
    })
    fireEvent.click(screen.getByText("preview"))
    await screen.findByText("confirmRestore")
    await waitFor(() =>
      expect(screen.getByText("confirmRestore")).not.toBeDisabled()
    )
    fireEvent.click(screen.getByText("confirmRestore"))
    await waitFor(() =>
      expect(execute).toHaveBeenCalledWith("file_revert", {
        toolCallId: "patch1",
        dryRun: false,
        previewToken: "sha256:token",
      })
    )
  })
  it("adds native UserInput with a client id and preserves the draft on failure", async () => {
    const execute = vi.fn().mockRejectedValue(new Error("lost acknowledgement"))
    render(
      <NativeQueueControls
        caps={{
          queue: {
            version: 1,
            method: "_session/queue",
            actions: ["list", "add"],
          },
        }}
        execute={execute}
        disabled={false}
        viewer={false}
        idle
        localQueueCount={0}
      />
    )
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "native draft" },
    })
    fireEvent.click(screen.getByText("addQueue"))
    await screen.findByRole("alert")
    expect(screen.getByRole("textbox")).toHaveValue("native draft")
    expect(execute).toHaveBeenCalledTimes(1)
    expect(execute).toHaveBeenCalledWith("queue", {
      action: "add",
      input: [{ type: "text", text: "native draft", text_elements: [] }],
      clientUserMessageId: expect.any(String),
    })
  })
})
