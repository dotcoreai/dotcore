import { create } from "@bufbuild/protobuf";
import { EmptySchema } from "@bufbuild/protobuf/wkt";

import type { ResourceWatchClient } from "@/api/resource-watch-client";
import {
  ListHardwareSnapshotsResponseSchema,
  type MachineHardwareSnapshot,
  MachineHardwareSnapshotSchema,
} from "@/gen/dotcore/site/v1/service_pb";
import {
  type KeyedResource,
  KeyedResourceSchema,
  type WatchResourcesRequest,
  type WatchResourcesResponse,
  WatchResourcesResponseSchema,
} from "@/gen/dotcore/watch/v1/service_pb";
import type { ResourceKind, ResourcePayload } from "@/resource-watch/resource-watch";

/** Reads answering a watched kind, like the owner reads the manager makes each tick. */
export type ResourceSources = {
  [Kind in ResourceKind]?: () => Promise<ResourcePayload<Kind>>;
};

/**
 * The hardware snapshots payload of the manager: one entry per machine id, holding that machine's
 * hardware result.
 */
export function hardwareSnapshotsPayload(current: Record<string, MachineHardwareSnapshot>) {
  return create(ListHardwareSnapshotsResponseSchema, {
    machines: Object.entries(current).map(([machineId, entry]) =>
      create(MachineHardwareSnapshotSchema, { ...entry, machineId }),
    ),
  });
}

/** A current snapshot whose refresh failed, like a machine whose hardware read errored. */
export function failedSnapshot(message: string) {
  return create(MachineHardwareSnapshotSchema, {
    refresh: { outcome: { case: "failed", value: { code: "READ_FAILED", message } } },
  });
}

/**
 * Fake resource watch service: records each opened stream and lets a test push events into the
 * latest one. Pushes are delivered asynchronously, so tests flush pending work before asserting.
 *
 * With `sources`, it also answers like the manager: every open reads each watched kind that has a
 * source and sends it as its read settles, then the snapshot completion once every watched kind has one; `tick` reads
 * them again and sends them, as does every `tickMs` while a stream is open when set. A source that
 * fails ends the stream with its error, as a failed owner read does.
 */
export class FakeResourceWatch {
  readonly requests: WatchResourcesRequest[] = [];
  readonly signals: AbortSignal[] = [];
  sessionId = "session-a";
  private readonly sources: ResourceSources;
  private readonly tickMs: number | undefined;
  private tickSequence = 0n;
  private current: {
    close: (error?: Error) => void;
    push: (response: WatchResourcesResponse) => void;
  } = { close: () => {}, push: () => {} };

  constructor({ sources = {}, tickMs }: { sources?: ResourceSources; tickMs?: number } = {}) {
    this.sources = sources;
    this.tickMs = tickMs;
  }

  client(): ResourceWatchClient {
    return {
      watchResources: (request, options) => {
        const signal = options?.signal ?? new AbortController().signal;
        this.requests.push(request as WatchResourcesRequest);
        this.signals.push(signal);
        const stream = this.open();
        void this.answer(signal, true);
        if (this.tickMs !== undefined) {
          const timer = setInterval(() => void this.answer(signal, false), this.tickMs);
          signal.addEventListener("abort", () => clearInterval(timer));
        }
        return stream;
      },
    };
  }

  /** Reads the sources of the latest stream's kinds again and sends them. */
  async tick() {
    const signal = this.signals.at(-1);
    if (signal) await this.answer(signal, false);
  }

  /** Kinds the open stream watches, in request order; none once the latest stream closed. */
  watchedKinds(): (string | undefined)[] {
    if (this.signals.at(-1)?.aborted ?? true) return [];
    return this.requests.at(-1)?.subscriptions.map(({ kind }) => kind.case) ?? [];
  }

  /** Sends one kind's full read on the latest stream, keyed by its kind. */
  payload<Kind extends ResourceKind>(kind: Kind, value: ResourcePayload<Kind>) {
    this.push({
      case: "resource",
      value: create(KeyedResourceSchema, {
        key: kind,
        payload: { case: kind, value } as KeyedResource["payload"],
      }),
    });
  }

  snapshotComplete() {
    this.push({ case: "snapshotComplete", value: create(EmptySchema) });
  }

  /** Ends the latest stream with `error`, or cleanly without one. */
  close(error?: Error) {
    this.current.close(error);
  }

  private async answer(signal: AbortSignal, opening: boolean) {
    const kinds = (this.requests.at(-1)?.subscriptions ?? []).flatMap(({ kind }) =>
      kind.case ? [kind.case] : [],
    );
    const answered = kinds.filter((kind) => this.sources[kind]);
    if (signal.aborted || answered.length === 0) return;

    try {
      // Each kind is sent as its own read settles, so a slow owner read never holds back the others.
      await Promise.all(
        answered.map(async (kind) => {
          const read = this.sources[kind] as () => Promise<ResourcePayload<typeof kind>>;
          const value = await read();
          if (!signal.aborted && signal === this.signals.at(-1)) this.payload(kind, value);
        }),
      );
      if (signal.aborted || signal !== this.signals.at(-1)) return;
      if (opening && answered.length === kinds.length) this.snapshotComplete();
    } catch (error) {
      if (!signal.aborted) this.close(error instanceof Error ? error : new Error(String(error)));
    }
  }

  private push(event: WatchResourcesResponse["event"]) {
    this.tickSequence += 1n;
    this.current.push(
      create(WatchResourcesResponseSchema, {
        event,
        managerSessionId: this.sessionId,
        tickSequence: this.tickSequence,
      }),
    );
  }

  private open(): AsyncIterable<WatchResourcesResponse> {
    const queue: WatchResourcesResponse[] = [];
    let wake: (() => void) | undefined;
    let closed: { error?: Error } | undefined;
    this.current = {
      close: (error) => {
        closed = { error };
        wake?.();
      },
      push: (response) => {
        queue.push(response);
        wake?.();
      },
    };

    return (async function* () {
      for (;;) {
        const next = queue.shift();
        if (next) {
          yield next;
        } else if (closed) {
          if (closed.error) throw closed.error;
          return;
        } else {
          await new Promise<void>((resolve) => (wake = resolve));
        }
      }
    })();
  }
}
