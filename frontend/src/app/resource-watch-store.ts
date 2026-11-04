import { create } from "@bufbuild/protobuf";
import { Code } from "@connectrpc/connect";

import type { ResourceWatchClient } from "@/api/resource-watch-client";
import type {
  ManagerSession,
  ResourceKind,
  ResourcePayload,
  ResourceWatchRead,
} from "@/app/resource-watch";
import {
  type ResourceSubscription,
  ResourceSubscriptionSchema,
  type WatchResourcesResponse,
} from "@/gen/dotcore/watch/v1/service_pb";
import { runStreamLoop } from "@/lib/stream-loop";

/**
 * Default wait before reopening the stream after kinds are only removed, so a kind released and
 * registered again in quick succession keeps the open stream.
 */
export const DEFAULT_REOPEN_DELAY_MS = 100;

/**
 * Owns the console's one resource watch stream. Components register the kinds they need; the
 * stream watches the union of registered kinds, opened once they register, reopened right after a
 * kind is added and shortly after kinds are only removed, and the store keeps the latest payload
 * per kind until the kind is no longer watched. Payloads from a new manager session replace everything read from the
 * previous one.
 */
export class ResourceWatchStore {
  private readonly client: ResourceWatchClient;
  private readonly reopenDelayMs: number;
  private readonly registrations = new Map<ResourceKind, Set<symbol>>();
  private readonly reads = new Map<ResourceKind, ResourceWatchRead<unknown>>();
  private readonly listeners = new Set<() => void>();
  private currentSession: ManagerSession = { generation: 0 };
  private stream: { kinds: ResourceKind[]; stop: () => void } | undefined;
  private reopenTimer: ReturnType<typeof setTimeout> | undefined;
  private openQueued = false;

  constructor(client: ResourceWatchClient, reopenDelayMs = DEFAULT_REOPEN_DELAY_MS) {
    this.client = client;
    this.reopenDelayMs = reopenDelayMs;
  }

  /** Watches `kind` until the returned function is called. */
  register(kind: ResourceKind): () => void {
    const token = Symbol(kind);
    const holders = this.registrations.get(kind) ?? new Set();
    holders.add(token);
    this.registrations.set(kind, holders);
    this.scheduleReopen();

    return () => {
      holders.delete(token);
      if (holders.size === 0) this.registrations.delete(kind);
      this.scheduleReopen();
    };
  }

  /** The latest read of `kind`; the same object until it changes. */
  read<Kind extends ResourceKind>(kind: Kind): ResourceWatchRead<ResourcePayload<Kind>> {
    return (this.reads.get(kind) ?? loading) as ResourceWatchRead<ResourcePayload<Kind>>;
  }

  readonly session = (): ManagerSession => this.currentSession;

  readonly subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  /** Stops the stream; a later registration opens it again. */
  close(): void {
    clearTimeout(this.reopenTimer);
    this.reopenTimer = undefined;
    this.stream?.stop();
    this.stream = undefined;
  }

  private scheduleReopen() {
    clearTimeout(this.reopenTimer);
    this.reopenTimer = undefined;
    const stream = this.stream;
    if (!stream || [...this.registrations.keys()].some((kind) => !stream.kinds.includes(kind))) {
      // A newly needed kind loads at once; the components of one render register together, so
      // one reopen follows them.
      if (this.openQueued) return;
      this.openQueued = true;
      queueMicrotask(() => {
        this.openQueued = false;
        this.reopen();
      });
      return;
    }

    this.reopenTimer = setTimeout(() => {
      this.reopenTimer = undefined;
      this.reopen();
    }, this.reopenDelayMs);
  }

  private reopen() {
    const kinds = [...this.registrations.keys()].sort();
    if (kinds.join() === (this.stream?.kinds.join() ?? "")) return;

    this.stream?.stop();
    this.stream = undefined;
    // A kind left unwatched is forgotten, so watching it again never shows a stale read.
    for (const kind of [...this.reads.keys()]) {
      if (!kinds.includes(kind)) this.reads.delete(kind);
    }
    this.notify();
    if (kinds.length === 0) return;

    const subscriptions = kinds.map((kind) =>
      create(ResourceSubscriptionSchema, {
        key: kind,
        kind: { case: kind, value: {} } as ResourceSubscription["kind"],
      }),
    );
    const stop = runStreamLoop<WatchResourcesResponse>({
      onFailure: () => {
        for (const kind of kinds) {
          this.reads.set(kind, { failed: true, value: this.reads.get(kind)?.value });
        }
        this.notify();
      },
      onMessage: (response, markLive) => this.apply(response, markLive),
      open: (signal) => this.client.watchResources({ subscriptions }, { signal }),
      // Every other failure is transient for a watch that resends everything on reopen.
      terminalCodes: [Code.InvalidArgument],
    });
    this.stream = { kinds, stop };
  }

  private apply(response: WatchResourcesResponse, markLive: () => void) {
    const { generation, id } = this.currentSession;
    if (response.managerSessionId && response.managerSessionId !== id) {
      // A restarted manager supersedes every earlier read; kinds load again from its payloads.
      if (id !== undefined) this.reads.clear();
      this.currentSession = {
        generation: id === undefined ? generation : generation + 1,
        id: response.managerSessionId,
      };
      this.notify();
    }

    const { event } = response;
    if (event.case === "snapshotComplete") {
      markLive();
    } else if (event.case === "resource" && event.value.payload.case) {
      this.reads.set(event.value.payload.case, { failed: false, value: event.value.payload.value });
      this.notify();
    }
  }

  private notify() {
    for (const listener of this.listeners) listener();
  }
}

const loading: ResourceWatchRead<never> = { failed: false };
