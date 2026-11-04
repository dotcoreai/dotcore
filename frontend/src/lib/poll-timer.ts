/** Backoff sequence after failed polled reads: one, two, then five seconds until it settles. */
export const DEFAULT_POLL_RETRY_DELAYS_MS = [1_000, 2_000, 5_000] as const;

export interface PollTimerOptions {
  /**
   * When true (default), the pending timer is cleared when the document hides and the timer
   * never arms while the document is hidden.
   */
  pauseWhileHidden?: boolean;
  /** Called when the document hides, after the pending timer is cleared. Only with `pauseWhileHidden`. */
  onHidden?: () => void;
  /** Called when the document becomes visible again; the owner decides what to run. */
  onVisible?: () => void;
}

export interface PollTimer {
  /** Arm a one-shot timer that runs `fire` after the delay; replaces any pending timer. */
  arm: (delayMs: number) => void;
  /** Whether a timer is pending. */
  hasPending: () => boolean;
  /** Clear the pending timer, if any. */
  disarm: () => void;
  /** Clear the pending timer, remove the visibility listener, and stop all further activity. */
  dispose: () => void;
}

/**
 * One-shot timer mechanics for repeating reads. Owns the self-arming `setTimeout`, the
 * hidden-document gating, and the `visibilitychange` listener lifecycle, so owners only decide
 * which delay to arm and what to run; they never re-implement the timer bookkeeping. `fire`,
 * `onHidden`, and `onVisible` run at call time, so a closure may reference an effect-local
 * `read` declared after the timer is created.
 */
export function createPollTimer(fire: () => void, options: PollTimerOptions = {}): PollTimer {
  const { pauseWhileHidden = true, onHidden, onVisible } = options;

  let disposed = false;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const clear = () => {
    if (timer !== undefined) {
      clearTimeout(timer);
      timer = undefined;
    }
  };

  const handleVisibilityChange = () => {
    if (document.visibilityState === "hidden") {
      if (pauseWhileHidden) {
        clear();
        onHidden?.();
      }
      return;
    }

    onVisible?.();
  };

  const needsListener = pauseWhileHidden || onHidden !== undefined || onVisible !== undefined;
  if (needsListener) {
    document.addEventListener("visibilitychange", handleVisibilityChange);
  }

  return {
    arm: (delayMs) => {
      clear();
      if (disposed || (pauseWhileHidden && document.visibilityState === "hidden")) return;

      timer = setTimeout(() => {
        timer = undefined;
        fire();
      }, delayMs);
    },
    hasPending: () => timer !== undefined,
    disarm: clear,
    dispose: () => {
      disposed = true;
      clear();
      if (needsListener) {
        document.removeEventListener("visibilitychange", handleVisibilityChange);
      }
    },
  };
}
