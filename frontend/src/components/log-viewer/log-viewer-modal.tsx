import {
  type ReactNode,
  type RefObject,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import type { LogViewerOperations } from "@/api/log-types";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { useInspectorFloatingLayerRegistration } from "@/components/ui/inspector-shell";
import { StatusBadge } from "@/components/ui/status-badge";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { useWatchPausedWhile } from "@/hooks/use-watch-pause";
import type { StatusProjection } from "@/lib/status-projection";

import {
  formatLogTimestamp,
  type LogConnection,
  MAX_DISPLAYED_LOG_ROWS,
  type SourceChipState,
  sourceChipState,
} from "./state";
import { useLogViewer } from "./use-log-viewer";

const latestThreshold = 16;
const olderThreshold = 48;

export interface LogViewerCopy {
  ariaLabel: string;
  continuityWarning: string;
  /** Footer text once the followed subject is gone and the stream has closed for good. */
  ended?: string;
  empty: string;
  sourceTruncated?: string;
  unavailable: string;
}

export interface LogViewerModalProps<Position> {
  copy: LogViewerCopy;
  onOpenChange: (open: boolean) => void;
  open: boolean;
  openerRef?: RefObject<HTMLElement | null>;
  operations: LogViewerOperations<Position>;
  /** Follows each ended or unavailable source on its own stream; see `useLogViewer`. */
  perSourceReconnect?: boolean;
  sessionKey: string;
  showSourceFilters?: boolean;
  title: ReactNode;
}

export function LogViewerModal<Position>({
  copy,
  onOpenChange,
  open,
  openerRef,
  operations,
  perSourceReconnect = false,
  sessionKey,
  showSourceFilters = false,
  title,
}: LogViewerModalProps<Position>) {
  useInspectorFloatingLayerRegistration(open);
  useWatchPausedWhile(open);
  const viewer = useLogViewer({ enabled: open, operations, perSourceReconnect, sessionKey });
  const [sourceSelection, setSourceSelection] = useState<SourceSelection>({ sessionKey });
  const [latestState, setLatestState] = useState({ sessionKey, value: true });
  const selectedSources =
    sourceSelection.sessionKey === sessionKey ? sourceSelection.sources : undefined;
  const atLatest = latestState.sessionKey === sessionKey ? latestState.value : true;
  const atLatestRef = useRef(true);
  const knownSourceNamesRef = useRef<Set<string>>(new Set());
  const logRef = useRef<HTMLOListElement>(null);
  const pendingRestorationRef = useRef<PendingViewportRestoration | undefined>(undefined);
  const viewportLoadGenerationRef = useRef(0);
  const viewportLoadInFlightRef = useRef(false);
  const visibleEntries = selectedSources
    ? viewer.entries.filter((entry) => selectedSources.has(entry.sourceName))
    : viewer.entries;
  const allSourcesUnavailable =
    Boolean(viewer.sources.length) &&
    viewer.sources.every((source) => source.state === "unavailable");
  // Per-source viewers report a connection problem when every source is gone; the single-stream
  // viewer reports the stream only and shows source state on the chips.
  const connection: LogConnection =
    perSourceReconnect && allSourcesUnavailable ? "unavailable" : viewer.connection;
  const sourcesWithLines = useMemo(
    () => new Set(viewer.entries.map((entry) => entry.sourceName)),
    [viewer.entries],
  );
  const retainedHistoryLoadingVisible = useDelayedLoading(connection === "loading");
  const earlierHistoryLoadingVisible = useDelayedLoading(viewer.loadingOlder);
  const retainedHistoryFeedbackVisible =
    retainedHistoryLoadingVisible && (connection === "loading" || connection === "following");
  const earlierHistoryFeedbackVisible = earlierHistoryLoadingVisible && !viewer.olderLoadFailed;
  const loadOlder = viewer.loadOlder;
  const canLoadOlder =
    viewer.nextBeforePosition !== undefined &&
    !viewer.loadingOlder &&
    viewer.entries.length < MAX_DISPLAYED_LOG_ROWS;

  useLayoutEffect(() => {
    viewportLoadGenerationRef.current += 1;
    viewportLoadInFlightRef.current = false;
    pendingRestorationRef.current = undefined;
    knownSourceNamesRef.current.clear();
    atLatestRef.current = true;
  }, [open, operations, sessionKey]);

  useEffect(() => {
    const knownSourceNames = knownSourceNamesRef.current;
    const newSourceNames = viewer.sources
      .map((source) => source.sourceName)
      .filter((sourceName) => !knownSourceNames.has(sourceName));
    knownSourceNamesRef.current = new Set(viewer.sources.map((source) => source.sourceName));
    if (newSourceNames.length) {
      setSourceSelection((current) => {
        const sources = current.sessionKey === sessionKey ? current.sources : undefined;
        return {
          sessionKey,
          sources: sources ? new Set([...sources, ...newSourceNames]) : sources,
        };
      });
    }
  }, [sessionKey, viewer.sources]);

  const setFollowingLatest = useCallback(
    (next: boolean) => {
      atLatestRef.current = next;
      setLatestState({ sessionKey, value: next });
    },
    [sessionKey],
  );

  const loadOlderForViewport = useCallback(
    async (preserveAnchor: boolean) => {
      const log = logRef.current;
      if (!log || !canLoadOlder || viewportLoadInFlightRef.current) return;
      viewportLoadInFlightRef.current = true;
      const generation = viewportLoadGenerationRef.current;
      if (preserveAnchor) pendingRestorationRef.current = { anchor: captureViewportAnchor(log) };
      const loaded = await loadOlder();
      if (viewportLoadGenerationRef.current !== generation) return;
      viewportLoadInFlightRef.current = false;
      if (!loaded) pendingRestorationRef.current = undefined;
    },
    [canLoadOlder, loadOlder],
  );

  const inspectViewport = useCallback(() => {
    const log = logRef.current;
    if (!log || log.clientHeight <= 0) return;
    const latest = isAtLatest(log);
    if (!pendingRestorationRef.current) setFollowingLatest(latest);
    if (!canLoadOlder) return;
    if (log.scrollHeight <= log.clientHeight + 1) void loadOlderForViewport(false);
    else if (log.scrollTop <= olderThreshold) void loadOlderForViewport(true);
  }, [canLoadOlder, loadOlderForViewport, setFollowingLatest]);

  useLayoutEffect(() => {
    const log = logRef.current;
    if (!log) return;
    const restoration = pendingRestorationRef.current;
    if (restoration && viewer.loadingOlder) {
      if (!restoration.anchor || !findAnchorEntry(log, restoration.anchor.entryId)) {
        restoration.anchor = captureViewportAnchor(log);
      }
      return;
    }
    if (restoration) {
      if (!viewer.olderLoadFailed && restoration.anchor)
        restoreViewportAnchor(log, restoration.anchor);
      pendingRestorationRef.current = undefined;
      return;
    }
    if (atLatestRef.current) log.scrollTop = log.scrollHeight;
  }, [selectedSources, viewer.entries, viewer.loadingOlder, viewer.olderLoadFailed]);

  useEffect(() => {
    inspectViewport();
  }, [inspectViewport, viewer.entries, viewer.nextBeforePosition]);

  useEffect(() => {
    const handleResize = () => {
      const log = logRef.current;
      if (log && atLatestRef.current && !pendingRestorationRef.current)
        log.scrollTop = log.scrollHeight;
      inspectViewport();
    };
    window.addEventListener("resize", handleResize);
    return () => window.removeEventListener("resize", handleResize);
  }, [inspectViewport]);

  const toggleSource = (sourceName: string) => {
    setSourceSelection((current) => {
      const currentSources = current.sessionKey === sessionKey ? current.sources : undefined;
      const next = new Set(currentSources ?? viewer.sources.map((source) => source.sourceName));
      if (next.has(sourceName)) next.delete(sourceName);
      else next.add(sourceName);
      return { sessionKey, sources: next };
    });
  };

  const jumpToLatest = () => {
    const log = logRef.current;
    if (!log) return;
    pendingRestorationRef.current = undefined;
    setFollowingLatest(true);
    log.scrollTop = log.scrollHeight;
    log.focus();
  };

  const presentedEntries = retainedHistoryFeedbackVisible ? [] : visibleEntries;
  const outputStatus = logOutputStatus(
    connection,
    viewer.entries.length,
    visibleEntries.length,
    allSourcesUnavailable,
    copy,
    retainedHistoryFeedbackVisible,
  );

  return (
    <Dialog modal onOpenChange={onOpenChange} open={open}>
      <DialogContent
        finalFocus={openerRef}
        overlayClassName="z-[60]"
        className="inset-0 z-[60] flex h-dvh max-h-none max-w-none translate-x-0 translate-y-0 flex-col gap-0 overflow-hidden rounded-none p-0 sm:top-1/2 sm:left-1/2 sm:h-[min(84dvh,56rem)] sm:w-[min(92vw,72rem)] sm:max-w-none sm:-translate-x-1/2 sm:-translate-y-1/2 sm:rounded-xl"
      >
        <DialogHeader className="shrink-0 gap-1 border-b px-5 py-4 pr-16 sm:pr-12">
          <DialogTitle className="text-[17px] leading-6 font-semibold">{title}</DialogTitle>
        </DialogHeader>

        {showSourceFilters && viewer.sources.length ? (
          <div
            className="flex shrink-0 flex-wrap items-center gap-x-5 gap-y-2 border-b bg-muted/20 px-4 py-3 sm:px-5"
            aria-label="Log viewer toolbar"
          >
            <fieldset
              className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-2 pointer-coarse:gap-y-4"
              aria-label="Log sources"
            >
              <legend className="mr-1 float-left text-xs font-medium text-muted-foreground">
                Sources
              </legend>
              {viewer.sources.map((source) => {
                const chip = perSourceReconnect
                  ? undefined
                  : sourceChipState(source, viewer.availableSources, sourcesWithLines);
                return (
                  <label
                    className="relative inline-flex touch-target items-center gap-1.5 text-xs"
                    key={source.sourceName}
                  >
                    <input
                      aria-label={
                        (chip ? chip === "live" : source.state === "available")
                          ? source.sourceLabel
                          : `${source.sourceLabel} (${chip ? sourceChipStatus[chip].label.toLowerCase() : source.state})`
                      }
                      checked={selectedSources?.has(source.sourceName) ?? true}
                      className="size-3.5 accent-primary"
                      onChange={() => toggleSource(source.sourceName)}
                      type="checkbox"
                    />
                    {source.sourceLabel}
                    {chip ? (
                      <StatusBadge status={sourceChipStatus[chip]} />
                    ) : source.state !== "available" ? (
                      <span className="text-muted-foreground">({source.state})</span>
                    ) : null}
                  </label>
                );
              })}
            </fieldset>
          </div>
        ) : null}

        <div className="flex min-h-0 flex-1 flex-col px-3 pt-3 sm:px-4 sm:pt-4">
          <ViewerNotices
            copy={copy}
            loadingOlderVisible={earlierHistoryFeedbackVisible}
            showContinuityWarning={
              viewer.continuityWarning && (!allSourcesUnavailable || Boolean(viewer.entries.length))
            }
            viewer={viewer}
          />
          <ol
            aria-label={copy.ariaLabel}
            aria-live="off"
            className="min-h-0 flex-1 space-y-1.5 overflow-y-auto overflow-x-hidden rounded-lg border bg-background p-3 font-mono text-[11px] leading-4"
            onScroll={inspectViewport}
            ref={logRef}
            role="log"
            // The output viewport owns keyboard scrolling even though its rows are not interactive.
            // eslint-disable-next-line jsx-a11y/no-noninteractive-tabindex
            tabIndex={0}
          >
            {outputStatus ? (
              <li
                aria-label="Log output status"
                className="flex min-h-full items-center justify-center px-6 py-12 text-center font-sans text-sm text-muted-foreground"
                role="status"
              >
                {outputStatus}
              </li>
            ) : null}
            {presentedEntries.map((entry) => {
              const timestamp = formatLogTimestamp(entry);
              return (
                <li
                  className="whitespace-pre-wrap [overflow-wrap:anywhere]"
                  data-entry-id={entry.id}
                  key={entry.id}
                >
                  <span className="text-muted-foreground">
                    <time dateTime={timestamp} title={timestamp}>
                      {viewerTimestamp(timestamp)}
                    </time>{" "}
                    <span className="font-medium text-foreground">{entry.sourceLabel}</span>{" "}
                    <span>{entry.category}</span>
                  </span>{" "}
                  <span>
                    {entry.text}
                    {entry.truncated ? " [truncated]" : ""}
                  </span>
                </li>
              );
            })}
          </ol>
          <div
            className="flex h-10 shrink-0 items-center justify-between gap-3"
            aria-label="Log viewer footer"
          >
            <span
              aria-live="polite"
              className="inline-flex min-h-5 items-center gap-2 text-xs text-muted-foreground"
            >
              <span
                aria-hidden="true"
                className={`size-2 rounded-full ${connectionTone(connection)}`}
              />
              {connectionLabel(connection, retainedHistoryFeedbackVisible, copy)}
            </span>
            <Button
              disabled={atLatest || !viewer.entries.length}
              onClick={jumpToLatest}
              size="sm"
              type="button"
              variant="outline"
            >
              Latest
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function ViewerNotices<Position>({
  copy,
  loadingOlderVisible,
  showContinuityWarning,
  viewer,
}: {
  copy: LogViewerCopy;
  loadingOlderVisible: boolean;
  showContinuityWarning: boolean;
  viewer: ReturnType<typeof useLogViewer<Position>>;
}) {
  const informational = [
    loadingOlderVisible ? "Loading earlier output…" : undefined,
    viewer.olderLoadFailed ? "Earlier output could not be loaded." : undefined,
    viewer.entries.length >= MAX_DISPLAYED_LOG_ROWS && viewer.nextBeforePosition !== undefined
      ? `Displayed output reached the ${MAX_DISPLAYED_LOG_ROWS.toLocaleString()}-entry limit; earlier output was not loaded.`
      : undefined,
    viewer.droppedRows
      ? `Dropped ${viewer.droppedRows.toLocaleString()} oldest displayed rows.`
      : undefined,
    viewer.responseTruncated
      ? "The history response reached its size limit; earlier output may remain."
      : undefined,
    viewer.sourceTruncated ? copy.sourceTruncated : undefined,
  ].filter((message): message is string => Boolean(message));

  if (!showContinuityWarning && !informational.length) return null;

  return (
    <div aria-live="polite" className="mb-2 space-y-2 text-xs" role="status">
      {showContinuityWarning ? (
        <p className="rounded-md bg-warning px-3 py-2 text-warning-foreground">
          {copy.continuityWarning}
        </p>
      ) : null}
      {informational.length ? (
        <p className="text-muted-foreground">{informational.join(" ")}</p>
      ) : null}
    </div>
  );
}

function logOutputStatus(
  connection: LogConnection,
  entryCount: number,
  visibleEntryCount: number,
  allSourcesUnavailable: boolean,
  copy: LogViewerCopy,
  loadingFeedbackVisible: boolean,
): string | undefined {
  if (loadingFeedbackVisible) return "Loading retained output…";
  if (entryCount && !visibleEntryCount) return "No selected sources have displayed output.";
  if (entryCount) return undefined;
  if (connection === "loading") return undefined;
  if (connection === "unavailable" || allSourcesUnavailable) return copy.unavailable;
  return copy.empty;
}

function connectionTone(connection: LogConnection): string {
  if (connection === "following" || connection === "reconnected") return "bg-success-foreground";
  if (connection === "reconnecting" || connection === "loading") return "bg-warning-foreground";
  return "bg-destructive";
}

function connectionLabel(
  connection: LogConnection,
  loadingFeedbackVisible: boolean,
  copy: LogViewerCopy,
): string {
  if (connection === "unavailable") return "Logs are unavailable.";
  if (connection === "ended") return copy.ended ?? "Log stream ended.";
  if (connection === "failed") return "Live output stopped.";
  if (connection === "reconnecting") return "Reconnecting to live output…";
  if (connection === "reconnected") return "Reconnected to live output";
  if (loadingFeedbackVisible) return "Loading retained output…";
  if (connection === "loading") return "";
  return "Live";
}

const sourceChipStatus: Record<SourceChipState, StatusProjection> = {
  ended: { label: "Ended", tone: "neutral" },
  live: { label: "Live", tone: "success" },
  "not-started": { label: "Not started", tone: "neutral" },
  unavailable: { label: "Unavailable", tone: "warning" },
};

function viewerTimestamp(timestamp: string): string {
  if (timestamp === "Unknown time") return "Unknown";
  return timestamp.slice(11, 23);
}

interface SourceSelection {
  sessionKey: string;
  sources?: ReadonlySet<string>;
}

interface PendingViewportRestoration {
  anchor?: ViewportAnchor;
}

interface ViewportAnchor {
  entryId: string;
  viewportOffset: number;
}

function captureViewportAnchor(log: HTMLOListElement): ViewportAnchor | undefined {
  const viewportTop = log.getBoundingClientRect().top;
  const entries = Array.from(log.querySelectorAll<HTMLElement>("[data-entry-id]"));
  const entry =
    entries.find((candidate) => {
      const bounds = candidate.getBoundingClientRect();
      return Number.isFinite(bounds.bottom)
        ? bounds.bottom > viewportTop
        : bounds.top >= viewportTop;
    }) ?? entries[0];
  if (!entry?.dataset.entryId) return undefined;
  return {
    entryId: entry.dataset.entryId,
    viewportOffset: entry.getBoundingClientRect().top - viewportTop,
  };
}

function restoreViewportAnchor(log: HTMLOListElement, anchor: ViewportAnchor) {
  const entry = findAnchorEntry(log, anchor.entryId);
  if (!entry) return;
  const currentOffset = entry.getBoundingClientRect().top - log.getBoundingClientRect().top;
  log.scrollTop += currentOffset - anchor.viewportOffset;
}

function findAnchorEntry(log: HTMLOListElement, entryId: string) {
  return Array.from(log.querySelectorAll<HTMLElement>("[data-entry-id]")).find(
    (entry) => entry.dataset.entryId === entryId,
  );
}

function isAtLatest(log: HTMLOListElement): boolean {
  return log.scrollHeight - log.scrollTop - log.clientHeight <= latestThreshold;
}
