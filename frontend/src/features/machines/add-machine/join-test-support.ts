import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { vi } from "vitest";

import type { SiteJoinClient } from "@/api/site-join-client";
import {
  ApproveJoinResponseSchema,
  CancelJoinResponseSchema,
  FingerprintPatternColor,
  type JoinState,
  JoinStateSchema,
  RejectJoinResponseSchema,
  StartJoinResponseSchema,
  WatchJoinResponseSchema,
} from "@/gen/dotcore/site/v1/membership_pb";

/** How long a fake join's command stays valid from the moment it starts. */
export const joinLifetimeMs = 15 * 60 * 1_000;

/**
 * Golden vector A of the backend's fingerprint pattern derivation: the fingerprint and the pattern
 * the backend derives from it (blue, yellow, magenta; wire values 4, 3, 5).
 */
export const goldenKeyFingerprint =
  "87524a4a3c1f9e0b5d26e8f1a7c4093b6e2d51f8c0a97b34e1d6f2085ab9c7e3";

export const goldenFingerprintPattern = {
  colors: [
    FingerprintPatternColor.BLUE,
    FingerprintPatternColor.YELLOW,
    FingerprintPatternColor.MAGENTA,
  ],
  // prettier-ignore
  cells: [
    1, 0, 2, 0, 1,
    2, 0, 3, 0, 2,
    3, 0, 0, 0, 3,
    1, 3, 3, 3, 1,
    2, 1, 3, 1, 2,
  ],
};

/** A join state the fake manager reports. */
export function joinState(init: MessageInitShape<typeof JoinStateSchema>): JoinState {
  return create(JoinStateSchema, init);
}

/**
 * A fake join management API. Each started join numbers its token and watch from 1; a test reports
 * states with `report` and every call is a spy.
 */
export function fakeSiteJoinClient() {
  const watches: Watch[] = [];

  const client = {
    approveJoin: vi.fn(() => Promise.resolve(create(ApproveJoinResponseSchema))),
    cancelJoin: vi.fn(() => Promise.resolve(create(CancelJoinResponseSchema))),
    rejectJoin: vi.fn(() => Promise.resolve(create(RejectJoinResponseSchema))),
    startJoin: vi.fn(() =>
      Promise.resolve(
        create(StartJoinResponseSchema, {
          expiresAt: timestampFromDate(new Date(Date.now() + joinLifetimeMs)),
          joinId: `join-${watches.length + 1}`,
          token: `token-${watches.length + 1}`,
        }),
      ),
    ),
    watchJoin: vi.fn((_request: unknown, options?: { signal?: AbortSignal }) => {
      const watch = new Watch(options?.signal);
      watches.push(watch);
      return watch;
    }),
  } satisfies Record<keyof SiteJoinClient, unknown>;

  return {
    client: client as unknown as SiteJoinClient,
    spies: client,
    /** Sends a state on the latest watch. */
    report: (init: MessageInitShape<typeof JoinStateSchema>) =>
      watches
        .at(-1)
        ?.push(
          create(WatchJoinResponseSchema, { event: { case: "state", value: joinState(init) } }),
        ),
    /** Ends the latest watch without a final state. */
    end: () => watches.at(-1)?.end(),
  };
}

type Message = ReturnType<typeof create<typeof WatchJoinResponseSchema>>;

// A push-driven stream that stops with the abort signal, like a Connect server stream.
class Watch implements AsyncIterable<Message> {
  private readonly queue: Message[] = [];
  private waiting?: {
    reject: (reason: unknown) => void;
    resolve: (r: IteratorResult<Message>) => void;
  };
  private ended = false;

  constructor(private readonly signal?: AbortSignal) {
    signal?.addEventListener("abort", () => this.waiting?.reject(new Error("aborted")));
  }

  push(message: Message) {
    const waiting = this.waiting;
    if (waiting) {
      this.waiting = undefined;
      waiting.resolve({ done: false, value: message });
    } else {
      this.queue.push(message);
    }
  }

  end() {
    this.ended = true;
    this.waiting?.resolve({ done: true, value: undefined });
    this.waiting = undefined;
  }

  [Symbol.asyncIterator](): AsyncIterator<Message> {
    return {
      next: () => {
        if (this.signal?.aborted) return Promise.reject(new Error("aborted"));
        const queued = this.queue.shift();
        if (queued) return Promise.resolve({ done: false, value: queued });
        if (this.ended) return Promise.resolve({ done: true, value: undefined });
        return new Promise((resolve, reject) => {
          this.waiting = { reject, resolve };
        });
      },
    };
  }
}
