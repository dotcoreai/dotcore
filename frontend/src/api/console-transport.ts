import { ConnectError, type Interceptor } from "@connectrpc/connect";
import { createGrpcWebTransport } from "@connectrpc/connect-web";

import { type DeveloperSettings, readDeveloperSettings } from "@/lib/console-preferences";

/** Seams of the latency interceptor; each defaults to the production behavior. */
export interface LatencyDependencies {
  /** Draws a number in [0, 1). */
  random?: () => number;
  /** Returns the current developer settings; called once per request. */
  readSettings?: () => DeveloperSettings;
  /** Waits `delayMs`, rejecting with the signal's reason when it aborts first. */
  sleep?: (delayMs: number, signal: AbortSignal) => Promise<void>;
}

/**
 * Creates the gRPC-Web transport every Console API client shares, so cross-cutting request policy
 * such as synthetic latency has one place to live.
 */
export function createConsoleTransport(baseUrl: string, latency: LatencyDependencies = {}) {
  return createGrpcWebTransport({ baseUrl, interceptors: [createLatencyInterceptor(latency)] });
}

/**
 * Delays each call by a random time within the configured developer range before it is dispatched.
 * Settings are read per call so toggling takes effect without recreating clients. Unary and server
 * stream calls share this path: a stream is delayed once at start and its messages are untouched.
 */
export function createLatencyInterceptor({
  random = Math.random,
  readSettings = readDeveloperSettings,
  sleep = sleepUntilAborted,
}: LatencyDependencies = {}): Interceptor {
  return (next) => async (request) => {
    const { latencyEnabled, latencyMinMs, latencyMaxMs } = readSettings();

    if (latencyEnabled) {
      // The request signal already combines the caller's abort and the call's timeout, so the
      // delay counts against the deadline like real server latency would.
      await sleep(
        latencyMinMs + Math.floor(random() * (latencyMaxMs - latencyMinMs + 1)),
        request.signal,
      );
    }

    return next(request);
  };
}

function sleepUntilAborted(delayMs: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal.aborted) {
      reject(ConnectError.from(signal.reason));
      return;
    }

    const onAbort = () => {
      clearTimeout(timer);
      reject(ConnectError.from(signal.reason));
    };
    const timer = setTimeout(() => {
      signal.removeEventListener("abort", onAbort);
      resolve();
    }, delayMs);

    signal.addEventListener("abort", onAbort, { once: true });
  });
}
