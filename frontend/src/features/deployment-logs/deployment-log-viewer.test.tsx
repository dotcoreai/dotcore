import { Code } from "@connectrpc/connect";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useRef, useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  DeploymentLogError,
  type DeploymentLogOperations,
  type ReadDeploymentLogsResult,
} from "@/api/deployment-log-client";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { LOADING_MINIMUM_VISIBLE_MS, LOADING_REVEAL_DELAY_MS } from "@/hooks/use-delayed-loading";
import { expectResourceMarker, inspectorTitle } from "@/test/resource-marker";

import { DeploymentLogViewer } from "./deployment-log-viewer";

const logEntry = (text: string, sourceName = "worker") => ({
  category: "stdout",
  sourceLabel: sourceName === "worker" ? "Worker" : "Sidecar",
  sourceName,
  text,
  timestamp: { nanos: 0, seconds: 1n },
  truncated: false,
});

function deploymentLogError(code: Code) {
  return new DeploymentLogError("follow failed", code);
}

// A follow that stays open without delivering anything.
function silentFollow() {
  return { [Symbol.asyncIterator]: () => ({ next: () => new Promise<never>(() => undefined) }) };
}

// A follow that fails with the given status after it opens.
function failingFollow(code: Code) {
  return {
    [Symbol.asyncIterator]: () => ({
      next: () => Promise.resolve().then(() => Promise.reject(deploymentLogError(code))),
    }),
  };
}

// A follow that delivers one entry, then ends cleanly.
function singleEntryFollow(entry: ReturnType<typeof logEntry>) {
  const events = [{ event: { kind: "entry" as const, entry } }];
  return (async function* () {
    await Promise.resolve();
    yield* events;
  })();
}

function renderViewer(operations: DeploymentLogOperations) {
  return render(
    <DeploymentLogViewer
      colorKey="deployment-1"
      name="Deployment one"
      deploymentId="deployment-1"
      onOpenChange={() => undefined}
      open
      operations={operations}
    />,
  );
}

function emptyHistory(): ReadDeploymentLogsResult {
  return {
    entries: [],
    historyState: "no-older-output-observed",
    latestPositions: [],
    responseTruncated: false,
    sources: [{ sourceLabel: "Worker", sourceName: "worker", state: "available" }],
  };
}

describe("DeploymentLogViewer", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.useRealTimers();
  });

  it("delays retained-history feedback and keeps toolbar and viewport timing aligned", async () => {
    vi.useFakeTimers();
    let resolveRead!: (value: ReadDeploymentLogsResult) => void;
    const read = vi.fn(
      () =>
        new Promise<ReadDeploymentLogsResult>((resolve) => {
          resolveRead = resolve;
        }),
    );

    render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={{
          follow: vi.fn(async function* () {
            await new Promise(() => undefined);
            yield { event: { kind: "entry" as const, entry: logEntry("unreachable") } };
          }),
          read,
        }}
      />,
    );
    const toolbarStatus = screen.getByLabelText("Log viewer footer").firstElementChild;

    expect(read).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("status", { name: "Log output status" })).not.toBeInTheDocument();
    expect(toolbarStatus).toHaveTextContent("");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_REVEAL_DELAY_MS);
    });
    expect(screen.getByRole("status", { name: "Log output status" })).toHaveTextContent(
      "Loading retained output…",
    );
    expect(toolbarStatus).toHaveTextContent("Loading retained output…");

    await act(async () => {
      resolveRead(emptyHistory());
      await Promise.resolve();
    });
    expect(screen.getByRole("status", { name: "Log output status" })).toHaveTextContent(
      "Loading retained output…",
    );
    expect(toolbarStatus).toHaveTextContent("Loading retained output…");
    expect(screen.queryByText("No retained output is available.")).not.toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_MINIMUM_VISIBLE_MS);
    });
    expect(screen.getByRole("status", { name: "Log output status" })).toHaveTextContent(
      "No retained output is available.",
    );
    expect(toolbarStatus).toHaveTextContent("Live");
  });

  it("delays earlier-history feedback while prepending loaded entries immediately", async () => {
    let resolveOlder!: (value: ReadDeploymentLogsResult) => void;
    const read = vi
      .fn()
      .mockResolvedValueOnce({
        ...emptyHistory(),
        entries: [logEntry("current")],
        historyState: "older-output-may-remain" as const,
        nextBeforeTimestamp: { nanos: 0, seconds: 10n },
      })
      .mockImplementationOnce(
        () =>
          new Promise<ReadDeploymentLogsResult>((resolve) => {
            resolveOlder = resolve;
          }),
      );

    render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={{ follow: vi.fn(async function* () {}), read }}
      />,
    );
    const log = await screen.findByRole("log");
    expect(await screen.findByText("current")).toBeVisible();
    Object.defineProperties(log, {
      clientHeight: { configurable: true, value: 100 },
      scrollHeight: { configurable: true, value: 1_000 },
      scrollTop: { configurable: true, value: 4, writable: true },
    });
    vi.useFakeTimers();

    fireEvent.scroll(log);
    expect(read).toHaveBeenCalledTimes(2);
    expect(screen.getByText("current")).toBeVisible();
    expect(screen.queryByText("Loading earlier output…")).not.toBeInTheDocument();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_REVEAL_DELAY_MS);
    });
    expect(screen.getByText("Loading earlier output…")).toBeVisible();
    expect(screen.getByText("current")).toBeVisible();

    await act(async () => {
      resolveOlder({
        ...emptyHistory(),
        entries: [{ ...logEntry("older"), timestamp: { nanos: 0, seconds: 0n } }],
      });
      await Promise.resolve();
    });
    expect(screen.getByText("older")).toBeVisible();
    expect(screen.getByText("current")).toBeVisible();
    expect(screen.getByText(/Loading earlier output…/)).toBeVisible();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(LOADING_MINIMUM_VISIBLE_MS);
    });
    expect(screen.queryByText(/Loading earlier output…/)).not.toBeInTheDocument();
  });
  it("loads retained history, renders one-line-flow text, and filters buffered rows", async () => {
    const read = vi.fn();
    const operations: DeploymentLogOperations = {
      follow: vi.fn(async function* () {
        await Promise.resolve();
        yield { event: { kind: "entry" as const, entry: logEntry("live") } };
        await new Promise(() => undefined);
      }),
      read: read.mockResolvedValue({
        entries: [
          logEntry("\u001b[31mretained\u001b[0m"),
          logEntry("sidecar", "sidecar"),
          logEntry(`<img src=x onerror="alert(1)">${"x".repeat(300)}`),
        ],
        historyState: "older-output-may-remain" as const,
        latestPositions: [{ latestTimestamp: { nanos: 0, seconds: 1n }, sourceName: "worker" }],
        nextBeforeTimestamp: { nanos: 0, seconds: 0n },
        responseTruncated: false,
        sources: [
          { sourceLabel: "Worker", sourceName: "worker", state: "available" as const },
          { sourceLabel: "Sidecar", sourceName: "sidecar", state: "available" as const },
        ],
      }),
    };
    const onOpenChange = vi.fn();
    const user = userEvent.setup();
    render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={onOpenChange}
        open
        operations={operations}
      />,
    );

    await screen.findByText("retained");
    expect(screen.getByRole("dialog", { name: "Deployment one · Logs" })).toHaveClass(
      "flex",
      "overflow-hidden",
    );
    expectResourceMarker(
      inspectorTitle(screen.getByRole("dialog", { name: "Deployment one · Logs" })),
      "deployment-1",
    );
    await waitFor(() =>
      expect(screen.getByRole("log", { name: "Deployment log output" })).toHaveTextContent("live"),
    );
    expect(
      screen.queryByText(/Older pages can omit equal-timestamp entries/),
    ).not.toBeInTheDocument();
    const toolbar = screen.getByLabelText("Log viewer toolbar");
    expect(toolbar.firstElementChild).toHaveAccessibleName("Log sources");
    expect(screen.getByLabelText("Log viewer footer").firstElementChild).toHaveTextContent("Live");
    const retainedRow = screen.getByText("retained").closest("li");
    expect(retainedRow).toHaveTextContent("00:00:01.000 Worker stdout retained");
    expect(retainedRow).toHaveClass("whitespace-pre-wrap", "[overflow-wrap:anywhere]");
    expect(retainedRow).not.toHaveClass("border-b", "bg-background", "px-3", "py-1.5");
    expect(retainedRow?.className).not.toMatch(/\b(?:border|divide|bg-|grid|rounded|shadow)/);
    expect(retainedRow?.parentElement).toHaveClass("space-y-1.5", "text-[11px]");
    expect(screen.getByText(/<img src=x onerror=/)).toBeVisible();
    expect(screen.getByRole("log").querySelector("img")).toBeNull();
    expect(screen.getByRole("log")).toHaveAttribute("aria-live", "off");

    await user.click(screen.getByLabelText("Sidecar"));
    expect(screen.getByRole("log")).not.toHaveTextContent("sidecar");
    expect(screen.getByRole("log")).toHaveTextContent("retained");

    expect(screen.queryByRole("button", { name: "Load older" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /scrolling/i })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /wrapping|wrap lines/i })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Latest" })).toBeDisabled();
    expect(read).toHaveBeenCalledTimes(1);
    expect(onOpenChange).not.toHaveBeenCalled();
  });

  it("follows only while at latest and conditionally jumps back to the bottom", async () => {
    let publishReplacement: (() => void) | undefined;
    let publishWhileDetached: (() => void) | undefined;
    const retained = Array.from({ length: 2 }, (_, index) => ({
      ...logEntry(`retained-${index}`, "worker"),
      timestamp: { nanos: 0, seconds: BigInt(index) },
    }));
    const follow = vi.fn(async function* () {
      await new Promise<void>((resolve) => {
        publishReplacement = resolve;
      });
      yield {
        event: {
          kind: "entry" as const,
          entry: { ...logEntry("replacement"), timestamp: { nanos: 0, seconds: 2n } },
        },
      };
      await new Promise<void>((resolve) => {
        publishWhileDetached = resolve;
      });
      yield {
        event: {
          kind: "entry" as const,
          entry: { ...logEntry("detached receipt"), timestamp: { nanos: 0, seconds: 3n } },
        },
      };
    });
    const operations: DeploymentLogOperations = {
      follow,
      read: vi.fn().mockResolvedValue({
        entries: retained,
        historyState: "no-older-output-observed" as const,
        latestPositions: [],
        responseTruncated: false,
        sources: [{ sourceLabel: "Worker", sourceName: "worker", state: "available" as const }],
      }),
    };
    const user = userEvent.setup();
    render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={operations}
      />,
    );

    const log = await screen.findByRole("log");
    let currentScrollTop = 900;
    const scrollTop = vi.fn((next: number) => {
      currentScrollTop = next;
    });
    Object.defineProperties(log, {
      clientHeight: { configurable: true, value: 100 },
      scrollHeight: { configurable: true, value: 1_000 },
      scrollTop: {
        configurable: true,
        get: () => currentScrollTop,
        set: scrollTop,
      },
    });
    await waitFor(() => {
      expect(follow).toHaveBeenCalled();
      expect(publishReplacement).toBeTypeOf("function");
    });
    scrollTop.mockClear();

    publishReplacement?.();
    await waitFor(() => expect(scrollTop).toHaveBeenCalledWith(1_000));

    currentScrollTop = 200;
    fireEvent.scroll(log);
    expect(screen.getByRole("button", { name: "Latest" })).toBeEnabled();

    scrollTop.mockClear();
    publishWhileDetached?.();
    await screen.findByText("detached receipt");
    expect(scrollTop).not.toHaveBeenCalled();

    currentScrollTop = 900;
    fireEvent.scroll(log);
    expect(screen.getByRole("button", { name: "Latest" })).toBeDisabled();
    currentScrollTop = 200;
    fireEvent.scroll(log);

    scrollTop.mockClear();
    await user.click(screen.getByRole("button", { name: "Latest" }));
    expect(scrollTop).toHaveBeenCalledWith(1_000);
    expect(log).toHaveFocus();
    expect(screen.getByRole("button", { name: "Latest" })).toBeDisabled();
  });

  it("restores a visible occurrence while older loading overlaps live receipt and reflow", async () => {
    let publishLive: (() => void) | undefined;
    let resolveOlder: ((result: ReadDeploymentLogsResult) => void) | undefined;
    let olderRendered = false;
    const read = vi
      .fn()
      .mockResolvedValueOnce({
        ...emptyHistory(),
        entries: [logEntry("current")],
        historyState: "older-output-may-remain" as const,
        nextBeforeTimestamp: { nanos: 0, seconds: 10n },
      })
      .mockImplementationOnce(
        () =>
          new Promise<ReadDeploymentLogsResult>((resolve) => {
            resolveOlder = resolve;
          }),
      );
    const follow = vi.fn(async function* () {
      await new Promise<void>((resolve) => {
        publishLive = resolve;
      });
      yield {
        event: {
          kind: "entry" as const,
          entry: { ...logEntry("live"), timestamp: { nanos: 0, seconds: 2n } },
        },
      };
    });
    render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={{ follow, read }}
      />,
    );

    const log = await screen.findByRole("log");
    const currentRow = (await screen.findByText("current")).closest("li");
    expect(currentRow).not.toBeNull();
    const currentEntryId = currentRow?.dataset.entryId;
    vi.spyOn(log, "getBoundingClientRect").mockReturnValue({ top: 0 } as DOMRect);
    vi.spyOn(currentRow!, "getBoundingClientRect").mockImplementation(
      () => ({ top: olderRendered ? 50 : 20 }) as DOMRect,
    );
    Object.defineProperties(log, {
      clientHeight: { configurable: true, value: 100 },
      scrollHeight: {
        configurable: true,
        get: () => (olderRendered ? 1_400 : 1_000),
      },
      scrollTop: { configurable: true, value: 4, writable: true },
    });

    fireEvent.scroll(log);
    fireEvent.scroll(log);
    expect(read).toHaveBeenCalledTimes(2);
    expect(read).toHaveBeenLastCalledWith(
      {
        beforeTimestamp: { nanos: 0, seconds: 10n },
        deploymentId: "deployment-1",
        pageSize: 200,
        sourceNames: undefined,
      },
      expect.any(AbortSignal),
    );

    publishLive?.();
    await screen.findByText("live");
    olderRendered = true;
    resolveOlder?.({
      ...emptyHistory(),
      entries: [{ ...logEntry("older"), timestamp: { nanos: 0, seconds: 0n } }],
      historyState: "no-older-output-observed",
    });
    await screen.findByText("older");
    await waitFor(() => expect(log.scrollTop).toBe(34));
    expect(screen.getByText("current").closest("li")).toHaveAttribute(
      "data-entry-id",
      currentEntryId,
    );
    expect(screen.queryByText(/whole-second timestamp boundary/)).not.toBeInTheDocument();
    expect(screen.queryByText(/No earlier retained output/)).not.toBeInTheDocument();
  });

  it("uses a newly visible occurrence when filtering changes during older loading", async () => {
    let resolveOlder: ((result: ReadDeploymentLogsResult) => void) | undefined;
    let olderRendered = false;
    const read = vi
      .fn()
      .mockResolvedValueOnce({
        ...emptyHistory(),
        entries: [logEntry("worker current"), logEntry("sidecar current", "sidecar")],
        historyState: "older-output-may-remain" as const,
        nextBeforeTimestamp: { nanos: 0, seconds: 10n },
        sources: [
          { sourceLabel: "Worker", sourceName: "worker", state: "available" as const },
          { sourceLabel: "Sidecar", sourceName: "sidecar", state: "available" as const },
        ],
      })
      .mockImplementationOnce(
        () =>
          new Promise<ReadDeploymentLogsResult>((resolve) => {
            resolveOlder = resolve;
          }),
      );
    const user = userEvent.setup();
    render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={{ follow: vi.fn(async function* () {}), read }}
      />,
    );

    const log = await screen.findByRole("log");
    const workerRow = (await screen.findByText("worker current")).closest("li");
    const sidecarRow = screen.getByText("sidecar current").closest("li");
    vi.spyOn(log, "getBoundingClientRect").mockReturnValue({ top: 0 } as DOMRect);
    vi.spyOn(workerRow!, "getBoundingClientRect").mockReturnValue({ top: 10 } as DOMRect);
    vi.spyOn(sidecarRow!, "getBoundingClientRect").mockImplementation(
      () => ({ top: olderRendered ? 70 : 30 }) as DOMRect,
    );
    Object.defineProperties(log, {
      clientHeight: { configurable: true, value: 100 },
      scrollHeight: { configurable: true, get: () => (olderRendered ? 1_500 : 1_000) },
      scrollTop: { configurable: true, value: 4, writable: true },
    });

    fireEvent.scroll(log);
    expect(read).toHaveBeenCalledTimes(2);
    await user.click(screen.getByLabelText("Worker"));
    expect(screen.queryByText("worker current")).not.toBeInTheDocument();
    fireEvent(window, new Event("resize"));

    olderRendered = true;
    resolveOlder?.({
      ...emptyHistory(),
      entries: [{ ...logEntry("sidecar older", "sidecar"), timestamp: { nanos: 0, seconds: 0n } }],
      historyState: "no-older-output-observed",
    });
    await screen.findByText("sidecar older");
    await waitFor(() => expect(log.scrollTop).toBe(44));
  });

  it("reacquires an anchor after all sources are hidden during older loading", async () => {
    let resolveOlder: ((result: ReadDeploymentLogsResult) => void) | undefined;
    let olderRendered = false;
    const read = vi
      .fn()
      .mockResolvedValueOnce({
        ...emptyHistory(),
        entries: [logEntry("worker current"), logEntry("sidecar current", "sidecar")],
        historyState: "older-output-may-remain" as const,
        nextBeforeTimestamp: { nanos: 0, seconds: 10n },
        sources: [
          { sourceLabel: "Worker", sourceName: "worker", state: "available" as const },
          { sourceLabel: "Sidecar", sourceName: "sidecar", state: "available" as const },
        ],
      })
      .mockImplementationOnce(
        () =>
          new Promise<ReadDeploymentLogsResult>((resolve) => {
            resolveOlder = resolve;
          }),
      );
    const user = userEvent.setup();
    render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={{ follow: vi.fn(async function* () {}), read }}
      />,
    );

    const log = await screen.findByRole("log");
    vi.spyOn(log, "getBoundingClientRect").mockReturnValue({ top: 0 } as DOMRect);
    Object.defineProperties(log, {
      clientHeight: { configurable: true, value: 100 },
      scrollHeight: { configurable: true, get: () => (olderRendered ? 1_500 : 1_000) },
      scrollTop: { configurable: true, value: 4, writable: true },
    });

    fireEvent.scroll(log);
    expect(read).toHaveBeenCalledTimes(2);

    await user.click(screen.getByLabelText("Worker"));
    await user.click(screen.getByLabelText("Sidecar"));
    expect(log.querySelectorAll("[data-entry-id]")).toHaveLength(0);
    expect(screen.getByRole("status", { name: "Log output status" })).toHaveTextContent(
      "No selected sources have displayed output.",
    );

    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (
      this: HTMLElement,
    ) {
      return {
        top: this.textContent?.includes("sidecar current") ? (olderRendered ? 80 : 30) : 0,
      } as DOMRect;
    });
    await user.click(screen.getByLabelText("Sidecar"));
    expect(await screen.findByText("sidecar current")).toBeVisible();
    await user.click(screen.getByLabelText("Sidecar"));
    expect(log.querySelectorAll("[data-entry-id]")).toHaveLength(0);
    await user.click(screen.getByLabelText("Sidecar"));
    expect(await screen.findByText("sidecar current")).toBeVisible();

    olderRendered = true;
    resolveOlder?.({
      ...emptyHistory(),
      entries: [{ ...logEntry("sidecar older", "sidecar"), timestamp: { nanos: 0, seconds: 0n } }],
      historyState: "no-older-output-observed",
    });
    await screen.findByText("sidecar older");
    await waitFor(() => expect(log.scrollTop).toBe(54));
    expect(read).toHaveBeenCalledTimes(2);
  });

  it("does not apply a stale anchor after older loading fails", async () => {
    let rejectOlder: ((error: Error) => void) | undefined;
    let loadingFinished = false;
    const read = vi
      .fn()
      .mockResolvedValueOnce({
        ...emptyHistory(),
        entries: [logEntry("current")],
        historyState: "older-output-may-remain" as const,
        nextBeforeTimestamp: { nanos: 0, seconds: 10n },
      })
      .mockImplementationOnce(
        () =>
          new Promise<ReadDeploymentLogsResult>((_resolve, reject) => {
            rejectOlder = reject;
          }),
      );
    render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={{ follow: vi.fn(async function* () {}), read }}
      />,
    );

    const log = await screen.findByRole("log");
    const currentRow = (await screen.findByText("current")).closest("li");
    vi.spyOn(log, "getBoundingClientRect").mockReturnValue({ top: 0 } as DOMRect);
    vi.spyOn(currentRow!, "getBoundingClientRect").mockImplementation(
      () => ({ top: loadingFinished ? 70 : 20 }) as DOMRect,
    );
    Object.defineProperties(log, {
      clientHeight: { configurable: true, value: 100 },
      scrollHeight: { configurable: true, value: 1_000 },
      scrollTop: { configurable: true, value: 4, writable: true },
    });

    fireEvent.scroll(log);
    expect(read).toHaveBeenCalledTimes(2);
    loadingFinished = true;
    rejectOlder?.(new Error("history failed"));

    await screen.findByText("Earlier output could not be loaded.");
    expect(log.scrollTop).toBe(4);
    fireEvent(window, new Event("resize"));
    expect(log.scrollTop).toBe(4);
    expect(read).toHaveBeenCalledTimes(2);
  });

  it("lets an explicit jump cancel pending anchor restoration", async () => {
    let resolveOlder: ((result: ReadDeploymentLogsResult) => void) | undefined;
    let olderRendered = false;
    const read = vi
      .fn()
      .mockResolvedValueOnce({
        ...emptyHistory(),
        entries: [logEntry("current")],
        historyState: "older-output-may-remain" as const,
        nextBeforeTimestamp: { nanos: 0, seconds: 10n },
      })
      .mockImplementationOnce(
        () =>
          new Promise<ReadDeploymentLogsResult>((resolve) => {
            resolveOlder = resolve;
          }),
      );
    const user = userEvent.setup();
    render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={{ follow: vi.fn(async function* () {}), read }}
      />,
    );

    const log = await screen.findByRole("log");
    const currentRow = (await screen.findByText("current")).closest("li");
    vi.spyOn(log, "getBoundingClientRect").mockReturnValue({ top: 0 } as DOMRect);
    vi.spyOn(currentRow!, "getBoundingClientRect").mockImplementation(
      () => ({ top: olderRendered ? 70 : 20 }) as DOMRect,
    );
    Object.defineProperties(log, {
      clientHeight: { configurable: true, value: 100 },
      scrollHeight: { configurable: true, get: () => (olderRendered ? 1_500 : 1_000) },
      scrollTop: { configurable: true, value: 4, writable: true },
    });

    fireEvent.scroll(log);
    await user.click(screen.getByRole("button", { name: "Latest" }));
    expect(log.scrollTop).toBe(1_000);
    expect(log).toHaveFocus();

    olderRendered = true;
    resolveOlder?.({
      ...emptyHistory(),
      entries: [{ ...logEntry("older"), timestamp: { nanos: 0, seconds: 0n } }],
      historyState: "no-older-output-observed",
    });
    await screen.findByText("older");
    await waitFor(() => expect(log.scrollTop).toBe(1_500));
    expect(read).toHaveBeenCalledTimes(2);
  });

  it("invalidates pending restoration when the same deployment closes and reopens", async () => {
    let resolveOlder: ((result: ReadDeploymentLogsResult) => void) | undefined;
    const read = vi
      .fn()
      .mockResolvedValueOnce({
        ...emptyHistory(),
        entries: [logEntry("before close")],
        historyState: "older-output-may-remain" as const,
        nextBeforeTimestamp: { nanos: 0, seconds: 10n },
      })
      .mockImplementationOnce(
        () =>
          new Promise<ReadDeploymentLogsResult>((resolve) => {
            resolveOlder = resolve;
          }),
      )
      .mockResolvedValueOnce({
        ...emptyHistory(),
        entries: [logEntry("after reopen")],
      });
    const operations = { follow: vi.fn(async function* () {}), read };
    const { rerender } = render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={operations}
      />,
    );

    const log = await screen.findByRole("log");
    Object.defineProperties(log, {
      clientHeight: { configurable: true, value: 100 },
      scrollHeight: { configurable: true, value: 1_000 },
      scrollTop: { configurable: true, value: 4, writable: true },
    });
    fireEvent.scroll(log);
    expect(read).toHaveBeenCalledTimes(2);

    rerender(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open={false}
        operations={operations}
      />,
    );
    rerender(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={operations}
      />,
    );
    await screen.findByText("after reopen");
    expect(read).toHaveBeenCalledTimes(3);

    resolveOlder?.({
      ...emptyHistory(),
      entries: [{ ...logEntry("late older"), timestamp: { nanos: 0, seconds: 0n } }],
    });
    await Promise.resolve();
    expect(screen.queryByText("late older")).not.toBeInTheDocument();
    expect(screen.getByText("after reopen")).toBeVisible();
    expect(read).toHaveBeenCalledTimes(3);
  });

  it("fills a short viewport with older output and does not page zero-height layouts", async () => {
    const read = vi
      .fn()
      .mockResolvedValueOnce({
        ...emptyHistory(),
        entries: [logEntry("latest")],
        historyState: "older-output-may-remain" as const,
        nextBeforeTimestamp: { nanos: 0, seconds: 10n },
      })
      .mockResolvedValueOnce({
        ...emptyHistory(),
        entries: [{ ...logEntry("older"), timestamp: { nanos: 0, seconds: 0n } }],
      });
    render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={{ follow: vi.fn(async function* () {}), read }}
      />,
    );

    const log = await screen.findByRole("log");
    expect(read).toHaveBeenCalledTimes(1);
    Object.defineProperties(log, {
      clientHeight: { configurable: true, value: 300 },
      scrollHeight: { configurable: true, value: 200 },
    });
    fireEvent(window, new Event("resize"));

    await screen.findByText("older");
    expect(read).toHaveBeenCalledTimes(2);
  });

  it("recovers from follow setup failure without a continuity warning", async () => {
    vi.useFakeTimers();
    const follow = vi
      .fn()
      .mockImplementationOnce(async function* () {
        await Promise.reject(new Error("setup failed"));
        yield { event: { kind: "entry" as const, entry: logEntry("unreachable") } };
      })
      .mockImplementationOnce(async function* () {
        yield { event: { kind: "entry" as const, entry: logEntry("setup recovered") } };
        await new Promise(() => undefined);
      });
    const operations: DeploymentLogOperations = {
      follow,
      read: vi.fn().mockResolvedValue(emptyHistory()),
    };
    render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={operations}
      />,
    );

    await vi.advanceTimersByTimeAsync(0);
    expect(follow).toHaveBeenCalledTimes(1);
    expect(screen.getByText("Reconnecting to live output…")).toBeVisible();

    await vi.advanceTimersByTimeAsync(250);
    await vi.advanceTimersByTimeAsync(16);
    expect(follow).toHaveBeenCalledTimes(2);
    expect(screen.getByRole("log")).toHaveTextContent("setup recovered");
    expect(screen.queryByText(/Continuity boundary/)).not.toBeInTheDocument();
  });

  it("resumes an interrupted established stream from its latest position", async () => {
    vi.useFakeTimers();
    const follow = vi
      .fn()
      .mockImplementationOnce(async function* () {
        await Promise.resolve();
        yield { event: { kind: "entry" as const, entry: logEntry("before interruption") } };
        throw new Error("stream interrupted");
      })
      .mockImplementationOnce(async function* () {
        yield {
          event: {
            kind: "entry" as const,
            entry: {
              ...logEntry("after interruption"),
              timestamp: { nanos: 0, seconds: 2n },
            },
          },
        };
        await new Promise(() => undefined);
      });
    const operations: DeploymentLogOperations = {
      follow,
      read: vi.fn().mockResolvedValue(emptyHistory()),
    };
    render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={operations}
      />,
    );

    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(250);
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(16);

    expect(follow).toHaveBeenCalledTimes(2);
    expect(follow).toHaveBeenNthCalledWith(
      2,
      {
        deploymentId: "deployment-1",
        resumePositions: [{ latestTimestamp: { nanos: 0, seconds: 1n }, sourceName: "worker" }],
        sourceNames: undefined,
      },
      expect.any(AbortSignal),
    );
    expect(screen.getByRole("log")).toHaveTextContent("before interruption");
    expect(screen.getByText("Reconnected to live output")).toBeVisible();
    expect(screen.queryByText(/Continuity boundary/)).not.toBeInTheDocument();
  });

  it("shows a workload announced after preparation with no new follow call", async () => {
    const follow = vi.fn(async function* () {
      yield {
        event: {
          kind: "source" as const,
          source: { sourceLabel: "Worker", sourceName: "worker", state: "unavailable" as const },
        },
      };
      yield {
        event: { kind: "entry" as const, entry: logEntry("pulling layers", "prepare:image") },
      };
      yield {
        event: {
          kind: "source" as const,
          source: { sourceLabel: "Worker", sourceName: "worker", state: "available" as const },
        },
      };
      yield { event: { kind: "entry" as const, entry: logEntry("workload started") } };
      await new Promise(() => undefined);
    });
    const operations: DeploymentLogOperations = {
      follow,
      read: vi.fn().mockResolvedValue({
        ...emptyHistory(),
        sources: [
          { sourceLabel: "Worker", sourceName: "worker", state: "unavailable" as const },
          { sourceLabel: "Image", sourceName: "prepare:image", state: "available" as const },
        ],
      }),
    };
    renderViewer(operations);

    await waitFor(() => expect(screen.getByRole("log")).toHaveTextContent("workload started"));
    expect(screen.getByRole("log")).toHaveTextContent("pulling layers");
    expect(screen.getByRole("checkbox", { name: "Worker" })).toBeVisible();
    expect(within(screen.getByLabelText("Log viewer footer")).getByText("Live")).toBeVisible();
    expect(screen.queryByText("Reconnecting to live output…")).not.toBeInTheDocument();
    expect(screen.queryByText(/Continuity boundary/)).not.toBeInTheDocument();
    expect(follow).toHaveBeenCalledTimes(1);
    expect(follow).toHaveBeenCalledWith(
      { deploymentId: "deployment-1", resumePositions: [], sourceNames: undefined },
      expect.any(AbortSignal),
    );
  });

  it("keeps the status Live with one follow call when a container ends", async () => {
    vi.useFakeTimers();
    const follow = vi.fn(async function* () {
      yield {
        event: {
          kind: "source" as const,
          source: { sourceLabel: "Worker", sourceName: "worker", state: "ended" as const },
        },
      };
      yield {
        event: { kind: "entry" as const, entry: logEntry("sidecar remains live", "sidecar") },
      };
      await new Promise(() => undefined);
    });
    const operations: DeploymentLogOperations = {
      follow,
      read: vi.fn().mockResolvedValue({
        ...emptyHistory(),
        latestPositions: [{ latestTimestamp: { nanos: 0, seconds: 1n }, sourceName: "worker" }],
        sources: [
          { sourceLabel: "Worker", sourceName: "worker", state: "available" as const },
          { sourceLabel: "Sidecar", sourceName: "sidecar", state: "available" as const },
        ],
      }),
    };
    renderViewer(operations);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(50);
    });
    expect(screen.getByRole("log")).toHaveTextContent("sidecar remains live");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
    expect(follow).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("log")).toHaveTextContent("sidecar remains live");
    expect(within(screen.getByLabelText("Log viewer footer")).getByText("Live")).toBeVisible();
    expect(screen.queryByText("Reconnecting to live output…")).not.toBeInTheDocument();
    expect(screen.getByRole("checkbox", { name: "Worker (ended)" })).toBeVisible();
    expect(screen.getByRole("checkbox", { name: "Sidecar" })).toBeVisible();
  });

  it("reopens a dropped stream once with a resume position for every source", async () => {
    vi.useFakeTimers();
    const follow = vi
      .fn()
      .mockImplementationOnce(async function* () {
        await Promise.resolve();
        yield {
          event: {
            kind: "entry" as const,
            entry: { ...logEntry("worker line"), timestamp: { nanos: 123_456_789, seconds: 5n } },
          },
        };
        yield {
          event: {
            kind: "entry" as const,
            entry: {
              ...logEntry("sidecar line", "sidecar"),
              timestamp: { nanos: 1, seconds: 7n },
            },
          },
        };
        throw deploymentLogError(Code.Unavailable);
      })
      .mockImplementationOnce(() => silentFollow());
    const operations: DeploymentLogOperations = {
      follow,
      read: vi.fn().mockResolvedValue({
        ...emptyHistory(),
        sources: [
          { sourceLabel: "Worker", sourceName: "worker", state: "available" as const },
          { sourceLabel: "Sidecar", sourceName: "sidecar", state: "available" as const },
        ],
      }),
    };
    renderViewer(operations);

    await vi.advanceTimersByTimeAsync(0);
    expect(follow).toHaveBeenCalledTimes(1);
    expect(screen.getByText("Reconnecting to live output…")).toBeVisible();

    await vi.advanceTimersByTimeAsync(249);
    expect(follow).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(follow).toHaveBeenCalledTimes(2);
    expect(follow).toHaveBeenNthCalledWith(
      2,
      {
        deploymentId: "deployment-1",
        resumePositions: [
          { latestTimestamp: { nanos: 123_456_789, seconds: 5n }, sourceName: "worker" },
          { latestTimestamp: { nanos: 1, seconds: 7n }, sourceName: "sidecar" },
        ],
        sourceNames: undefined,
      },
      expect.any(AbortSignal),
    );

    await vi.advanceTimersByTimeAsync(60_000);
    expect(follow).toHaveBeenCalledTimes(2);
  });

  it("backs off between reopen attempts up to a cap", async () => {
    vi.useFakeTimers();
    const follow = vi.fn(() => failingFollow(Code.Unavailable));
    renderViewer({ follow, read: vi.fn().mockResolvedValue(emptyHistory()) });

    await vi.advanceTimersByTimeAsync(250 + 500 + 1_000 + 2_000 + 4_000 + 8_000);
    expect(follow).toHaveBeenCalledTimes(7);
    await vi.advanceTimersByTimeAsync(7_999);
    expect(follow).toHaveBeenCalledTimes(7);
    await vi.advanceTimersByTimeAsync(1);
    expect(follow).toHaveBeenCalledTimes(8);
  });

  it("reopens after a transport failure that carries no status", async () => {
    vi.useFakeTimers();
    const follow = vi.fn(() => failingFollow(Code.Unknown));
    renderViewer({ follow, read: vi.fn().mockResolvedValue(emptyHistory()) });

    await vi.advanceTimersByTimeAsync(0);
    expect(follow).toHaveBeenCalledTimes(1);
    expect(screen.getByText("Reconnecting to live output…")).toBeVisible();
    await vi.advanceTimersByTimeAsync(250);
    expect(follow).toHaveBeenCalledTimes(2);
  });

  it("stops without reopening when the stream ends cleanly", async () => {
    vi.useFakeTimers();
    const follow = vi.fn(() => singleEntryFollow(logEntry("last line")));
    renderViewer({ follow, read: vi.fn().mockResolvedValue(emptyHistory()) });

    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(follow).toHaveBeenCalledTimes(1);
    expect(screen.getByText("Deployment removed.")).toBeVisible();
    expect(screen.getByRole("log")).toHaveTextContent("last line");
    expect(screen.queryByText("Reconnecting to live output…")).not.toBeInTheDocument();
  });

  it("stops without reopening when the deployment is not found", async () => {
    vi.useFakeTimers();
    const follow = vi.fn(() => failingFollow(Code.NotFound));
    renderViewer({ follow, read: vi.fn().mockResolvedValue(emptyHistory()) });

    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(follow).toHaveBeenCalledTimes(1);
    expect(screen.getByText("Deployment removed.")).toBeVisible();
  });

  it("shows an error without looping on other follow errors", async () => {
    vi.useFakeTimers();
    const follow = vi.fn(() => failingFollow(Code.InvalidArgument));
    renderViewer({ follow, read: vi.fn().mockResolvedValue(emptyHistory()) });

    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(follow).toHaveBeenCalledTimes(1);
    expect(screen.getByText("Live output stopped.")).toBeVisible();
    expect(screen.queryByText("Reconnecting to live output…")).not.toBeInTheDocument();
  });

  it("derives each source chip state from the events it has seen", async () => {
    const follow = vi.fn(async function* () {
      yield {
        event: {
          kind: "source" as const,
          source: { sourceLabel: "Exited", sourceName: "exited", state: "ended" as const },
        },
      };
      yield {
        event: {
          kind: "source" as const,
          source: { sourceLabel: "Dropped", sourceName: "dropped", state: "unavailable" as const },
        },
      };
      yield {
        event: {
          kind: "source" as const,
          source: {
            sourceLabel: "Returning",
            sourceName: "returning",
            state: "available" as const,
          },
        },
      };
      await new Promise(() => undefined);
    });
    const operations: DeploymentLogOperations = {
      follow,
      read: vi.fn().mockResolvedValue({
        ...emptyHistory(),
        entries: [logEntry("old line", "returning")],
        sources: [
          { sourceLabel: "Live", sourceName: "live", state: "available" as const },
          { sourceLabel: "Waiting", sourceName: "waiting", state: "unavailable" as const },
          { sourceLabel: "Exited", sourceName: "exited", state: "available" as const },
          { sourceLabel: "Dropped", sourceName: "dropped", state: "available" as const },
          { sourceLabel: "Returning", sourceName: "returning", state: "unavailable" as const },
          { sourceLabel: "Gone", sourceName: "gone", state: "unavailable" as const },
        ],
      }),
    };
    renderViewer(operations);

    await screen.findByRole("checkbox", { name: "Exited (ended)" });
    expect(screen.getByRole("checkbox", { name: "Live" })).toBeVisible();
    expect(screen.getByRole("checkbox", { name: "Waiting (unavailable)" })).toBeVisible();
    expect(screen.getByRole("checkbox", { name: "Dropped (unavailable)" })).toBeVisible();
    // Returned to live: keeps its earlier line
    expect(screen.getByRole("checkbox", { name: "Returning" })).toBeVisible();
    expect(screen.getByRole("log")).toHaveTextContent("old line");
    expect(screen.getByRole("checkbox", { name: "Gone (unavailable)" })).toBeVisible();
    // Source state shows only in the checkbox names, never as a pill next to them
    const sources = screen.getByRole("group", { name: "Log sources" });
    expect(sources).toHaveTextContent("LiveWaitingExitedDroppedReturningGone");
  });

  it("marks a source with history lines that is reported unavailable as unavailable", async () => {
    const operations: DeploymentLogOperations = {
      follow: vi.fn(() => silentFollow()),
      read: vi.fn().mockResolvedValue({
        ...emptyHistory(),
        entries: [logEntry("earlier output")],
        sources: [{ sourceLabel: "Worker", sourceName: "worker", state: "unavailable" as const }],
      }),
    };
    renderViewer(operations);

    expect(await screen.findByRole("checkbox", { name: "Worker (unavailable)" })).toBeVisible();
  });

  it("aborts follow and clears scheduled reconnects when unmounted", async () => {
    vi.useFakeTimers();
    let followSignal: AbortSignal | undefined;
    const follow = vi.fn(async function* (
      _request: { sourceNames?: string[] },
      signal: AbortSignal,
    ) {
      followSignal = signal;
      yield {
        event: {
          kind: "source" as const,
          source: { sourceLabel: "Worker", sourceName: "worker", state: "unavailable" as const },
        },
      };
      await new Promise<void>((resolve) => signal.addEventListener("abort", () => resolve()));
    });
    const operations: DeploymentLogOperations = {
      follow,
      read: vi.fn().mockResolvedValue(emptyHistory()),
    };
    const { unmount } = render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={operations}
      />,
    );

    await vi.advanceTimersByTimeAsync(0);
    expect(follow).toHaveBeenCalledTimes(1);
    expect(followSignal?.aborted).toBe(false);

    unmount();
    expect(followSignal?.aborted).toBe(true);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(follow).toHaveBeenCalledTimes(1);
  });

  it("renders explicit loading, empty, and unavailable output states", async () => {
    const loadingRead = new Promise<ReadDeploymentLogsResult>(() => undefined);
    const { rerender } = render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="loading"
        onOpenChange={() => undefined}
        open
        operations={{ follow: vi.fn(async function* () {}), read: vi.fn(() => loadingRead) }}
      />,
    );

    expect(screen.queryByRole("status", { name: "Log output status" })).not.toBeInTheDocument();

    rerender(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="empty"
        onOpenChange={() => undefined}
        open
        operations={{
          follow: vi.fn(async function* () {}),
          read: vi.fn().mockResolvedValue({
            entries: [],
            historyState: "complete",
            latestPositions: [],
            responseTruncated: false,
            sources: [],
          }),
        }}
      />,
    );
    expect(await screen.findByText("No retained output is available.")).toBeVisible();

    rerender(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="unavailable"
        onOpenChange={() => undefined}
        open
        operations={{
          follow: vi.fn(() => silentFollow()),
          read: vi.fn().mockResolvedValue({
            entries: [],
            historyState: "complete",
            latestPositions: [],
            responseTruncated: false,
            sources: [{ sourceLabel: "Worker", sourceName: "worker", state: "unavailable" }],
          }),
        }}
      />,
    );
    expect(await screen.findByText("Logs are unavailable for this deployment.")).toBeVisible();
    expect(within(screen.getByLabelText("Log viewer footer")).getByText("Live")).toBeVisible();
    expect(screen.queryByText("Reconnecting to live output…")).not.toBeInTheDocument();
    expect(screen.queryByText(/Continuity boundary/)).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Load older" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Latest" })).toBeDisabled();
    expect(screen.queryByRole("button", { name: /wrapping|scrolling/i })).not.toBeInTheDocument();
  });

  it("does not show output from the previous deployment while the next history loads", async () => {
    let resolveSecond: ((value: ReadDeploymentLogsResult) => void) | undefined;
    const operations: DeploymentLogOperations = {
      follow: vi.fn(async function* () {
        await new Promise(() => undefined);
        yield { event: { kind: "entry" as const, entry: logEntry("unreachable") } };
      }),
      read: vi
        .fn()
        .mockResolvedValueOnce({
          entries: [logEntry("first output")],
          historyState: "no-older-output-observed" as const,
          latestPositions: [],
          responseTruncated: false,
          sources: [],
        })
        .mockImplementationOnce(
          () =>
            new Promise<ReadDeploymentLogsResult>((resolve) => {
              resolveSecond = resolve;
            }),
        ),
    };
    const { rerender } = render(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-1"
        onOpenChange={() => undefined}
        open
        operations={operations}
      />,
    );
    await screen.findByText("first output");

    rerender(
      <DeploymentLogViewer
        colorKey="deployment-1"
        name="Deployment one"
        deploymentId="deployment-2"
        onOpenChange={() => undefined}
        open
        operations={operations}
      />,
    );

    expect(screen.queryByText("first output")).not.toBeInTheDocument();
    expect(screen.queryByRole("status", { name: "Log output status" })).not.toBeInTheDocument();
    resolveSecond?.({
      entries: [],
      historyState: "no-older-output-observed",
      latestPositions: [],
      responseTruncated: false,
      sources: [],
    });
  });

  it("keeps its parent panel open while controls change log state", async () => {
    const onPanelOpenChange = vi.fn();
    const operations: DeploymentLogOperations = {
      follow: vi.fn(async function* () {}),
      read: vi.fn().mockResolvedValue({
        entries: [],
        historyState: "complete" as const,
        latestPositions: [],
        responseTruncated: false,
        sources: [],
      }),
    };
    function PanelHarness() {
      const openerRef = useRef<HTMLButtonElement>(null);
      return (
        <>
          <button ref={openerRef} type="button">
            Deployment opener
          </button>
          <InspectorShell
            body={<p>Deployment details</p>}
            floatingLayer={
              <DeploymentLogViewer
                colorKey="deployment-1"
                name="Deployment one"
                deploymentId="deployment-1"
                onOpenChange={() => undefined}
                open
                operations={operations}
              />
            }
            id="deployment-panel"
            onOpenChange={onPanelOpenChange}
            open
            openerRef={openerRef}
            title="Deployment"
          />
        </>
      );
    }

    render(<PanelHarness />);
    const log = await screen.findByRole("log", { name: "Deployment log output" });
    fireEvent.scroll(log);

    expect(screen.getByRole("dialog")).toHaveClass("z-[60]");
    expect(document.querySelector('[data-slot="dialog-overlay"]')).toHaveClass("z-[60]");
    expect(onPanelOpenChange).not.toHaveBeenCalled();
  });

  it("closes on Escape and restores focus to its opener", async () => {
    const operations: DeploymentLogOperations = {
      follow: vi.fn(async function* () {}),
      read: vi.fn().mockResolvedValue({
        entries: [],
        historyState: "complete" as const,
        latestPositions: [],
        responseTruncated: false,
        sources: [],
      }),
    };
    const user = userEvent.setup();

    function ViewerHarness() {
      const [open, setOpen] = useState(false);
      const openerRef = useRef<HTMLButtonElement>(null);
      return (
        <>
          <button
            onClick={(event) => {
              event.currentTarget.blur();
              setOpen(true);
            }}
            ref={openerRef}
            type="button"
          >
            Open logs
          </button>
          {open ? (
            <DeploymentLogViewer
              colorKey="deployment-1"
              name="Deployment one"
              deploymentId="deployment-1"
              onOpenChange={(nextOpen) => setOpen(nextOpen)}
              open
              openerRef={openerRef}
              operations={operations}
            />
          ) : null}
        </>
      );
    }

    render(<ViewerHarness />);
    const opener = screen.getByRole("button", { name: "Open logs" });
    await user.click(opener);
    await screen.findByRole("log", { name: "Deployment log output" });
    await user.keyboard("{Escape}");

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(opener).toHaveFocus());
  });
});
