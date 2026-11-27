import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";
import { vi } from "vitest";

import type { SiteRescanClient } from "@/api/site-rescan-client";
import {
  ApproveRescanResponseSchema,
  CancelRescanResponseSchema,
  RejectRescanResponseSchema,
  type RescanState,
  RescanStateSchema,
  StartRescanResponseSchema,
  WatchRescanResponseSchema,
} from "@/gen/dotcore/site/v1/rescan_pb";
import { ExecutionTargetSchema } from "@/gen/dotcore/site/v1/site_pb";
import { PushStream } from "@/test/push-stream";

/** How long a fake re-scan's invitation stays open from the moment it starts. */
export const rescanLifetimeMs = 15 * 60 * 1_000;

/** A declared execution target, as a re-scan proposal carries it. */
export function target(init: MessageInitShape<typeof ExecutionTargetSchema>) {
  return create(ExecutionTargetSchema, init);
}

/**
 * A fake re-scan management API. Like the manager, it keeps one open re-scan per node: a start is
 * refused until the previous re-scan of that node ended or its cancel was answered, which arrives
 * on a later task. Started re-scans are numbered from 1; a test reports states with `report` and
 * every call is a spy. `holdStarts` leaves starts unanswered until `releaseStarts`.
 */
export function fakeSiteRescanClient() {
  const watches: { rescanId: string; stream: PushStream<Message> }[] = [];
  const open = new Map<string, string>();
  let startedCount = 0;
  let holdStarts = false;
  const heldStarts: (() => void)[] = [];

  const client = {
    approveRescan: vi.fn(() => Promise.resolve(create(ApproveRescanResponseSchema))),
    cancelRescan: vi.fn(
      ({ rescanId }: { rescanId: string }) =>
        new Promise<ReturnType<typeof create<typeof CancelRescanResponseSchema>>>((resolve) =>
          setTimeout(() => {
            open.delete(rescanId);
            resolve(create(CancelRescanResponseSchema));
          }, 0),
        ),
    ),
    rejectRescan: vi.fn(() => Promise.resolve(create(RejectRescanResponseSchema))),
    startRescan: vi.fn<SiteRescanClient["startRescan"]>(({ nodeId = "" }) => {
      if (holdStarts) {
        // The manager creates the re-scan when the start is answered, not when it is asked.
        return new Promise((resolve, reject) => {
          heldStarts.push(() => {
            start(nodeId).then(resolve, reject);
          });
        });
      }
      return start(nodeId);
    }),
    watchRescan: vi.fn(({ rescanId }: { rescanId: string }, options?: { signal?: AbortSignal }) => {
      const stream = new PushStream<Message>(options?.signal);
      watches.push({ rescanId, stream });
      return stream;
    }),
  } satisfies Record<keyof SiteRescanClient, unknown>;

  function start(nodeId: string) {
    if ([...open.values()].includes(nodeId)) {
      return Promise.reject(
        new ConnectError(
          "this node already has an open re-scan; wait for it to end or cancel it",
          Code.FailedPrecondition,
        ),
      );
    }
    startedCount += 1;
    const rescanId = `rescan-${startedCount}`;
    open.set(rescanId, nodeId);
    return Promise.resolve(
      create(StartRescanResponseSchema, {
        expiresAt: timestampFromDate(new Date(Date.now() + rescanLifetimeMs)),
        rescanId,
      }),
    );
  }

  return {
    client: client as unknown as SiteRescanClient,
    spies: client,
    /** Sends a state on the latest watch; a final state ends its re-scan. */
    report: (init: MessageInitShape<typeof RescanStateSchema>) => {
      const watch = watches.at(-1);
      if (!watch) return;
      const reported = state(init);
      if (FINAL_STATES.has(reported.state.case)) open.delete(watch.rescanId);
      watch.stream.push(
        create(WatchRescanResponseSchema, { event: { case: "state", value: reported } }),
      );
    },
    /** Ends the latest watch without a final state. */
    end: () => watches.at(-1)?.stream.end(),
    /** Holds every start unanswered until `releaseStarts`. */
    holdStarts: () => {
      holdStarts = true;
    },
    /** Answers the held starts and stops holding. */
    releaseStarts: () => {
      holdStarts = false;
      heldStarts.splice(0).forEach((answer) => answer());
    },
    /** The re-scans still open, by identity. */
    open: () => [...open.keys()],
  };
}

const FINAL_STATES = new Set<RescanState["state"]["case"]>([
  "cancelled",
  "committed",
  "expired",
  "failed",
  "noOp",
]);

/** A re-scan state the fake manager reports. */
export function state(init: MessageInitShape<typeof RescanStateSchema>): RescanState {
  return create(RescanStateSchema, init);
}

type Message = ReturnType<typeof create<typeof WatchRescanResponseSchema>>;
