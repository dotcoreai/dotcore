import { Code, ConnectError } from "@connectrpc/connect";

import { createPollTimer } from "@/lib/poll-timer";
import { timing } from "@/lib/timing";

/** Error codes a request would hit again on every reopen, so the loop stops instead of retrying. */
export const DEFAULT_TERMINAL_CODES: readonly Code[] = [Code.InvalidArgument, Code.NotFound];

/** What a stream owner supplies to keep one server stream open. */
export interface StreamLoopOptions<Message> {
  /** Opens the stream; the signal aborts it when the loop closes or stops. */
  open: (signal: AbortSignal) => AsyncIterable<Message>;
  /**
   * Handles one message. Call `markLive` once the stream has delivered its initial state: the
   * retry backoff resets only after a live stream stays open past the longest retry delay.
   */
  onMessage: (message: Message, markLive: () => void) => void;
  /** Called when an open stream ends or fails and the loop will retry or has stopped for good. */
  onFailure: (error: ConnectError | undefined, terminal: boolean) => void;
  /** Codes that stop the loop without retry; defaults to invalid argument and not found. */
  terminalCodes?: readonly Code[];
}

/**
 * Keeps one server stream open: reopens it with the shared poll backoff after it ends or fails,
 * closes it while the document is hidden and opens it again when visible. A terminal error code
 * is reported once and not retried. Returns the function that stops the loop.
 */
export function runStreamLoop<Message>(options: StreamLoopOptions<Message>): () => void {
  const { open, onMessage, onFailure, terminalCodes = DEFAULT_TERMINAL_CODES } = options;
  let disposed = false;
  let terminal = false;
  let failureCount = 0;
  let controller: AbortController | undefined;

  const timer = createPollTimer(() => void connect(), {
    onHidden: () => {
      controller?.abort();
      controller = undefined;
    },
    onVisible: () => {
      failureCount = 0;
      void connect();
    },
  });

  const connect = async () => {
    if (disposed || terminal || document.visibilityState === "hidden") return;

    controller?.abort();
    const current = new AbortController();
    controller = current;
    const delays = timing().pollRetryMs;
    const connectedAt = Date.now();
    let live = false;
    let failure: ConnectError | undefined;

    try {
      for await (const message of open(current.signal)) {
        if (current.signal.aborted) return;

        onMessage(message, () => {
          live = true;
        });
        // A watcher the server keeps dropping must not reconnect every second.
        if (live && Date.now() - connectedAt >= delays[delays.length - 1]) failureCount = 0;
      }
    } catch (error) {
      failure = ConnectError.from(error);
      terminal = terminalCodes.includes(failure.code);
    }
    if (current.signal.aborted || disposed) return;

    onFailure(failure, terminal);
    if (terminal) return;

    failureCount += 1;
    timer.arm(delays[Math.min(failureCount - 1, delays.length - 1)]);
  };

  void connect();

  return () => {
    disposed = true;
    controller?.abort();
    timer.dispose();
  };
}
