import { ActivityIcon, ServerIcon } from "lucide-react";

import type { DataTableUiDefinition } from "@/components/data-table/data-table";
import { formatChartValue } from "@/components/metrics/time-series-chart/chart-data";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { StatusBadge } from "@/components/ui/status-badge";
import { machineName, placementMachine } from "@/features/deployments/placement";
import type { InferenceRowValues } from "@/features/inference/use-inference-metrics";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";
import type { DataTableDefinition } from "@/lib/data-table-model";
import { createDataTableViewCodec } from "@/lib/data-table-view";
import { formatBytes } from "@/lib/format";
import type { StatusProjection } from "@/lib/status-projection";
import { upstreamLocalityLabel } from "@/lib/upstream-presentation";

import { modelInspectorPanelId } from "./model-inspector-panel";
import {
  localModelSnapshot,
  modelDeploymentPresentationName,
  projectModelDeploymentStatus,
} from "./model-projection";

/** A column of the models table, in display order. */
export type ModelDeploymentColumnId =
  | "deployment"
  | "status"
  | "machine"
  | "requests"
  | "vram"
  | "generatedTokens"
  | "tokenGeneration"
  | "ttft";

/** A filter field of the models table. */
export type ModelDeploymentField = "status" | "machine";

/** One model deployment as the models table shows it. A missing value is absent. */
export interface ModelDeploymentRow {
  /** The row's name for assistive technology, with the deployment id when the name is ambiguous. */
  accessibleName: string;
  deploymentId: string;
  /** The model and its weight quantization, or the upstream origin. */
  detail?: string;
  displayName: string;
  /** The machine's SiteNode ID, absent without a placement or for an upstream deployment. */
  machineId?: string;
  /** The machine's name, or "Upstream" for an upstream deployment. */
  machineName?: string;
  status: StatusProjection;
  /** Traffic over the page range; absent until the metrics are first read. */
  traffic?: InferenceRowValues;
  /** Accelerator memory measured on the deployment's devices, absent when none is measured. */
  vramBytes?: bigint;
}

/** What the models table combines with each deployment. */
export interface ModelDeploymentRowSources {
  machines: readonly Machine[] | undefined;
  trafficOf: (deploymentId: string) => InferenceRowValues | undefined;
}

/**
 * The model deployments as a data table: one row per deployment, filtered on the projected status
 * and the machine's name. Status and machine sort by their label, figures by their value.
 */
export const modelDeploymentTableDefinition: DataTableDefinition<
  ModelDeploymentRow,
  ModelDeploymentRow,
  ModelDeploymentColumnId,
  ModelDeploymentField
> = {
  columns: [
    {
      id: "deployment",
      key: "deployment",
      label: "Deployment",
      pinned: true,
      sort: { key: "name", kind: "name" },
    },
    {
      defaultVisible: true,
      id: "status",
      key: "status",
      label: "Status",
      sort: { kind: "text", value: (row) => row.status.label },
    },
    {
      defaultVisible: true,
      id: "machine",
      key: "machine",
      label: "Machine",
      sort: { kind: "text", value: (row) => row.machineName },
    },
    {
      defaultVisible: true,
      id: "requests",
      key: "requests",
      label: "Requests",
      sort: { kind: "value", value: (row) => row.traffic?.requests },
    },
    {
      defaultVisible: true,
      id: "vram",
      key: "vram",
      label: "VRAM",
      sort: {
        kind: "value",
        value: (row) => (row.vramBytes === undefined ? undefined : Number(row.vramBytes)),
      },
    },
    {
      id: "generatedTokens",
      key: "generated",
      label: "Generated tokens",
      sort: { kind: "value", value: (row) => row.traffic?.generatedTokens },
    },
    {
      id: "tokenGeneration",
      key: "speed",
      label: "Token generation",
      sort: { kind: "value", value: (row) => row.traffic?.tokenGeneration ?? undefined },
    },
    {
      id: "ttft",
      key: "ttft",
      label: "Time to first token",
      sort: { kind: "value", value: (row) => row.traffic?.ttft ?? undefined },
    },
  ],
  defaultSort: { column: "deployment", direction: "asc" },
  fields: [
    {
      domain: { kind: "open" },
      id: "status",
      key: "status",
      label: "Status",
      match: "any",
      values: (row) => [row.status.label],
    },
    {
      domain: { kind: "open" },
      id: "machine",
      key: "machine",
      label: "Machine",
      match: "any",
      values: (row) => (row.machineName ? [row.machineName] : []),
    },
  ],
  rowId: (row) => row.deploymentId,
  rowName: (row) => row.accessibleName,
  searchText: (row) => [row.displayName, row.detail ?? "", row.machineName ?? "", row.deploymentId],
};

/** The models view in the URL: `q`, `f`, `status`, `machine`, `sort`, and `cols`. */
export const modelDeploymentViewCodec = createDataTableViewCodec(modelDeploymentTableDefinition);

/** The model deployments as a rendered data table. */
export const modelDeploymentTableUi: DataTableUiDefinition<
  ModelDeploymentRow,
  ModelDeploymentRow,
  ModelDeploymentColumnId,
  ModelDeploymentField
> = {
  codec: modelDeploymentViewCodec,
  columns: {
    deployment: { cell: ({ row }) => deploymentCell(row) },
    generatedTokens: {
      cell: ({ row }) =>
        figure(row.traffic && formatChartValue("requests", row.traffic.generatedTokens)),
    },
    machine: { cell: ({ row }) => machineCell(row), showFrom: "32rem" },
    requests: {
      cell: ({ row }) => figure(row.traffic && formatChartValue("requests", row.traffic.requests)),
      showFrom: "34rem",
    },
    status: { cell: ({ row }) => <StatusBadge status={row.status} /> },
    tokenGeneration: {
      cell: ({ row }) => figure(optional("tokensPerSecond", row.traffic?.tokenGeneration)),
    },
    ttft: { cell: ({ row }) => figure(optional("seconds", row.traffic?.ttft)) },
    vram: {
      cell: ({ row }) =>
        figure(row.vramBytes === undefined ? undefined : formatBytes(row.vramBytes)),
      showFrom: "40rem",
    },
  },
  definition: modelDeploymentTableDefinition,
  fields: {
    machine: { icon: ServerIcon },
    status: { icon: ActivityIcon },
  },
  label: "Running models",
  noun: { one: "model", other: "models" },
  panelId: modelInspectorPanelId,
};

/** The table rows of the listed model deployments. */
export function modelDeploymentRows(
  deployments: readonly ModelDeployment[],
  { machines, trafficOf }: ModelDeploymentRowSources,
): ModelDeploymentRow[] {
  const names = new Map<string, number>();
  for (const deployment of deployments) {
    const name = modelDeploymentPresentationName(deployment);
    names.set(name, (names.get(name) ?? 0) + 1);
  }

  return deployments.map((deployment) => {
    const displayName = modelDeploymentPresentationName(deployment);
    const upstream = deployment.snapshot?.source.case === "upstream";
    const siteNodeId = upstream ? undefined : deploymentSiteNodeId(deployment);

    return {
      accessibleName:
        upstream || (names.get(displayName) ?? 0) > 1
          ? `${displayName} (${deployment.modelDeploymentId})`
          : displayName,
      deploymentId: deployment.modelDeploymentId,
      detail: upstream ? upstreamDetail(deployment) : localDetail(deployment),
      displayName,
      machineId: siteNodeId,
      machineName: upstream
        ? "Upstream"
        : siteNodeId
          ? machineName(siteNodeId, placementMachine(machines, siteNodeId))
          : undefined,
      status: projectModelDeploymentStatus(deployment),
      traffic: trafficOf(deployment.modelDeploymentId),
      vramBytes: deploymentVramBytes(deployment),
    };
  });
}

/** The accelerator memory measured on the deployment's devices; absent without a measure. */
function deploymentVramBytes(deployment: ModelDeployment): bigint | undefined {
  const result = deployment.acceleratorMemory?.result;
  const devices = result?.case === "available" ? result.value.devices : [];
  return devices.length
    ? devices.reduce((total, device) => total + device.usedMemoryBytes, 0n)
    : undefined;
}

/** The machine a local deployment is placed on, else the one it was observed on. */
function deploymentSiteNodeId(deployment: ModelDeployment): string | undefined {
  return (
    deployment.intent?.placement?.siteNodeId ||
    deployment.state?.observedPlacements[0]?.managedNodeId ||
    undefined
  );
}

function localDetail(deployment: ModelDeployment): string | undefined {
  const model = localModelSnapshot(deployment.snapshot);
  const parts = [model?.modelDisplayName, model?.modelArtifact?.weightQuantization].filter(Boolean);
  return parts.length ? parts.join(" · ") : undefined;
}

/**
 * An upstream deployment names what it is: an external model, the provider serving it, and where
 * that provider runs.
 */
function upstreamDetail(deployment: ModelDeployment): string {
  const boundary = deployment.upstreamDesiredConfiguration?.boundary;
  const locality = boundary ? upstreamLocalityLabel(boundary.locality) : undefined;
  return ["External", boundary?.trustDomain, locality].filter(Boolean).join(" · ");
}

const dash = <span className="text-muted-foreground">—</span>;

function deploymentCell(row: ModelDeploymentRow) {
  return (
    <>
      <div className="text-sm font-semibold">
        <ResourceMarker colorKey={row.deploymentId} />
        {row.displayName}
      </div>
      {row.detail ? <div className="mt-0.5 text-xs text-muted-foreground">{row.detail}</div> : null}
    </>
  );
}

function machineCell(row: ModelDeploymentRow) {
  if (!row.machineName) return dash;
  if (!row.machineId) return <span>{row.machineName}</span>;

  return (
    <span>
      <ResourceMarker colorKey={row.machineId} />
      {row.machineName}
    </span>
  );
}

function optional(unit: "seconds" | "tokensPerSecond", value: number | null | undefined) {
  return value === null || value === undefined ? undefined : formatChartValue(unit, value);
}

function figure(text: string | undefined) {
  return text === undefined ? dash : <span className="tabular-nums">{text}</span>;
}
