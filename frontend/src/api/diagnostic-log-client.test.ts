import { create } from "@bufbuild/protobuf";
import { TimestampSchema } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";
import { describe, expect, it, vi } from "vitest";

import type { LogFollowEvent } from "@/components/log-viewer/types";
import {
  BackendDiagnosticEventSchema,
  DiagnosticHistoryState,
  DiagnosticSourceState,
  FollowBackendDiagnosticsResponseSchema,
  MachineDiagnosticEventSchema,
  MachineDiagnosticStateSchema,
  ReadBackendDiagnosticsResponseSchema,
} from "@/gen/dotcore/diagnostic/v1/service_pb";

import { createDiagnosticLogAdapter, DiagnosticLogError } from "./diagnostic-log-client";

type ReadRpc = Parameters<typeof createDiagnosticLogAdapter>[0];
type FollowRpc = Parameters<typeof createDiagnosticLogAdapter>[1];

const manager = create(MachineDiagnosticStateSchema, {
  displayName: "Manager",
  latestPosition: new Uint8Array([3]),
  machineId: "node-manager",
  state: DiagnosticSourceState.AVAILABLE,
});
const worker = create(MachineDiagnosticStateSchema, {
  displayName: "GPU box",
  machineId: "node-worker",
  state: DiagnosticSourceState.UNAVAILABLE,
});

describe("diagnostic log client", () => {
  it("maps the newest page to machine sources, labels, and per-machine positions", async () => {
    const read = vi.fn<ReadRpc>().mockResolvedValue(
      create(ReadBackendDiagnosticsResponseSchema, {
        events: [
          create(MachineDiagnosticEventSchema, {
            event: create(BackendDiagnosticEventSchema, {
              fields: [
                { key: "request_id", value: "abc" },
                { key: "attempt", value: "2" },
                { key: "endpoint", value: "https://private.example.test/v1" },
                { key: "api_key", value: "secret-looking-value" },
                { key: "site_root", value: "/private/site" },
              ],
              level: "WARN",
              message: "retrying",
              position: new Uint8Array([3]),
              target: "dotcore::worker",
              timestamp: create(TimestampSchema, { nanos: 123_000_000, seconds: 10n }),
              truncated: true,
            }),
            machineId: "node-manager",
          }),
        ],
        historyState: DiagnosticHistoryState.OLDER_OUTPUT_MAY_REMAIN,
        machines: [manager, worker],
        nextBefore: {
          positions: [{ machineId: "node-manager", position: new Uint8Array([1, 2]) }],
        },
        responseTruncated: true,
      }),
    );
    const operations = createDiagnosticLogAdapter(read, vi.fn());
    const signal = new AbortController().signal;

    const result = await operations.read({ pageSize: 200 }, signal);

    expect(read).toHaveBeenCalledWith(expect.objectContaining({ pageSize: 200 }), { signal });
    expect(read.mock.calls[0]?.[0].before).toBeUndefined();
    expect(result).toEqual({
      entries: [
        {
          category: "WARN",
          position: new Uint8Array([3]),
          sourceLabel: "Manager",
          sourceName: "node-manager",
          text: "dotcore::worker: retrying request_id=abc attempt=2 endpoint=https://private.example.test/v1 api_key=secret-looking-value site_root=[redacted]",
          timestamp: { nanos: 123_000_000, seconds: 10n },
          truncated: true,
        },
      ],
      historyState: "older-output-may-remain",
      latestPosition: { machines: new Map([["node-manager", new Uint8Array([3])]]) },
      nextBeforePosition: { machines: new Map([["node-manager", new Uint8Array([1, 2])]]) },
      responseTruncated: true,
      sources: [
        { sourceLabel: "Manager", sourceName: "node-manager", state: "available" },
        { sourceLabel: "GPU box", sourceName: "node-worker", state: "unavailable" },
      ],
    });
  });

  it("sends older-page continuations and leaves live machine states to the follow", async () => {
    const read = vi.fn<ReadRpc>().mockResolvedValue(
      create(ReadBackendDiagnosticsResponseSchema, {
        historyState: DiagnosticHistoryState.NO_OLDER_OUTPUT_OBSERVED,
        machines: [worker],
      }),
    );
    const operations = createDiagnosticLogAdapter(read, vi.fn());

    const result = await operations.read({
      beforePosition: { machines: new Map([["node-worker", new Uint8Array([9])]]) },
      pageSize: 200,
    });

    expect(read.mock.calls[0]?.[0].before?.positions).toEqual([
      expect.objectContaining({ machineId: "node-worker", position: new Uint8Array([9]) }),
    ]);
    expect(result.sources).toEqual([]);
    expect(result.latestPosition).toBeUndefined();
    expect(result.nextBeforePosition).toBeUndefined();
    expect(result.historyState).toBe("no-older-output-observed");
  });

  it("follows machines with labels from their states and narrows a single-machine resume", async () => {
    const follow = vi.fn<FollowRpc>(async function* () {
      await Promise.resolve();
      yield create(FollowBackendDiagnosticsResponseSchema, {
        event: { case: "sourceEvent", value: worker },
      });
      yield create(FollowBackendDiagnosticsResponseSchema, {
        event: {
          case: "diagnostic",
          value: create(MachineDiagnosticEventSchema, {
            event: create(BackendDiagnosticEventSchema, {
              level: "INFO",
              message: "ready",
              position: new Uint8Array([5]),
            }),
            machineId: "node-worker",
          }),
        },
      });
      yield create(FollowBackendDiagnosticsResponseSchema, {
        event: {
          case: "sourceEvent",
          value: create(MachineDiagnosticStateSchema, {
            displayName: "Gone",
            machineId: "node-gone",
            state: DiagnosticSourceState.ENDED,
          }),
        },
      });
    });
    const operations = createDiagnosticLogAdapter(vi.fn(), follow);
    const signal = new AbortController().signal;
    const events: LogFollowEvent[] = [];
    const resumePosition = {
      machines: new Map([
        ["node-manager", new Uint8Array([1])],
        ["node-worker", new Uint8Array([4])],
      ]),
    };

    for await (const event of operations.follow(
      { resumePosition, sourceName: "node-worker" },
      signal,
    )) {
      events.push(event);
    }

    const request = follow.mock.calls[0]?.[0];
    expect(request?.machineIds).toEqual(["node-worker"]);
    expect(request?.resumePositions).toEqual([
      expect.objectContaining({ machineId: "node-worker", position: new Uint8Array([4]) }),
    ]);
    expect(follow.mock.calls[0]?.[1]).toEqual({ signal });
    expect(events).toEqual([
      {
        event: {
          kind: "source",
          source: { sourceLabel: "GPU box", sourceName: "node-worker", state: "unavailable" },
        },
      },
      {
        event: {
          entry: {
            category: "INFO",
            position: new Uint8Array([5]),
            sourceLabel: "GPU box",
            sourceName: "node-worker",
            text: "ready",
            timestamp: undefined,
            truncated: false,
          },
          kind: "entry",
        },
      },
      {
        event: {
          kind: "source",
          source: { sourceLabel: "Gone", sourceName: "node-gone", state: "ended" },
        },
      },
    ]);
  });

  it("advances only the position of the entry's machine", () => {
    const operations = createDiagnosticLogAdapter(vi.fn(), vi.fn());
    const current = {
      machines: new Map([
        ["node-manager", new Uint8Array([1])],
        ["node-worker", new Uint8Array([2])],
      ]),
    };
    const entry = {
      category: "INFO",
      sourceLabel: "GPU box",
      sourceName: "node-worker",
      text: "ready",
      truncated: false,
    };

    expect(
      operations.advancePosition?.(current, { ...entry, position: new Uint8Array([7]) }),
    ).toEqual({
      machines: new Map([
        ["node-manager", new Uint8Array([1])],
        ["node-worker", new Uint8Array([7])],
      ]),
    });
    expect(operations.advancePosition?.(current, entry)).toBe(current);
    expect(current.machines.get("node-worker")).toEqual(new Uint8Array([2]));
  });

  it("normalizes transport failures", async () => {
    const operations = createDiagnosticLogAdapter(
      vi.fn().mockRejectedValue(new ConnectError("retained source failed", Code.Unavailable)),
      vi.fn(),
    );

    await expect(operations.read({ pageSize: 200 })).rejects.toEqual(
      expect.objectContaining<Partial<DiagnosticLogError>>({
        code: Code.Unavailable,
        message: "retained source failed",
        name: "DiagnosticLogError",
      }),
    );
  });
});
