import { Code } from "@connectrpc/connect";
import { type RefObject, useMemo } from "react";

import {
  type DeploymentLogEntry,
  DeploymentLogError,
  type DeploymentLogOperations,
  type DeploymentLogResumePosition,
  type LogTimestamp,
} from "@/api/deployment-log-client";
import { LogViewerModal } from "@/components/log-viewer/log-viewer-modal";
import type {
  LogEntry,
  LogFollowFailure,
  LogViewerOperations,
} from "@/components/log-viewer/types";
import { ResourceTitle } from "@/components/ui/resource-marker";

export interface DeploymentLogViewerProps {
  /** Identity-marker color key of the deployment shown in the `<name> · Logs` title. */
  colorKey: string;
  deploymentId: string;
  /** Presentation name of the deployment shown in the `<name> · Logs` title. */
  name: string;
  onOpenChange: (open: boolean) => void;
  open: boolean;
  openerRef?: RefObject<HTMLElement | null>;
  operations: DeploymentLogOperations;
  sourceNames?: readonly string[];
}

export function DeploymentLogViewer({
  colorKey,
  deploymentId,
  name,
  onOpenChange,
  open,
  openerRef,
  operations,
  sourceNames,
}: DeploymentLogViewerProps) {
  const viewerOperations = useMemo(
    () => bindDeploymentOperations(deploymentId, operations, sourceNames),
    [deploymentId, operations, sourceNames],
  );

  return (
    <LogViewerModal
      copy={deploymentCopy}
      onOpenChange={onOpenChange}
      open={open}
      openerRef={openerRef}
      operations={viewerOperations}
      sessionKey={deploymentId}
      showSourceFilters
      title={<ResourceTitle colorKey={colorKey} name={name} screen="Logs" />}
    />
  );
}

const deploymentCopy = {
  ariaLabel: "Deployment log output",
  continuityWarning: "Continuity boundary: output around a reconnect may be missing or repeated.",
  ended: "Deployment removed.",
  empty: "No retained output is available.",
  sourceTruncated: "Retained output was truncated.",
  unavailable: "Logs are unavailable for this deployment.",
};

interface DeploymentViewerPosition {
  beforeTimestamp?: LogTimestamp;
  resumePositions?: DeploymentLogResumePosition[];
}

function bindDeploymentOperations(
  deploymentId: string,
  operations: DeploymentLogOperations,
  sourceNames?: readonly string[],
): LogViewerOperations<DeploymentViewerPosition> {
  return {
    read: async (request, signal) => {
      const result = await operations.read(
        {
          deploymentId,
          pageSize: request.pageSize,
          sourceNames: sourceNames?.length ? [...sourceNames] : undefined,
          beforeTimestamp: request.beforePosition?.beforeTimestamp,
        },
        signal,
      );
      return {
        entries: result.entries.map(toViewerEntry),
        historyState: result.historyState,
        latestPosition: {
          resumePositions: result.latestPositions.length
            ? result.latestPositions
            : latestResumePositions(result.entries),
        },
        nextBeforePosition: result.nextBeforeTimestamp
          ? { beforeTimestamp: result.nextBeforeTimestamp }
          : undefined,
        responseTruncated: result.responseTruncated,
        sources: result.sources,
      };
    },
    follow: (request, signal) =>
      normalizeFollow(
        operations.follow(
          {
            deploymentId,
            resumePositions: request.sourceName
              ? request.resumePosition?.resumePositions?.filter(
                  (position) => position.sourceName === request.sourceName,
                )
              : request.resumePosition?.resumePositions,
            sourceNames: request.sourceName
              ? [request.sourceName]
              : sourceNames?.length
                ? [...sourceNames]
                : undefined,
          },
          signal,
        ),
      ),
    classifyFollowError: classifyFollowFailure,
    advancePosition: (current, entry) => {
      if (!entry.timestamp) return current;
      const positions = new Map(
        (current?.resumePositions ?? []).map((position) => [position.sourceName, position]),
      );
      const existing = positions.get(entry.sourceName)?.latestTimestamp;
      if (!existing || compareTimestamps(entry.timestamp, existing) > 0) {
        positions.set(entry.sourceName, {
          latestTimestamp: entry.timestamp,
          sourceName: entry.sourceName,
        });
      }
      return { ...current, resumePositions: [...positions.values()] };
    },
  };
}

function classifyFollowFailure(error: unknown): LogFollowFailure {
  if (!(error instanceof DeploymentLogError)) return "retry";
  if (error.code === Code.NotFound) return "gone";
  // The transport reports a dropped connection as an unknown error, so it reopens like unavailable.
  return error.code === Code.Unavailable || error.code === Code.Unknown ? "retry" : "fatal";
}

async function* normalizeFollow(events: ReturnType<DeploymentLogOperations["follow"]>) {
  for await (const event of events) {
    if (event.event.kind === "entry") {
      yield { event: { kind: "entry" as const, entry: toViewerEntry(event.event.entry) } };
    } else {
      yield { event: { kind: "source" as const, source: event.event.source } };
    }
  }
}

function toViewerEntry(entry: DeploymentLogEntry): LogEntry {
  return {
    category: entry.stream,
    sourceLabel: entry.sourceLabel,
    sourceName: entry.sourceName,
    text: entry.text,
    timestamp: entry.timestamp,
    truncated: entry.truncated,
  };
}

function latestResumePositions(entries: readonly DeploymentLogEntry[]) {
  const positions = new Map<string, DeploymentLogResumePosition>();
  for (const entry of entries) {
    if (entry.timestamp) {
      positions.set(entry.sourceName, {
        latestTimestamp: entry.timestamp,
        sourceName: entry.sourceName,
      });
    }
  }
  return [...positions.values()];
}

function compareTimestamps(left: LogTimestamp, right: LogTimestamp) {
  if (left.seconds !== right.seconds) return left.seconds < right.seconds ? -1 : 1;
  return left.nanos - right.nanos;
}
