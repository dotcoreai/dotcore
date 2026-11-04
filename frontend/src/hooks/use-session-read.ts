import { useEffect, useRef, useState } from "react";

import { useManagerSession } from "@/app/resource-watch";
import { createPollTimer, DEFAULT_POLL_RETRY_DELAYS_MS } from "@/lib/poll-timer";
import type { ReadState } from "@/lib/read-state";

export interface SessionReadOptions {
  /**
   * Whether to read; defaults to true. A disabled read keeps its last value, and reads on enable
   * unless it already read in this session.
   */
  enabled?: boolean;
}

/**
 * Reads `read` once per manager session, for data immutable within a manager process such as the
 * catalogs: it reads on mount or enable, again when the resource watch reports a restarted manager,
 * and retries a failed read with backoff, paused while the document is hidden, until one succeeds.
 * The last value stays in view through a failure or a re-read. Memoize `read`; a new function reads
 * again.
 */
export function useSessionRead<T>(
  read: (signal: AbortSignal) => Promise<T>,
  { enabled = true }: SessionReadOptions = {},
): ReadState<T> {
  const { generation } = useManagerSession();
  const [state, setState] = useState<{
    generation: number;
    read: typeof read;
    value: ReadState<T>;
  }>();
  const stateRef = useRef(state);

  useEffect(() => {
    stateRef.current = state;
  });

  useEffect(() => {
    if (!enabled) return;
    // A read that succeeded in this session still holds; only a new session or function reads again.
    const held = stateRef.current;
    if (held?.read === read && held.generation === generation && !held.value.failed) return;

    let disposed = false;
    let done = false;
    let failureCount = 0;
    let controller: AbortController | undefined;

    const timer = createPollTimer(() => void run(), {
      onHidden: () => controller?.abort(),
      onVisible: () => {
        failureCount = 0;
        if (!done) void run();
      },
    });

    const run = async () => {
      if (disposed || document.visibilityState === "hidden") return;

      const current = new AbortController();
      controller = current;
      try {
        const value = await read(current.signal);
        if (current.signal.aborted || disposed) return;
        done = true;
        setState({ generation, read, value: { failed: false, value } });
      } catch {
        if (current.signal.aborted || disposed) return;
        failureCount += 1;
        setState((previous) => ({
          generation,
          read,
          value: {
            failed: true,
            value: previous?.read === read ? previous.value.value : undefined,
          },
        }));
        timer.arm(
          DEFAULT_POLL_RETRY_DELAYS_MS[
            Math.min(failureCount - 1, DEFAULT_POLL_RETRY_DELAYS_MS.length - 1)
          ],
        );
      }
    };

    void run();

    return () => {
      disposed = true;
      controller?.abort();
      timer.dispose();
    };
  }, [enabled, generation, read]);

  return state?.read === read ? state.value : { failed: false };
}
