import { create } from "@bufbuild/protobuf";
import { type CallOptions, Code, ConnectError, createClient } from "@connectrpc/connect";

import { createConsoleTransport } from "@/api/console-transport";
import { resolveManagementOrigin } from "@/api/management-origin";
import type {
  LogEntry,
  LogFollowEvent,
  LogHistoryResult,
  LogSource,
  LogSourceState,
  LogViewerOperations,
} from "@/components/log-viewer/types";
import {
  DiagnosticHistoryContinuationSchema,
  DiagnosticService,
  type DiagnosticSourceState,
  FollowBackendDiagnosticsRequestSchema,
  type FollowBackendDiagnosticsResponse,
  type MachineDiagnosticEvent,
  MachineDiagnosticPositionSchema,
  type MachineDiagnosticState,
  ReadBackendDiagnosticsRequestSchema,
  type ReadBackendDiagnosticsResponse,
} from "@/gen/dotcore/diagnostic/v1/service_pb";

/** Opaque per-machine positions; a machine without one has no older page to read. */
export interface DiagnosticLogPosition {
  machines: ReadonlyMap<string, Uint8Array>;
}

export type DiagnosticLogOperations = LogViewerOperations<DiagnosticLogPosition>;

export class DiagnosticLogError extends Error {
  constructor(
    message: string,
    readonly code: Code,
  ) {
    super(message);
    this.name = "DiagnosticLogError";
  }
}

const diagnosticServerUrl = resolveManagementOrigin(
  import.meta.env.VITE_DOTCORE_MANAGEMENT_ORIGIN,
  {
    allowTestFallback: import.meta.env.MODE === "test",
    allowViteFallback: import.meta.env.DEV || import.meta.env.MODE === "test",
  },
);

export function createDiagnosticLogOperations(
  baseUrl = diagnosticServerUrl,
): DiagnosticLogOperations {
  const client = createClient(DiagnosticService, createConsoleTransport(baseUrl));
  return createDiagnosticLogAdapter(
    (request, options) => client.readBackendDiagnostics(request, options),
    (request, options) => client.followBackendDiagnostics(request, options),
  );
}

export function createDiagnosticLogAdapter(
  readRpc: (
    request: ReturnType<typeof create<typeof ReadBackendDiagnosticsRequestSchema>>,
    options?: CallOptions,
  ) => Promise<ReadBackendDiagnosticsResponse>,
  followRpc: (
    request: ReturnType<typeof create<typeof FollowBackendDiagnosticsRequestSchema>>,
    options?: CallOptions,
  ) => AsyncIterable<FollowBackendDiagnosticsResponse>,
): DiagnosticLogOperations {
  // Follow events name only their machine; labels come from the latest machine states seen.
  const labels = new Map<string, string>();
  return {
    read: async (request, signal) => {
      try {
        const response = await readRpc(
          create(ReadBackendDiagnosticsRequestSchema, {
            before: request.beforePosition
              ? create(DiagnosticHistoryContinuationSchema, {
                  positions: machinePositions(request.beforePosition.machines),
                })
              : undefined,
            pageSize: request.pageSize,
          }),
          { signal },
        );
        return normalizeHistory(response, labels, request.beforePosition !== undefined);
      } catch (error) {
        throw normalizeError(error);
      }
    },
    follow: (request, signal) => {
      const machines = request.resumePosition?.machines ?? new Map<string, Uint8Array>();
      const resume = request.sourceName
        ? new Map([...machines].filter(([machineId]) => machineId === request.sourceName))
        : machines;
      try {
        return normalizeFollow(
          followRpc(
            create(FollowBackendDiagnosticsRequestSchema, {
              machineIds: request.sourceName ? [request.sourceName] : [],
              resumePositions: machinePositions(resume),
            }),
            { signal },
          ),
          labels,
        );
      } catch (error) {
        return failedFollow(normalizeError(error));
      }
    },
    advancePosition: (current, entry) => {
      if (!entry.position) return current;
      const machines = new Map(current?.machines);
      machines.set(entry.sourceName, entry.position);
      return { machines };
    },
  };
}

export const diagnosticLogOperations = createDiagnosticLogOperations();

function normalizeHistory(
  response: ReadBackendDiagnosticsResponse,
  labels: Map<string, string>,
  older: boolean,
): LogHistoryResult<DiagnosticLogPosition> {
  for (const machine of response.machines) labels.set(machine.machineId, machine.displayName);
  const latest = response.machines.flatMap((machine) =>
    machine.latestPosition ? [[machine.machineId, machine.latestPosition] as const] : [],
  );
  return {
    entries: response.events.map((event) => normalizeEvent(event, labels)),
    historyState: normalizeHistoryState(response.historyState),
    latestPosition: older ? undefined : { machines: new Map(latest) },
    nextBeforePosition: response.nextBefore
      ? {
          machines: new Map(
            response.nextBefore.positions.map((position) => [
              position.machineId,
              position.position,
            ]),
          ),
        }
      : undefined,
    responseTruncated: response.responseTruncated,
    // Older pages describe only the retained history read, not the machines' live state.
    sources: older ? [] : response.machines.map(normalizeSource),
  };
}

function normalizeFollow(
  stream: AsyncIterable<FollowBackendDiagnosticsResponse>,
  labels: Map<string, string>,
): AsyncIterable<LogFollowEvent> {
  return (async function* () {
    try {
      for await (const response of stream) {
        if (response.event.case === "diagnostic") {
          yield { event: { kind: "entry", entry: normalizeEvent(response.event.value, labels) } };
        } else if (response.event.case === "sourceEvent") {
          labels.set(response.event.value.machineId, response.event.value.displayName);
          yield { event: { kind: "source", source: normalizeSource(response.event.value) } };
        }
      }
    } catch (error) {
      throw normalizeError(error);
    }
  })();
}

function machinePositions(machines: ReadonlyMap<string, Uint8Array>) {
  return [...machines].map(([machineId, position]) =>
    create(MachineDiagnosticPositionSchema, { machineId, position }),
  );
}

function normalizeEvent(
  { event, machineId }: MachineDiagnosticEvent,
  labels: ReadonlyMap<string, string>,
): LogEntry {
  if (!event) {
    return {
      category: "unknown",
      sourceLabel: labels.get(machineId) ?? machineId,
      sourceName: machineId,
      text: "",
      truncated: true,
    };
  }
  const fields = event.fields
    .map((field) => `${field.key}=${isFilesystemField(field.key) ? "[redacted]" : field.value}`)
    .join(" ");
  const target = event.target ? `${event.target}: ` : "";
  return {
    category: event.level || "unknown",
    position: event.position.length ? event.position : undefined,
    sourceLabel: labels.get(machineId) ?? machineId,
    sourceName: machineId,
    text: `${target}${event.message}${fields ? ` ${fields}` : ""}`,
    timestamp: event.timestamp
      ? { nanos: event.timestamp.nanos, seconds: event.timestamp.seconds }
      : undefined,
    truncated: event.truncated,
  };
}

function isFilesystemField(key: string): boolean {
  return key
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .some((segment) => ["dir", "directory", "file", "path", "root"].includes(segment));
}

function normalizeSource(machine: MachineDiagnosticState): LogSource {
  return {
    sourceLabel: machine.displayName || machine.machineId,
    sourceName: machine.machineId,
    state: normalizeSourceState(machine.state),
  };
}

function normalizeHistoryState(
  state: ReadBackendDiagnosticsResponse["historyState"],
): LogHistoryResult<DiagnosticLogPosition>["historyState"] {
  switch (Number(state)) {
    case 1:
      return "older-output-may-remain";
    case 2:
      return "no-older-output-observed";
    default:
      return "unspecified";
  }
}

function normalizeSourceState(state: DiagnosticSourceState): LogSourceState {
  switch (Number(state)) {
    case 1:
      return "available";
    case 2:
      return "unavailable";
    case 3:
      return "truncated";
    case 4:
      return "continuity-uncertain";
    case 5:
      return "ended";
    default:
      return "unspecified";
  }
}

function failedFollow(error: DiagnosticLogError): AsyncIterable<LogFollowEvent> {
  return { [Symbol.asyncIterator]: () => ({ next: () => Promise.reject(error) }) };
}

function normalizeError(error: unknown): DiagnosticLogError {
  const connectError = ConnectError.from(error);
  return new DiagnosticLogError(
    connectError.rawMessage || "Site log request failed.",
    connectError.code,
  );
}
