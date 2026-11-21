import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { vi } from "vitest";

import type { SiteMembershipClient } from "@/api/site-membership-client";
import {
  ApproveJoinResponseSchema,
  CancelJoinResponseSchema,
  FingerprintPatternColor,
  type JoinState,
  JoinStateSchema,
  RejectJoinResponseSchema,
  RemoveMachineResponseSchema,
  StartJoinResponseSchema,
  WatchJoinResponseSchema,
} from "@/gen/dotcore/site/v1/membership_pb";
import { PushStream } from "@/test/push-stream";

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
export function fakeSiteMembershipClient() {
  const watches: PushStream<Message>[] = [];

  const client = {
    approveJoin: vi.fn(() => Promise.resolve(create(ApproveJoinResponseSchema))),
    cancelJoin: vi.fn(() => Promise.resolve(create(CancelJoinResponseSchema))),
    rejectJoin: vi.fn(() => Promise.resolve(create(RejectJoinResponseSchema))),
    removeMachine: vi.fn<SiteMembershipClient["removeMachine"]>(() =>
      Promise.resolve(create(RemoveMachineResponseSchema)),
    ),
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
      const watch = new PushStream<Message>(options?.signal);
      watches.push(watch);
      return watch;
    }),
  } satisfies Record<keyof SiteMembershipClient, unknown>;

  return {
    client: client as unknown as SiteMembershipClient,
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
