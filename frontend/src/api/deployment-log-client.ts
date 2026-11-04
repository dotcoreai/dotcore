import { create } from "@bufbuild/protobuf";
import { type Timestamp, TimestampSchema } from "@bufbuild/protobuf/wkt";
import { type CallOptions, Code, ConnectError } from "@connectrpc/connect";

import {
  type DeploymentLogEntry as ApplicationDeploymentLogEntry,
  type DeploymentLogHistoryState as ApplicationDeploymentLogHistoryState,
  type DeploymentLogResumePosition as ApplicationDeploymentLogResumePosition,
  type DeploymentLogSource as ApplicationDeploymentLogSource,
  type DeploymentLogSourceEvent as ApplicationDeploymentLogSourceEvent,
  type DeploymentLogSourceState as ApplicationDeploymentLogSourceState,
  type DeploymentLogStream as ApplicationDeploymentLogStream,
  type FollowDeploymentLogsResponse as ApplicationFollowDeploymentLogsResponse,
  type ReadDeploymentLogsResponse as ApplicationReadDeploymentLogsResponse,
} from "@/gen/dotcore/application/v1/service_pb";
import {
  type DeploymentLogEntry as ModelDeploymentLogEntry,
  type DeploymentLogHistoryState as ModelDeploymentLogHistoryState,
  type DeploymentLogResumePosition as ModelDeploymentLogResumePosition,
  type DeploymentLogSource as ModelDeploymentLogSource,
  type DeploymentLogSourceEvent as ModelDeploymentLogSourceEvent,
  type DeploymentLogSourceState as ModelDeploymentLogSourceState,
  type DeploymentLogStream as ModelDeploymentLogStream,
  type FollowDeploymentLogsResponse as ModelFollowDeploymentLogsResponse,
  type ReadDeploymentLogsResponse as ModelReadDeploymentLogsResponse,
} from "@/gen/dotcore/model/v1/service_pb";

export interface LogTimestamp {
  seconds: bigint;
  nanos: number;
}

export interface DeploymentLogSource {
  sourceName: string;
  sourceLabel: string;
  state: "available" | "unavailable" | "ended" | "unspecified";
}

export interface DeploymentLogEntry {
  sourceName: string;
  sourceLabel: string;
  stream: "stdout" | "stderr" | "unspecified";
  timestamp?: LogTimestamp;
  content: Uint8Array;
  text: string;
  truncated: boolean;
}

export interface DeploymentLogResumePosition {
  sourceName: string;
  latestTimestamp?: LogTimestamp;
}

export interface ReadDeploymentLogsResult {
  sources: DeploymentLogSource[];
  entries: DeploymentLogEntry[];
  nextBeforeTimestamp?: LogTimestamp;
  historyState: "older-output-may-remain" | "no-older-output-observed" | "unspecified";
  responseTruncated: boolean;
  latestPositions: DeploymentLogResumePosition[];
}

export interface FollowDeploymentLogsEvent {
  event:
    { kind: "source"; source: DeploymentLogSource } | { kind: "entry"; entry: DeploymentLogEntry };
}

export interface DeploymentLogReadRequest {
  deploymentId: string;
  sourceNames?: string[];
  pageSize: number;
  beforeTimestamp?: LogTimestamp;
}

export interface DeploymentLogFollowRequest {
  deploymentId: string;
  sourceNames?: string[];
  resumePositions?: DeploymentLogResumePosition[];
}

export interface DeploymentLogOperations {
  read(request: DeploymentLogReadRequest, signal?: AbortSignal): Promise<ReadDeploymentLogsResult>;
  follow(
    request: DeploymentLogFollowRequest,
    signal?: AbortSignal,
  ): AsyncIterable<FollowDeploymentLogsEvent>;
}

export class DeploymentLogError extends Error {
  constructor(
    message: string,
    readonly code: Code,
  ) {
    super(message);
    this.name = "DeploymentLogError";
  }
}

export function createDeploymentLogOperations<
  ReadRequest,
  FollowRequest,
  ReadResponse extends ModelReadDeploymentLogsResponse | ApplicationReadDeploymentLogsResponse,
  FollowResponse extends
    ModelFollowDeploymentLogsResponse | ApplicationFollowDeploymentLogsResponse,
>(
  readRpc: (request: ReadRequest, options?: CallOptions) => Promise<ReadResponse>,
  followRpc: (request: FollowRequest, options?: CallOptions) => AsyncIterable<FollowResponse>,
  makeReadRequest: (request: DeploymentLogReadRequest) => ReadRequest,
  makeFollowRequest: (request: DeploymentLogFollowRequest) => FollowRequest,
): DeploymentLogOperations {
  return {
    read: async (request, signal) => {
      try {
        return normalizeReadResponse(await readRpc(makeReadRequest(request), { signal }));
      } catch (error) {
        throw normalizeDeploymentLogError(error);
      }
    },
    follow: (request, signal) => {
      try {
        return normalizeFollow(followRpc(makeFollowRequest(request), { signal }));
      } catch (error) {
        return failedFollow(normalizeDeploymentLogError(error));
      }
    },
  };
}

function normalizeFollow<
  Response extends ModelFollowDeploymentLogsResponse | ApplicationFollowDeploymentLogsResponse,
>(stream: AsyncIterable<Response>): AsyncIterable<FollowDeploymentLogsEvent> {
  return (async function* () {
    try {
      for await (const response of stream) {
        if (response.event.case === "entry") {
          yield { event: { kind: "entry", entry: normalizeEntry(response.event.value) } };
        } else if (response.event.case === "sourceEvent") {
          yield { event: { kind: "source", source: normalizeSourceEvent(response.event.value) } };
        }
      }
    } catch (error) {
      throw normalizeDeploymentLogError(error);
    }
  })();
}

function failedFollow(error: DeploymentLogError): AsyncIterable<FollowDeploymentLogsEvent> {
  return {
    [Symbol.asyncIterator]: () => ({
      next: () => Promise.reject(error),
    }),
  };
}

function normalizeReadResponse(
  response: ModelReadDeploymentLogsResponse | ApplicationReadDeploymentLogsResponse,
): ReadDeploymentLogsResult {
  return {
    sources: response.sources.map(normalizeSource),
    entries: response.entries.map(normalizeEntry),
    nextBeforeTimestamp: normalizeTimestamp(response.nextBeforeTimestamp),
    historyState: normalizeHistoryState(response.historyState),
    responseTruncated: response.responseTruncated,
    latestPositions: response.latestPositions.map(normalizePosition),
  };
}

function normalizeSource(
  source: ModelDeploymentLogSource | ApplicationDeploymentLogSource,
): DeploymentLogSource {
  return {
    sourceName: source.sourceName,
    sourceLabel: source.sourceLabel,
    state: normalizeSourceState(source.state),
  };
}

function normalizeSourceEvent(
  event: ModelDeploymentLogSourceEvent | ApplicationDeploymentLogSourceEvent,
): DeploymentLogSource {
  return {
    sourceName: event.sourceName,
    sourceLabel: event.sourceLabel,
    state: normalizeSourceState(event.state),
  };
}

function normalizeEntry(
  entry: ModelDeploymentLogEntry | ApplicationDeploymentLogEntry,
): DeploymentLogEntry {
  return {
    sourceName: entry.sourceName,
    sourceLabel: entry.sourceLabel,
    stream: normalizeStream(entry.stream),
    timestamp: normalizeTimestamp(entry.timestamp),
    content: entry.content,
    text: new TextDecoder().decode(entry.content),
    truncated: entry.truncated,
  };
}

function normalizePosition(
  position: ModelDeploymentLogResumePosition | ApplicationDeploymentLogResumePosition,
): DeploymentLogResumePosition {
  return {
    sourceName: position.sourceName,
    latestTimestamp: normalizeTimestamp(position.latestTimestamp),
  };
}

export function toProtoTimestamp(timestamp?: LogTimestamp): Timestamp | undefined {
  return timestamp ? create(TimestampSchema, timestamp) : undefined;
}

function normalizeTimestamp(timestamp?: Timestamp): LogTimestamp | undefined {
  return timestamp ? { seconds: timestamp.seconds, nanos: timestamp.nanos } : undefined;
}

function normalizeHistoryState(
  state: ModelDeploymentLogHistoryState | ApplicationDeploymentLogHistoryState,
): ReadDeploymentLogsResult["historyState"] {
  switch (Number(state)) {
    case 1:
      return "older-output-may-remain";
    case 2:
      return "no-older-output-observed";
    default:
      return "unspecified";
  }
}

function normalizeSourceState(
  state: ModelDeploymentLogSourceState | ApplicationDeploymentLogSourceState,
): DeploymentLogSource["state"] {
  switch (Number(state)) {
    case 1:
      return "available";
    case 2:
      return "unavailable";
    case 3:
      return "ended";
    default:
      return "unspecified";
  }
}

function normalizeStream(
  stream: ModelDeploymentLogStream | ApplicationDeploymentLogStream,
): DeploymentLogEntry["stream"] {
  switch (Number(stream)) {
    case 1:
      return "stdout";
    case 2:
      return "stderr";
    default:
      return "unspecified";
  }
}

function normalizeDeploymentLogError(error: unknown): DeploymentLogError {
  const connectError = ConnectError.from(error);
  return new DeploymentLogError(
    connectError.rawMessage || "Deployment log request failed.",
    connectError.code,
  );
}
