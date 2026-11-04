import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { DiagnosticLogOperations } from "@/api/diagnostic-log-client";
import { LOADING_REVEAL_DELAY_MS } from "@/hooks/use-delayed-loading";

import { DiagnosticLogViewer } from "./diagnostic-log-viewer";

const machines = {
  manager: { sourceLabel: "Manager", sourceName: "node-manager" },
  worker: { sourceLabel: "GPU box", sourceName: "node-worker" },
};

type Machine = keyof typeof machines;

const entry = (text: string, seconds = 1n, machine: Machine = "manager") => ({
  ...machines[machine],
  category: "INFO",
  text,
  timestamp: { nanos: 0, seconds },
  truncated: false,
});

const source = (
  machine: Machine,
  state: "available" | "unavailable" | "truncated" | "continuity-uncertain" = "available",
) => ({ ...machines[machine], state });

function history(overrides: Partial<Awaited<ReturnType<DiagnosticLogOperations["read"]>>> = {}) {
  return {
    entries: [],
    historyState: "no-older-output-observed" as const,
    responseTruncated: false,
    sources: [source("manager")],
    ...overrides,
  };
}

describe("DiagnosticLogViewer", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("delays diagnostic retained-history feedback in the toolbar and viewport", async () => {
    vi.useFakeTimers();
    const read = vi.fn(
      () => new Promise<Awaited<ReturnType<DiagnosticLogOperations["read"]>>>(() => undefined),
    );
    const operations: DiagnosticLogOperations = {
      read,
      follow: vi.fn(async function* () {}),
    };

    render(<DiagnosticLogViewer onOpenChange={() => undefined} open operations={operations} />);
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
  });
  it("renders a single machine's history and live logs under the Site logs title", async () => {
    const operations: DiagnosticLogOperations = {
      read: vi.fn().mockResolvedValue(history({ entries: [entry("retained")] })),
      follow: vi.fn(async function* () {
        yield { event: { kind: "entry" as const, entry: entry("live", 2n) } };
        await new Promise(() => undefined);
      }),
    };

    render(<DiagnosticLogViewer onOpenChange={() => undefined} open operations={operations} />);

    expect(screen.getByRole("dialog", { name: "Site logs" })).toBeVisible();
    const log = screen.getByRole("log", { name: "Site log output" });
    expect(await screen.findByText("retained")).toBeVisible();
    expect(log).toHaveTextContent("00:00:01.000 Manager INFO retained");
    await waitFor(() => expect(log).toHaveTextContent("00:00:02.000 Manager INFO live"));
    expect(screen.getByText("Live")).toBeVisible();
    expect(screen.getByRole("checkbox", { name: "Manager" })).toBeChecked();
  });

  it("interleaves machines by time, prefixed by machine, and filters them with checkboxes", async () => {
    const operations: DiagnosticLogOperations = {
      read: vi.fn().mockResolvedValue(
        history({
          entries: [entry("manager first", 1n), entry("worker second", 2n, "worker")],
          sources: [source("manager"), source("worker", "unavailable")],
        }),
      ),
      follow: vi.fn(async function* () {
        yield { event: { kind: "entry" as const, entry: entry("manager third", 3n) } };
        await new Promise(() => undefined);
      }),
    };
    const user = userEvent.setup();

    render(<DiagnosticLogViewer onOpenChange={() => undefined} open operations={operations} />);

    const log = screen.getByRole("log", { name: "Site log output" });
    await waitFor(() =>
      expect(log).toHaveTextContent(
        "00:00:01.000 Manager INFO manager first" +
          "00:00:02.000 GPU box INFO worker second" +
          "00:00:03.000 Manager INFO manager third",
      ),
    );
    const worker = screen.getByRole("checkbox", { name: "GPU box (unavailable)" });
    expect(worker).toBeChecked();
    expect(screen.getByRole("checkbox", { name: "Manager" })).toBeChecked();

    await user.click(worker);
    expect(worker).not.toBeChecked();
    expect(log).not.toHaveTextContent("worker second");
    expect(log).toHaveTextContent("manager third");

    await user.click(worker);
    expect(log).toHaveTextContent("GPU box INFO worker second");
  });

  it("shows loading, unavailable, truncation, and continuity states", async () => {
    const loading: DiagnosticLogOperations = {
      read: vi.fn(
        () => new Promise<Awaited<ReturnType<DiagnosticLogOperations["read"]>>>(() => undefined),
      ),
      follow: vi.fn(async function* () {}),
    };
    const { rerender } = render(
      <DiagnosticLogViewer onOpenChange={() => undefined} open operations={loading} />,
    );
    expect(screen.queryByRole("status", { name: "Log output status" })).not.toBeInTheDocument();

    const unavailable: DiagnosticLogOperations = {
      read: vi.fn().mockResolvedValue(
        history({
          sources: [source("manager", "unavailable")],
        }),
      ),
      follow: vi.fn(async function* () {}),
    };
    rerender(<DiagnosticLogViewer onOpenChange={() => undefined} open operations={unavailable} />);
    expect(await screen.findByText("Site logs are unavailable.")).toBeVisible();

    const failed: DiagnosticLogOperations = {
      read: vi.fn().mockRejectedValue(new Error("transport failed")),
      follow: vi.fn(async function* () {}),
    };
    rerender(<DiagnosticLogViewer onOpenChange={() => undefined} open operations={failed} />);
    expect(await screen.findByText("Site logs are unavailable.")).toBeVisible();

    const boundedStates: DiagnosticLogOperations = {
      read: vi.fn().mockResolvedValue(
        history({
          entries: [entry("partial")],
          responseTruncated: true,
          sources: [source("manager", "truncated")],
        }),
      ),
      follow: vi.fn(async function* () {
        yield {
          event: {
            kind: "source" as const,
            source: source("manager", "continuity-uncertain"),
          },
        };
        await new Promise(() => undefined);
      }),
    };
    rerender(
      <DiagnosticLogViewer onOpenChange={() => undefined} open operations={boundedStates} />,
    );
    expect(await screen.findByText(/history response reached its size limit/)).toBeVisible();
    expect(screen.queryByText(/truncated or malformed diagnostics/)).not.toBeInTheDocument();
    expect(await screen.findByText(/rotation or reconnect can omit or repeat/)).toBeVisible();
  });

  it("reconnects only an unavailable machine while other machines keep following", async () => {
    const requests: { resumePosition?: unknown; sourceName?: string }[] = [];
    let publishManager: (() => void) | undefined;
    const follow = vi.fn(async function* (
      request: { resumePosition?: unknown; sourceName?: string },
      signal?: AbortSignal,
    ) {
      requests.push(request);
      if (!request.sourceName) {
        yield { event: { kind: "source" as const, source: source("worker", "unavailable") } };
        await new Promise<void>((resolve) => {
          publishManager = resolve;
        });
        yield { event: { kind: "entry" as const, entry: entry("manager still live", 3n) } };
      } else {
        yield { event: { kind: "source" as const, source: source("worker") } };
        yield {
          event: { kind: "entry" as const, entry: entry("worker recovered", 4n, "worker") },
        };
      }
      await new Promise<void>((resolve) => signal?.addEventListener("abort", () => resolve()));
    });
    const operations: DiagnosticLogOperations = {
      read: vi.fn().mockResolvedValue(
        history({
          latestPosition: { machines: new Map([["node-worker", new Uint8Array([7])]]) },
          sources: [source("manager"), source("worker")],
        }),
      ),
      follow,
    };

    render(<DiagnosticLogViewer onOpenChange={() => undefined} open operations={operations} />);
    const log = screen.getByRole("log", { name: "Site log output" });
    await waitFor(() => expect(publishManager).toBeDefined());
    publishManager?.();
    await waitFor(() => expect(log).toHaveTextContent("Manager INFO manager still live"));

    await waitFor(() => expect(log).toHaveTextContent("GPU box INFO worker recovered"));
    expect(follow).toHaveBeenCalledTimes(2);
    expect(requests[1]).toEqual({
      resumePosition: { machines: new Map([["node-worker", new Uint8Array([7])]]) },
      sourceName: "node-worker",
    });
    expect(log).toHaveTextContent("Manager INFO manager still live");
    expect(screen.getByRole("checkbox", { name: "GPU box" })).toBeChecked();
  });

  it("stops a machine's own reconnect when the whole stream reconnects", async () => {
    const requests: { sourceName?: string }[] = [];
    const narrowedSignals: AbortSignal[] = [];
    let failWholeStream: (() => void) | undefined;
    const follow = vi.fn(async function* (
      request: { resumePosition?: unknown; sourceName?: string },
      signal?: AbortSignal,
    ) {
      requests.push(request);
      if (request.sourceName) {
        if (signal) narrowedSignals.push(signal);
        yield { event: { kind: "source" as const, source: source("worker") } };
      } else if (requests.length === 1) {
        yield { event: { kind: "source" as const, source: source("worker", "unavailable") } };
        await new Promise<void>((resolve) => {
          failWholeStream = resolve;
        });
        throw new Error("whole stream lost");
      }
      await new Promise<void>((resolve) => signal?.addEventListener("abort", () => resolve()));
    });
    const operations: DiagnosticLogOperations = {
      read: vi.fn().mockResolvedValue(history({ sources: [source("manager"), source("worker")] })),
      follow,
    };

    render(<DiagnosticLogViewer onOpenChange={() => undefined} open operations={operations} />);
    await waitFor(() => expect(narrowedSignals).toHaveLength(1));
    expect(narrowedSignals[0]?.aborted).toBe(false);

    failWholeStream?.();
    await waitFor(() => expect(requests).toHaveLength(3));

    expect(narrowedSignals[0]?.aborted).toBe(true);
    expect(requests[2]?.sourceName).toBeUndefined();
  });

  it("keeps detached live output in place and jumps to latest on request", async () => {
    let publish: (() => void) | undefined;
    const operations: DiagnosticLogOperations = {
      read: vi.fn().mockResolvedValue(history({ entries: [entry("retained")] })),
      follow: vi.fn(async function* () {
        await new Promise<void>((resolve) => {
          publish = resolve;
        });
        yield { event: { kind: "entry" as const, entry: entry("live", 2n) } };
      }),
    };
    const user = userEvent.setup();
    render(<DiagnosticLogViewer onOpenChange={() => undefined} open operations={operations} />);

    const log = await screen.findByRole("log", { name: "Site log output" });
    let scrollTop = 200;
    Object.defineProperties(log, {
      clientHeight: { configurable: true, value: 100 },
      scrollHeight: { configurable: true, value: 1_000 },
      scrollTop: {
        configurable: true,
        get: () => scrollTop,
        set: (value: number) => {
          scrollTop = value;
        },
      },
    });
    fireEvent.scroll(log);
    expect(screen.getByRole("button", { name: "Latest" })).toBeEnabled();

    publish?.();
    await screen.findByText("live");
    expect(scrollTop).toBe(200);

    await user.click(screen.getByRole("button", { name: "Latest" }));
    expect(scrollTop).toBe(1_000);
    expect(log).toHaveFocus();
  });
});
