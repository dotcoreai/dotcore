import { ConnectError } from "@connectrpc/connect";
import { useCallback, useEffect, useState } from "react";

import type { SiteMembershipClient } from "@/api/site-membership-client";
import type {
  JoinAttemptOutcome,
  JoinProposal,
  JoinState,
  StartJoinResponse,
} from "@/gen/dotcore/site/v1/membership_pb";

/** What the console knows about one join: its token, its latest state and why it can't continue. */
export interface JoinSession {
  /** The started join; absent while it starts. */
  started?: StartJoinResponse;
  /** When the join started, to size the countdown against its whole lifetime. */
  startedAt?: Date;
  /** The latest state the manager reported. */
  state?: JoinState;
  /** The latest proposal that asked for approval, kept to name the machine once committed. */
  proposal?: JoinProposal;
  /** Every failed or rejected attempt this session saw, oldest first, each once. */
  attempts: JoinAttemptOutcome[];
  /** Why the join could not start or the watch broke off. */
  error?: string;
}

/**
 * Starts a join on mount and follows its state until a final one. Unmounting before a final state
 * cancels the join, so closing the dialog never leaves a token usable. `restart` replaces the join
 * with a new one.
 */
export function useJoinSession(client: SiteMembershipClient): {
  restart: () => void;
  session: JoinSession;
} {
  const [attempt, setAttempt] = useState(0);
  const [session, setSession] = useState<JoinSession>(newSession);

  useEffect(() => {
    const abort = new AbortController();
    let joinId: string | undefined;
    let ended = false;

    async function follow() {
      try {
        const started = await client.startJoin({}, { signal: abort.signal });
        joinId = started.joinId;
        setSession((current) => ({ ...current, started, startedAt: new Date() }));

        for await (const message of client.watchJoin({ joinId }, { signal: abort.signal })) {
          if (message.event.case !== "state") continue;

          const state = message.event.value;
          ended = isFinal(state);
          setSession((current) => ({
            ...current,
            state,
            attempts: withAttempt(current.attempts, state),
            proposal:
              state.state.case === "awaitingApproval"
                ? state.state.value.proposal
                : current.proposal,
          }));
          if (ended) return;
        }

        if (!abort.signal.aborted) {
          setSession((current) => ({ ...current, error: "Lost the connection to the manager." }));
        }
      } catch (error) {
        if (abort.signal.aborted) return;
        setSession((current) => ({ ...current, error: ConnectError.from(error).rawMessage }));
      }
    }

    void follow();

    return () => {
      abort.abort();
      if (joinId && !ended) void client.cancelJoin({ joinId }).catch(() => undefined);
    };
  }, [client, attempt]);

  const restart = useCallback(() => {
    setSession(newSession());
    setAttempt((current) => current + 1);
  }, []);

  return { restart, session };
}

function newSession(): JoinSession {
  return { attempts: [] };
}

// The watch reports only the latest attempt, so the time it carries tells a new attempt from a
// state repeated after another change.
function withAttempt(attempts: JoinAttemptOutcome[], state: JoinState): JoinAttemptOutcome[] {
  const latest = state.state.case === "waiting" ? state.state.value.lastAttempt : undefined;
  if (!latest) return attempts;

  const seen = attempts.some((attempt) => attemptTimeKey(attempt) === attemptTimeKey(latest));
  return seen ? attempts : [...attempts, latest];
}

function attemptTimeKey(attempt: JoinAttemptOutcome): string {
  return `${attempt.at?.seconds ?? 0}:${attempt.at?.nanos ?? 0}`;
}

function isFinal(state: JoinState): boolean {
  switch (state.state.case) {
    case "committed":
    case "cancelled":
    case "expired":
    case "failed":
      return true;
    default:
      return false;
  }
}
