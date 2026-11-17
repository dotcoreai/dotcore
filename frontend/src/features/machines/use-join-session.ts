import { ConnectError } from "@connectrpc/connect";
import { useCallback, useEffect, useState } from "react";

import type { SiteJoinClient } from "@/api/site-join-client";
import type { JoinState, StartJoinResponse } from "@/gen/dotcore/site/v1/join_pb";

/** What the console knows about one join: its token, its latest state and why it can't continue. */
export interface JoinSession {
  /** The started join; absent while it starts. */
  started?: StartJoinResponse;
  /** The latest state the manager reported. */
  state?: JoinState;
  /** Name of the latest machine that asked for approval, kept to name it once committed. */
  proposalName?: string;
  /** Why the join could not start or the watch broke off. */
  error?: string;
}

/**
 * Starts a join on mount and follows its state until a final one. Unmounting before a final state
 * cancels the join, so closing the dialog never leaves a token usable. `restart` replaces the join
 * with a new one.
 */
export function useJoinSession(client: SiteJoinClient): {
  restart: () => void;
  session: JoinSession;
} {
  const [attempt, setAttempt] = useState(0);
  const [session, setSession] = useState<JoinSession>({});

  useEffect(() => {
    const abort = new AbortController();
    let joinId: string | undefined;
    let ended = false;

    async function follow() {
      try {
        const started = await client.startJoin({}, { signal: abort.signal });
        joinId = started.joinId;
        setSession((current) => ({ ...current, started }));

        for await (const message of client.watchJoin({ joinId }, { signal: abort.signal })) {
          if (message.event.case !== "state") continue;

          const state = message.event.value;
          ended = isFinal(state);
          setSession((current) => ({
            ...current,
            state,
            proposalName:
              state.state.case === "awaitingApproval"
                ? state.state.value.proposal?.displayName
                : current.proposalName,
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
    setSession({});
    setAttempt((current) => current + 1);
  }, []);

  return { restart, session };
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
