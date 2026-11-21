import { timestampDate } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError } from "@connectrpc/connect";
import { useCallback, useEffect, useRef, useState } from "react";

import type { SiteRescanClient } from "@/api/site-rescan-client";
import type {
  RescanAttemptOutcome,
  RescanProposal,
  RescanState,
  StartRescanResponse,
} from "@/gen/dotcore/site/v1/rescan_pb";

/**
 * What the console knows about one node re-scan: its invitation, the latest state the manager
 * reported, the answers that ended without a replacement, and why it can't continue.
 */
export interface MachineRescanSession {
  /** The started re-scan; absent while it starts. */
  started?: StartRescanResponse;
  /** When the re-scan started, to size the countdown against its whole lifetime. */
  startedAt?: Date;
  /** The latest state the manager reported. */
  state?: RescanState;
  /** The latest proposal that asked for a decision, kept to name the node once the re-scan ends. */
  proposal?: RescanProposal;
  /** Every answer that ended without a replacement, oldest first, each once. */
  attempts: RescanAttemptOutcome[];
  /** Why the re-scan could not start or the watch broke off. */
  error?: string;
}

/**
 * Starts a re-scan invitation for `nodeId` and follows it until a final state. Unmounting before a
 * final state cancels the re-scan, so closing the dialog never leaves an open invitation, and
 * `restart` replaces the re-scan with a new invitation once the previous one is cancelled, because
 * the manager keeps one open re-scan per node. A cancel refused because the node's replacement is
 * being recorded waits for that re-scan to end instead, and a restart that meets it shows the
 * recorded replacement rather than starting again.
 */
export function useMachineRescan(
  client: SiteRescanClient,
  nodeId: string,
): { restart: () => void; session: MachineRescanSession } {
  const [attempt, setAttempt] = useState(0);
  const [session, setSession] = useState<MachineRescanSession>(newSession);
  const ownerRef = useRef<RescanOwner | undefined>(undefined);

  useEffect(() => {
    ownerRef.current ??= new RescanOwner(setSession);
    const owner = ownerRef.current;
    owner.want({ attempt, client, nodeId });
    return () => owner.release();
  }, [attempt, client, nodeId]);

  const restart = useCallback(() => {
    setSession(newSession());
    setAttempt((current) => current + 1);
  }, []);

  return { restart, session };
}

/** The re-scan a component asks for: the node, the client reaching its manager, and its attempt. */
interface RescanRequest {
  attempt: number;
  client: SiteRescanClient;
  nodeId: string;
}

/**
 * Owns the one re-scan a component follows. The component states which re-scan it wants, or none,
 * and the owner converges on that one step at a time: it cancels the re-scan it runs and waits for
 * the manager to confirm before starting the next, so a replacement never meets the previous
 * invitation still open. A request withdrawn and renewed before the owner acts, as React does when
 * it remounts a component in development, leaves the running re-scan untouched.
 */
class RescanOwner {
  private wanted?: RescanRequest;
  private running?: RescanRun;
  /** The request whose re-scan already ended in a recorded replacement, which the session shows. */
  private recorded?: RescanRequest;
  private converging = false;

  constructor(
    private readonly update: (
      update: (current: MachineRescanSession) => MachineRescanSession,
    ) => void,
  ) {}

  /** Asks for the re-scan `request` names, replacing any other. */
  want(request: RescanRequest) {
    this.wanted = request;
    this.converge();
  }

  /** Asks for no re-scan: the running one is cancelled unless it is wanted again first. */
  release() {
    this.wanted = undefined;
    this.recorded = undefined;
    this.converge();
  }

  private converge() {
    if (this.converging) return;
    this.converging = true;
    // React withdraws and renews an effect within one synchronous commit, so acting after the
    // current task's microtasks sees only the request that holds once the commit ends.
    queueMicrotask(() => void this.step());
  }

  // A cancel that met a replacement being recorded ends in it; a dialog still open on the same
  // node shows it instead of inviting another re-scan over it.
  private showRecorded(stopped: RescanRequest, ended: RescanState | undefined) {
    const wanted = this.wanted;
    if (ended?.state.case !== "committed" || !wanted) return;
    if (wanted.client !== stopped.client || wanted.nodeId !== stopped.nodeId) return;

    this.recorded = wanted;
    this.update((current) => ({ ...current, state: ended }));
  }

  private async step() {
    try {
      for (;;) {
        const running = this.running;
        if (running && !sameRequest(running.request, this.wanted)) {
          this.running = undefined;
          const ended = await running.stop();
          this.showRecorded(running.request, ended);
        } else if (!running && this.wanted && !sameRequest(this.recorded, this.wanted)) {
          const request = this.wanted;
          // Another owner, such as a closed dialog's, may still be cancelling this node's re-scan.
          const cancelling = cancelsInFlight.get(request.client)?.get(request.nodeId);
          if (cancelling) {
            await cancelling;
            continue;
          }
          const run = startRescanRun(request, (update) => {
            // A run being replaced reports nothing into the session of the one replacing it.
            if (this.running === run && sameRequest(request, this.wanted)) this.update(update);
          });
          this.running = run;
        } else {
          return;
        }
      }
    } finally {
      this.converging = false;
    }
  }
}

/** One re-scan the owner runs, and how to end it. */
interface RescanRun {
  request: RescanRequest;
  /**
   * Stops following the re-scan and cancels it if it has not ended, once the manager confirms. When
   * the manager refuses because the replacement is being recorded, waits for the re-scan to end and
   * returns its final state.
   */
  stop: () => Promise<RescanState | undefined>;
}

/** Asks the manager for one re-scan and reports its states as they arrive. */
function startRescanRun(
  request: RescanRequest,
  update: (update: (current: MachineRescanSession) => MachineRescanSession) => void,
): RescanRun {
  const { client, nodeId } = request;
  const abort = new AbortController();
  let ended = false;

  // The start is never aborted: an invitation the manager created must be known to be cancelled.
  const started = client.startRescan({ nodeId }).then(
    (response) => response,
    (error: unknown) => {
      if (!abort.signal.aborted) {
        update((current) => ({ ...current, error: ConnectError.from(error).rawMessage }));
      }
      return undefined;
    },
  );

  async function follow() {
    const response = await started;
    if (!response || abort.signal.aborted) return;
    update((current) => ({ ...current, started: response, startedAt: new Date() }));

    try {
      const watch = client.watchRescan({ rescanId: response.rescanId }, { signal: abort.signal });
      for await (const message of watch) {
        if (message.event.case !== "state") continue;

        const state = message.event.value;
        ended = isFinal(state);
        update((current) => ({
          ...current,
          state,
          attempts: withAttempt(current.attempts, state),
          proposal:
            state.state.case === "awaitingApproval" ? state.state.value.proposal : current.proposal,
        }));
        if (ended) return;
      }

      if (!abort.signal.aborted) {
        update((current) => ({ ...current, error: "Lost the connection to the manager." }));
      }
    } catch (error) {
      if (abort.signal.aborted) return;
      update((current) => ({ ...current, error: ConnectError.from(error).rawMessage }));
    }
  }

  void follow();

  return {
    request,
    stop: () => {
      abort.abort();
      const stopping = (async () => {
        const response = await started;
        if (!response || ended) return undefined;
        try {
          await client.cancelRescan({ rescanId: response.rescanId });
        } catch (error) {
          // Any other refusal, such as an already ended re-scan, settles the cancel.
          if (ConnectError.from(error).code === Code.FailedPrecondition) {
            return await awaitFinalState(client, response);
          }
        }
        return undefined;
      })();
      trackCancel(client, nodeId, stopping);
      return stopping;
    },
  };
}

/**
 * Follows a re-scan whose replacement is being recorded to its final state, giving up when its
 * invitation would have expired.
 */
async function awaitFinalState(
  client: SiteRescanClient,
  response: StartRescanResponse,
): Promise<RescanState | undefined> {
  const abort = new AbortController();
  const remainingMs = response.expiresAt
    ? Math.max(0, timestampDate(response.expiresAt).getTime() - Date.now())
    : undefined;
  const timer =
    remainingMs === undefined ? undefined : setTimeout(() => abort.abort(), remainingMs);

  try {
    const watch = client.watchRescan({ rescanId: response.rescanId }, { signal: abort.signal });
    for await (const message of watch) {
      if (message.event.case === "state" && isFinal(message.event.value))
        return message.event.value;
    }
  } catch {
    // A watch that fails or is cut short leaves the cancel settled; the manager decides what next.
  } finally {
    clearTimeout(timer);
    abort.abort();
  }
  return undefined;
}

/** The cancels not yet answered, by client then node, so any owner can wait for one. */
const cancelsInFlight = new WeakMap<SiteRescanClient, Map<string, Promise<unknown>>>();

function trackCancel(client: SiteRescanClient, nodeId: string, cancel: Promise<unknown>) {
  let byNode = cancelsInFlight.get(client);
  if (!byNode) {
    byNode = new Map();
    cancelsInFlight.set(client, byNode);
  }
  byNode.set(nodeId, cancel);
  void cancel.finally(() => {
    if (byNode.get(nodeId) === cancel) byNode.delete(nodeId);
  });
}

function sameRequest(
  running: RescanRequest | undefined,
  wanted: RescanRequest | undefined,
): boolean {
  return (
    running !== undefined &&
    wanted !== undefined &&
    running.attempt === wanted.attempt &&
    running.client === wanted.client &&
    running.nodeId === wanted.nodeId
  );
}

function newSession(): MachineRescanSession {
  return { attempts: [] };
}

// The watch reports only the latest answer, so the time it carries tells a new answer from a state
// repeated after another change.
function withAttempt(attempts: RescanAttemptOutcome[], state: RescanState): RescanAttemptOutcome[] {
  const latest = state.state.case === "waiting" ? state.state.value.lastAttempt : undefined;
  if (!latest) return attempts;

  const seen = attempts.some((attempt) => attemptTimeKey(attempt) === attemptTimeKey(latest));
  return seen ? attempts : [...attempts, latest];
}

function attemptTimeKey(attempt: RescanAttemptOutcome): string {
  return `${attempt.at?.seconds ?? 0}:${attempt.at?.nanos ?? 0}`;
}

function isFinal(state: RescanState): boolean {
  switch (state.state.case) {
    case "committed":
    case "cancelled":
    case "noOp":
    case "expired":
    case "failed":
      return true;
    default:
      return false;
  }
}
