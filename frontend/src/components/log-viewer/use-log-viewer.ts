import { useCallback, useEffect, useReducer, useRef } from "react";

import { createLogViewerState, logViewerReducer, MAX_DISPLAYED_LOG_ROWS } from "./state";
import type { LogEntry, LogViewerOperations } from "./types";

const reconnectDelays = [250, 500, 1_000, 2_000, 4_000, 8_000];
export const MAX_PENDING_LOG_ENTRIES = MAX_DISPLAYED_LOG_ROWS;

export interface UseLogViewerOptions<Position> {
  enabled: boolean;
  operations: LogViewerOperations<Position>;
  /**
   * Opens a follow of its own for every source that ends or is unavailable, and retries it, on top
   * of the main stream. Without it the viewer holds one stream and source events only update
   * source state.
   */
  perSourceReconnect?: boolean;
  sessionKey: string;
}

export function useLogViewer<Position>({
  enabled,
  operations,
  perSourceReconnect = false,
  sessionKey,
}: UseLogViewerOptions<Position>) {
  const [state, dispatch] = useReducer(logViewerReducer<Position>, undefined, () =>
    createLogViewerState<Position>(),
  );
  const stateRef = useRef(state);
  const frameRef = useRef<number | undefined>(undefined);
  const pendingEntriesRef = useRef<LogEntry[]>([]);
  const pendingDroppedRef = useRef(0);
  const retryRef = useRef<number | undefined>(undefined);
  const sourceRetriesRef = useRef(new Map<string, number>());
  const sourceRetryAttemptsRef = useRef(new Map<string, number>());
  const olderControllerRef = useRef<AbortController | undefined>(undefined);
  const olderLoadingRef = useRef(false);
  const requestedContinuationsRef = useRef(new Set<Position>());

  stateRef.current = state;

  const flushEntries = useCallback(() => {
    frameRef.current = undefined;
    const entries = pendingEntriesRef.current.splice(0);
    const droppedRows = pendingDroppedRef.current;
    pendingDroppedRef.current = 0;
    if (entries.length || droppedRows) dispatch({ type: "entries-received", entries, droppedRows });
  }, []);

  const queueEntries = useCallback(
    (entries: LogEntry[]) => {
      const pending = pendingEntriesRef.current;
      if (entries.length >= MAX_PENDING_LOG_ENTRIES) {
        pendingDroppedRef.current += pending.length + entries.length - MAX_PENDING_LOG_ENTRIES;
        pendingEntriesRef.current = entries.slice(-MAX_PENDING_LOG_ENTRIES);
      } else {
        const overflow = Math.max(0, pending.length + entries.length - MAX_PENDING_LOG_ENTRIES);
        if (overflow) {
          pending.splice(0, overflow);
          pendingDroppedRef.current += overflow;
        }
        pending.push(...entries);
      }
      if (frameRef.current === undefined) frameRef.current = requestAnimationFrame(flushEntries);
    },
    [flushEntries],
  );

  useEffect(() => {
    if (!enabled || !sessionKey) return;

    const controller = new AbortController();
    let disposed = false;
    let wholeStreamRetryAttempt = 0;
    let resumePosition: Position | undefined;
    // Live single-source follows; a whole-stream reconnect replaces them, so it aborts them.
    const sourceFollows = new Map<string, AbortController>();
    // Sources that were available at some point; one that is not live yet (such as a workload
    // that does not exist during preparation) is retried without reporting an interruption.
    const liveSources = new Set<string>();
    requestedContinuationsRef.current.clear();
    dispatch({ type: "reset", sessionKey });

    const reconnect = () => {
      for (const retry of sourceRetriesRef.current.values()) window.clearTimeout(retry);
      sourceRetriesRef.current.clear();
      for (const sourceFollow of sourceFollows.values()) sourceFollow.abort();
      sourceFollows.clear();
      dispatch({ type: "reconnecting", continuityWarning: perSourceReconnect });
      const delay = reconnectDelays[Math.min(wholeStreamRetryAttempt, reconnectDelays.length - 1)];
      wholeStreamRetryAttempt += 1;
      retryRef.current = window.setTimeout(() => {
        if (!disposed) void startFollow(true);
      }, delay);
    };

    const reconnectSource = (sourceName: string) => {
      if (sourceRetriesRef.current.has(sourceName)) return;
      if (liveSources.has(sourceName)) dispatch({ type: "reconnecting", continuityWarning: true });
      const retryAttempt = sourceRetryAttemptsRef.current.get(sourceName) ?? 0;
      const delay = reconnectDelays[Math.min(retryAttempt, reconnectDelays.length - 1)];
      sourceRetryAttemptsRef.current.set(sourceName, retryAttempt + 1);
      const retry = window.setTimeout(() => {
        sourceRetriesRef.current.delete(sourceName);
        if (!disposed) void follow(sourceName, true);
      }, delay);
      sourceRetriesRef.current.set(sourceName, retry);
    };

    const follow = async (reconnectingSource?: string, reconnecting = false) => {
      const followController = new AbortController();
      const abortFollow = () => followController.abort();
      if (controller.signal.aborted) abortFollow();
      else controller.signal.addEventListener("abort", abortFollow, { once: true });
      if (reconnectingSource) {
        sourceFollows.get(reconnectingSource)?.abort();
        sourceFollows.set(reconnectingSource, followController);
      }
      const interrupted =
        reconnecting && (!reconnectingSource || liveSources.has(reconnectingSource));
      let sawTerminalSource = false;
      try {
        for await (const event of operations.follow(
          { resumePosition, sourceName: reconnectingSource },
          followController.signal,
        )) {
          if (
            event.event.kind === "entry" ||
            (event.event.kind === "source" && event.event.source.state === "available")
          ) {
            dispatch({
              type: interrupted ? "reconnected" : "following",
              continuityWarning: interrupted,
            });
            liveSources.add(
              event.event.kind === "entry"
                ? event.event.entry.sourceName
                : event.event.source.sourceName,
            );
          }
          if (event.event.kind === "entry") {
            wholeStreamRetryAttempt = 0;
            sourceRetryAttemptsRef.current.delete(event.event.entry.sourceName);
            resumePosition =
              operations.advancePosition?.(resumePosition, event.event.entry) ?? resumePosition;
            queueEntries([event.event.entry]);
          } else {
            dispatch({
              type: "source-updated",
              source: event.event.source,
              sourceDrivesConnection: true,
            });
            if (
              event.event.source.state === "ended" ||
              event.event.source.state === "unavailable"
            ) {
              sawTerminalSource = true;
              if (reconnectingSource) {
                followController.abort();
                reconnectSource(event.event.source.sourceName);
                break;
              }
              reconnectSource(event.event.source.sourceName);
            }
          }
        }
        if (!disposed && !sawTerminalSource && !followController.signal.aborted) {
          if (reconnectingSource) reconnectSource(reconnectingSource);
          else reconnect();
        }
      } catch {
        if (!disposed && !followController.signal.aborted) {
          if (reconnectingSource) reconnectSource(reconnectingSource);
          else reconnect();
        }
      } finally {
        controller.signal.removeEventListener("abort", abortFollow);
        if (reconnectingSource && sourceFollows.get(reconnectingSource) === followController)
          sourceFollows.delete(reconnectingSource);
      }
    };

    // Single-stream follow: source events only update source state. The stream ends cleanly when
    // the followed subject is gone, and is reopened with every resume position after a drop.
    const followStream = async (reconnecting: boolean) => {
      const followController = new AbortController();
      const abortFollow = () => followController.abort();
      if (controller.signal.aborted) abortFollow();
      else controller.signal.addEventListener("abort", abortFollow, { once: true });
      try {
        for await (const event of operations.follow({ resumePosition }, followController.signal)) {
          if (
            event.event.kind === "entry" ||
            (event.event.kind === "source" && event.event.source.state === "available")
          ) {
            dispatch({ type: reconnecting ? "reconnected" : "following" });
          }
          if (event.event.kind === "entry") {
            wholeStreamRetryAttempt = 0;
            resumePosition =
              operations.advancePosition?.(resumePosition, event.event.entry) ?? resumePosition;
            queueEntries([event.event.entry]);
          } else {
            dispatch({ type: "source-updated", source: event.event.source });
          }
        }
        if (!disposed && !followController.signal.aborted) dispatch({ type: "stream-ended" });
      } catch (error) {
        if (disposed || followController.signal.aborted) return;
        const failure = operations.classifyFollowError?.(error) ?? "retry";
        if (failure === "retry") reconnect();
        else dispatch({ type: failure === "gone" ? "stream-ended" : "stream-failed" });
      } finally {
        controller.signal.removeEventListener("abort", abortFollow);
      }
    };

    const startFollow = (reconnecting: boolean) =>
      perSourceReconnect ? follow(undefined, reconnecting) : followStream(reconnecting);

    const load = async () => {
      try {
        const result = await operations.read({ pageSize: 200 }, controller.signal);
        if (disposed) return;
        resumePosition = result.latestPosition;
        for (const source of result.sources)
          if (source.state === "available") liveSources.add(source.sourceName);
        dispatch({ type: "history-loaded", result, sourceDrivesConnection: perSourceReconnect });
        void startFollow(false);
      } catch {
        if (!disposed && !controller.signal.aborted) dispatch({ type: "unavailable" });
      }
    };

    void load();
    return () => {
      disposed = true;
      controller.abort();
      if (retryRef.current !== undefined) window.clearTimeout(retryRef.current);
      for (const retry of sourceRetriesRef.current.values()) window.clearTimeout(retry);
      sourceRetriesRef.current.clear();
      sourceRetryAttemptsRef.current.clear();
      olderControllerRef.current?.abort();
      if (frameRef.current !== undefined) cancelAnimationFrame(frameRef.current);
      frameRef.current = undefined;
      pendingEntriesRef.current = [];
      pendingDroppedRef.current = 0;
    };
  }, [enabled, operations, perSourceReconnect, queueEntries, sessionKey]);

  const loadOlder = useCallback(async () => {
    const continuation = stateRef.current.nextBeforePosition;
    if (
      continuation === undefined ||
      stateRef.current.entries.length >= MAX_DISPLAYED_LOG_ROWS ||
      stateRef.current.loadingOlder ||
      olderLoadingRef.current
    )
      return false;
    if (requestedContinuationsRef.current.has(continuation)) return false;
    requestedContinuationsRef.current.add(continuation);
    olderLoadingRef.current = true;
    dispatch({ type: "older-loading" });
    const controller = new AbortController();
    olderControllerRef.current = controller;
    try {
      const result = await operations.read(
        { beforePosition: continuation, pageSize: 200 },
        controller.signal,
      );
      if (!controller.signal.aborted) {
        dispatch({ type: "older-loaded", result });
        return true;
      }
    } catch {
      if (!controller.signal.aborted) dispatch({ type: "older-failed" });
    } finally {
      olderLoadingRef.current = false;
      if (olderControllerRef.current === controller) olderControllerRef.current = undefined;
    }
    return false;
  }, [operations]);

  const visibleState =
    state.sessionKey === sessionKey ? state : { ...createLogViewerState<Position>(), sessionKey };

  return { ...visibleState, loadOlder };
}
