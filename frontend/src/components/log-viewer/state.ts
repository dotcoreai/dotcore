import type { LogEntry, LogHistoryResult, LogSource } from "@/api/log-types";

export const MAX_DISPLAYED_LOG_ROWS = 10_000;

export interface DisplayLogEntry extends LogEntry {
  id: string;
  occurrence: bigint;
}

/** Health of the follow stream; `ended` means the followed subject is gone. */
export type LogConnection =
  "loading" | "following" | "reconnecting" | "reconnected" | "unavailable" | "ended" | "failed";

/** What one source chip shows, derived from the source events the viewer has seen. */
export type SourceChipState = "live" | "not-started" | "ended" | "unavailable";

export interface LogViewerState<Position> {
  /** Sources that were reported available at some point in this session. */
  availableSources: ReadonlySet<string>;
  connection: LogConnection;
  continuityWarning: boolean;
  droppedRows: number;
  entries: DisplayLogEntry[];
  historyState: LogHistoryResult<Position>["historyState"];
  loadingOlder: boolean;
  nextBeforePosition?: Position;
  nextEntryOccurrence: bigint;
  olderLoadFailed: boolean;
  olderPageLoaded: boolean;
  responseTruncated: boolean;
  sessionKey?: string;
  sourceTruncated: boolean;
  sources: LogSource[];
}

export type LogViewerAction<Position> =
  | { type: "reset"; sessionKey: string }
  | { type: "history-loaded"; result: LogHistoryResult<Position>; sourceDrivesConnection?: boolean }
  | { type: "entries-received"; entries: LogEntry[]; droppedRows?: number }
  | { type: "source-updated"; source: LogSource; sourceDrivesConnection?: boolean }
  | { type: "reconnecting"; continuityWarning?: boolean }
  | { type: "reconnected"; continuityWarning?: boolean }
  | { type: "following"; continuityWarning?: boolean }
  | { type: "unavailable" }
  | { type: "stream-ended" }
  | { type: "stream-failed" }
  | { type: "older-loading" }
  | { type: "older-loaded"; result: LogHistoryResult<Position> }
  | { type: "older-failed" };

export function createLogViewerState<Position>(): LogViewerState<Position> {
  return {
    availableSources: new Set(),
    connection: "loading",
    continuityWarning: false,
    droppedRows: 0,
    entries: [],
    historyState: "unspecified",
    loadingOlder: false,
    nextEntryOccurrence: 0n,
    olderLoadFailed: false,
    olderPageLoaded: false,
    responseTruncated: false,
    sourceTruncated: false,
    sources: [],
  };
}

export function logViewerReducer<Position>(
  state: LogViewerState<Position>,
  action: LogViewerAction<Position>,
): LogViewerState<Position> {
  switch (action.type) {
    case "reset":
      return { ...createLogViewerState<Position>(), sessionKey: action.sessionKey };
    case "history-loaded": {
      const { entries, nextOccurrence } = boundedEntries(
        action.result.entries,
        state.nextEntryOccurrence,
      );
      return {
        ...state,
        availableSources: withAvailable(state.availableSources, action.result.sources),
        connection: action.sourceDrivesConnection
          ? connectionForSources(action.result.sources)
          : "following",
        continuityWarning:
          state.continuityWarning || hasSourceState(action.result.sources, "continuity-uncertain"),
        droppedRows: state.droppedRows + Math.max(0, action.result.entries.length - entries.length),
        entries,
        historyState: action.result.historyState,
        nextBeforePosition: action.result.nextBeforePosition,
        nextEntryOccurrence: nextOccurrence,
        responseTruncated: action.result.responseTruncated,
        sourceTruncated: hasSourceState(action.result.sources, "truncated"),
        sources: action.result.sources,
      };
    }
    case "entries-received": {
      const { entries, dropped, nextOccurrence } = appendBounded(
        state.entries,
        action.entries,
        state.nextEntryOccurrence,
      );
      return {
        ...state,
        droppedRows: state.droppedRows + dropped + (action.droppedRows ?? 0),
        entries,
        nextEntryOccurrence: nextOccurrence,
      };
    }
    case "source-updated": {
      const sources = updateSource(state.sources, action.source);
      return {
        ...state,
        availableSources: withAvailable(state.availableSources, [action.source]),
        connection:
          action.sourceDrivesConnection &&
          action.source.state === "unavailable" &&
          sources.every((source) => source.state === "unavailable")
            ? "unavailable"
            : state.connection,
        continuityWarning:
          state.continuityWarning || action.source.state === "continuity-uncertain",
        sourceTruncated: state.sourceTruncated || action.source.state === "truncated",
        sources,
      };
    }
    case "reconnecting":
      return {
        ...state,
        connection: "reconnecting",
        continuityWarning: state.continuityWarning || Boolean(action.continuityWarning),
      };
    case "reconnected":
      return {
        ...state,
        connection: "reconnected",
        continuityWarning: state.continuityWarning || Boolean(action.continuityWarning),
      };
    case "following":
      return {
        ...state,
        connection: "following",
        continuityWarning: state.continuityWarning || Boolean(action.continuityWarning),
      };
    case "unavailable":
      return { ...state, connection: "unavailable" };
    case "stream-ended":
      return { ...state, connection: "ended" };
    case "stream-failed":
      return { ...state, connection: "failed" };
    case "older-loading":
      return { ...state, loadingOlder: true, olderLoadFailed: false };
    case "older-loaded": {
      const { entries, dropped, nextOccurrence } = prependBounded(
        state.entries,
        action.result.entries,
        state.nextEntryOccurrence,
      );
      return {
        ...state,
        continuityWarning:
          state.continuityWarning || hasSourceState(action.result.sources, "continuity-uncertain"),
        droppedRows: state.droppedRows + dropped,
        entries,
        historyState: action.result.historyState,
        loadingOlder: false,
        nextBeforePosition: action.result.nextBeforePosition,
        nextEntryOccurrence: nextOccurrence,
        olderLoadFailed: false,
        olderPageLoaded: true,
        availableSources: withAvailable(state.availableSources, action.result.sources),
        responseTruncated: state.responseTruncated || action.result.responseTruncated,
        sourceTruncated:
          state.sourceTruncated || hasSourceState(action.result.sources, "truncated"),
        sources: mergeSources(state.sources, action.result.sources),
      };
    }
    case "older-failed":
      return { ...state, loadingOlder: false, olderLoadFailed: true };
  }
}

export function sourceChipState(
  source: LogSource,
  availableSources: ReadonlySet<string>,
  sourcesWithLines: ReadonlySet<string>,
): SourceChipState | undefined {
  const hadOutput =
    availableSources.has(source.sourceName) || sourcesWithLines.has(source.sourceName);
  switch (source.state) {
    case "available":
      return "live";
    case "ended":
      return hadOutput ? "ended" : "not-started";
    case "unavailable":
      return hadOutput ? "unavailable" : "not-started";
    default:
      return undefined;
  }
}

export function normalizeLogText(value: string): string {
  // Terminal control sequences can move the cursor or alter presentation; preserve readable text.
  // eslint-disable-next-line no-control-regex
  const withoutEscapeSequences = value.replace(/\x1B\[[0-?]*[ -/]*[@-~]/g, "");
  return withoutEscapeSequences
    .replace(unsafeControlCharactersPattern, "�")
    .replace(/\r\n?/g, "\n");
}

export function formatLogTimestamp(entry: LogEntry): string {
  if (!entry.timestamp) return "Unknown time";
  return new Date(
    Number(entry.timestamp.seconds) * 1_000 + Math.trunc(entry.timestamp.nanos / 1_000_000),
  ).toISOString();
}

const unsafeControlCharactersPattern = new RegExp(
  `[${String.fromCharCode(0)}-${String.fromCharCode(8)}${String.fromCharCode(11)}${String.fromCharCode(12)}${String.fromCharCode(14)}-${String.fromCharCode(31)}${String.fromCharCode(127)}-${String.fromCharCode(159)}]`,
  "g",
);

function withAvailable(current: ReadonlySet<string>, sources: readonly LogSource[]) {
  const available = sources.filter((source) => source.state === "available");
  if (available.every((source) => current.has(source.sourceName))) return current;
  return new Set([...current, ...available.map((source) => source.sourceName)]);
}

function connectionForSources(sources: readonly LogSource[]): LogConnection {
  return sources.length && sources.every((source) => source.state === "unavailable")
    ? "unavailable"
    : "following";
}

function hasSourceState(sources: readonly LogSource[], state: LogSource["state"]): boolean {
  return sources.some((source) => source.state === state);
}

function boundedEntries(entries: readonly LogEntry[], nextOccurrence: bigint) {
  const allocated = allocateEntries(entries, nextOccurrence);
  return {
    entries: allocated.entries.sort(compareEntries).slice(-MAX_DISPLAYED_LOG_ROWS),
    nextOccurrence: allocated.nextOccurrence,
  };
}

function appendBounded(
  current: readonly DisplayLogEntry[],
  incoming: readonly LogEntry[],
  nextOccurrence: bigint,
) {
  const allocated = allocateEntries(incoming, nextOccurrence);
  const entries = [...current, ...allocated.entries].sort(compareEntries);
  const dropped = Math.max(0, entries.length - MAX_DISPLAYED_LOG_ROWS);
  return {
    dropped,
    entries: dropped ? entries.slice(dropped) : entries,
    nextOccurrence: allocated.nextOccurrence,
  };
}

function prependBounded(
  current: readonly DisplayLogEntry[],
  incoming: readonly LogEntry[],
  nextOccurrence: bigint,
) {
  const allocated = allocateEntries(incoming, nextOccurrence);
  const entries = [...allocated.entries, ...current].sort(compareEntries);
  const dropped = Math.max(0, entries.length - MAX_DISPLAYED_LOG_ROWS);
  return {
    dropped,
    entries: dropped ? entries.slice(dropped) : entries,
    nextOccurrence: allocated.nextOccurrence,
  };
}

function compareEntries(left: DisplayLogEntry, right: DisplayLogEntry): number {
  const leftSeconds = left.timestamp?.seconds ?? 0n;
  const rightSeconds = right.timestamp?.seconds ?? 0n;
  if (leftSeconds !== rightSeconds) return leftSeconds < rightSeconds ? -1 : 1;
  const leftNanos = left.timestamp?.nanos ?? 0;
  const rightNanos = right.timestamp?.nanos ?? 0;
  if (leftNanos !== rightNanos) return leftNanos - rightNanos;
  if (left.occurrence === right.occurrence) return 0;
  return left.occurrence < right.occurrence ? -1 : 1;
}

function allocateEntries(entries: readonly LogEntry[], firstOccurrence: bigint) {
  let nextOccurrence = firstOccurrence;
  const allocated = entries.map((entry) => {
    const occurrence = nextOccurrence;
    nextOccurrence += 1n;
    return {
      ...entry,
      id: `log-entry-${occurrence}`,
      occurrence,
      text: normalizeLogText(entry.text),
    };
  });
  return { entries: allocated, nextOccurrence };
}

function updateSource(sources: readonly LogSource[], source: LogSource) {
  return mergeSources(sources, [source]);
}

function mergeSources(current: readonly LogSource[], incoming: readonly LogSource[]) {
  const sources = new Map(current.map((source) => [source.sourceName, source]));
  for (const source of incoming) sources.set(source.sourceName, source);
  return [...sources.values()];
}
