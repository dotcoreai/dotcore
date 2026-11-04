import { useEffect, useRef, useState } from "react";

import { useDelayedLoading } from "@/hooks/use-delayed-loading";

/**
 * Runs one read per request key while a request is given. A result shows only for the key it was
 * asked for, an unchanged key reuses the read in flight so re-running effects ask once, and the
 * last successful value stays in view while the next key loads, so a quick refresh never blanks
 * the figures it is about to replace. A new `revision` reads the same key again in the background:
 * the read stays settled, and a failed refresh keeps the value it would have replaced, marked as
 * such until a refresh succeeds.
 */
export function useKeyedRead<T>(request: KeyedReadRequest<T> | undefined): KeyedRead<T> {
  const [result, setResult] = useState<KeyedReadResult<T>>();
  const inFlightRef = useRef<{ key: string; promise: Promise<T> } | undefined>(undefined);
  // The key names the read, so the effect reads its loader from the latest request without
  // restarting.
  const requestRef = useRef(request);
  const resultRef = useRef(result);
  const key = request?.key;
  const revision = request?.revision;

  useEffect(() => {
    requestRef.current = request;
    resultRef.current = result;
  });

  useEffect(() => {
    const current = requestRef.current;
    if (key === undefined || !current) return;

    let active = true;
    const existing = inFlightRef.current;
    // A settled key reads again for a new revision, keeping its value if that read fails.
    const settled = resultRef.current?.key === key;
    // A read already in flight for the key answers a new revision too.
    const promise = existing?.key === key ? existing.promise : current.load();
    inFlightRef.current = { key, promise };
    void promise
      .then((value) => {
        if (active) setResult({ key, value });
      })
      .catch((failure: unknown) => {
        if (!active) return;
        if (settled) {
          setResult((previous) =>
            previous?.key === key ? { ...previous, refreshFailed: true } : previous,
          );
          return;
        }
        const detail =
          failure instanceof Error ? failure.message : "The service returned no details";
        setResult({
          error: `${current.failureLabel}: ${detail}${/[.!?]$/.test(detail) ? "" : "."}`,
          key,
        });
      })
      .finally(() => {
        if (inFlightRef.current?.promise === promise) inFlightRef.current = undefined;
      });

    return () => {
      active = false;
    };
  }, [key, revision]);

  const current = key !== undefined && result?.key === key ? result : undefined;
  const pending = key !== undefined && !current;
  const loadingVisible = useDelayedLoading(pending);
  const shown = current ?? (result && !result.error ? result : undefined);

  return {
    error: current?.error,
    loadingVisible,
    pending,
    refreshFailed: Boolean(current?.refreshFailed),
    value: shown?.error ? undefined : shown?.value,
  };
}

/** One read, with a key that changes whenever what the read asks for does. */
export interface KeyedReadRequest<T> {
  /** Names the failure in the message that replaces the read's value when it fails. */
  failureLabel: string;
  key: string;
  load: () => Promise<T>;
  /** Reads the same key again when it changes, such as on a poll; absent never refreshes. */
  revision?: number;
}

/**
 * The latest read: its value, or the previous key's last successful value while the read is
 * pending, and the failure message when the read for the current key failed.
 */
export interface KeyedRead<T> {
  error?: string;
  /** Whether pending feedback should show, after the reveal delay and for its minimum time. */
  loadingVisible: boolean;
  pending: boolean;
  /** Whether the latest refresh of the current key failed, so its value may be outdated. */
  refreshFailed: boolean;
  value?: T;
}

interface KeyedReadResult<T> {
  error?: string;
  key: string;
  refreshFailed?: boolean;
  value?: T;
}
