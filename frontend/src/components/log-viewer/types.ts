export interface LogTimestamp {
  seconds: bigint;
  nanos: number;
}

export type LogSourceState =
  "available" | "unavailable" | "ended" | "truncated" | "continuity-uncertain" | "unspecified";

export interface LogSource {
  sourceName: string;
  sourceLabel: string;
  state: LogSourceState;
}

export interface LogEntry {
  sourceName: string;
  sourceLabel: string;
  category: string;
  timestamp?: LogTimestamp;
  text: string;
  truncated: boolean;
  /** Opaque source-issued position of this entry, for sources that resume by position. */
  position?: Uint8Array;
}

export interface LogHistoryResult<Position> {
  sources: LogSource[];
  entries: LogEntry[];
  nextBeforePosition?: Position;
  historyState: "older-output-may-remain" | "no-older-output-observed" | "unspecified";
  responseTruncated: boolean;
  latestPosition?: Position;
}

export interface LogFollowEvent {
  event: { kind: "source"; source: LogSource } | { kind: "entry"; entry: LogEntry };
}

export type LogFollowFailure = "gone" | "retry" | "fatal";

export interface LogViewerOperations<Position> {
  read(
    request: { pageSize: number; beforePosition?: Position },
    signal?: AbortSignal,
  ): Promise<LogHistoryResult<Position>>;
  follow(
    request: { resumePosition?: Position; sourceName?: string },
    signal?: AbortSignal,
  ): AsyncIterable<LogFollowEvent>;
  /**
   * Tells the single-stream viewer what a failed follow means: the subject is gone, the stream is
   * worth reopening, or the failure is final. Failures it leaves unclassified are reopened.
   */
  classifyFollowError?(error: unknown): LogFollowFailure;
  advancePosition?(current: Position | undefined, entry: LogEntry): Position | undefined;
}
