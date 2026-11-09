import type { ReactNode } from "react";

import type { DataTableUiDefinition } from "@/components/data-table/data-table";
import { Tags } from "@/components/data-table/data-table-cells";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { StatusBadge } from "@/components/ui/status-badge";
import { requestKindsLabel } from "@/domain/request-kinds";
import { formatCount } from "@/lib/format";

import {
  engineKindLabel,
  type ModelRow,
  type ProfileRow,
  speculativeDefaultMethod,
} from "./catalog-model-row";
import {
  ContextValue,
  IntelligenceValue,
  MemoryValue,
  ProfileNameCell,
} from "./catalog-profile-cells";
import { profileFieldUi, type ProfileStatus, runsOnFieldUi } from "./catalog-profile-fields";
import { type CatalogTableModel, type TableModelRow } from "./catalog-table-model";
import {
  type CatalogColumnId,
  catalogPlainViewCodec,
  type CatalogPredicateField,
  catalogRunsOnTableDefinition,
  catalogTableDefinition,
  catalogViewCodec,
} from "./catalog-view-state";
import { modelInspectorPanelId } from "./model-inspector-panel";

/**
 * The model catalog as a rendered data table: models expanding into their matching profiles, with
 * the model's intelligence score, memory, maximum context and total context metrics, engine and quantization tags, and a
 * placeholder for memory while estimates load.
 */
export const catalogTableUi: DataTableUiDefinition<
  ModelRow,
  ProfileRow,
  CatalogColumnId,
  CatalogPredicateField,
  CatalogTableModel
> = {
  child: {
    id: (profile) => profile.profileId,
    name: (profile) => profile.displayName,
    noun: { one: "profile", other: "profiles" },
  },
  codec: catalogPlainViewCodec,
  columns: {
    engine: {
      cell: (row) => <Tags values={row.engineKinds.map(engineKindLabel)} />,
      childCell: (profile) => <Tags values={profile.engineKinds.map(engineKindLabel)} />,
    },
    // A score belongs to the model: profile rows leave the cell empty, and so does an unscored model.
    intelligence: {
      cell: ({ row }) => <IntelligenceValue score={row.referenceScore} />,
    },
    // A licence belongs to the model: profile rows leave the cell empty, and so does an undeclared one.
    license: {
      cell: ({ row }) => (row.licenseId ? <Tags values={[row.licenseId]} /> : null),
    },
    maximumContext: {
      cell: (row) => <ContextValue context={row.maximumContext} />,
      childCell: (profile) => <ContextValue context={profile.maximumContext} />,
    },
    memory: {
      cell: (row, table) => <MemoryValue memory={row.memory} pending={table.memoryPending} />,
      childCell: (profile, table) => (
        <MemoryValue
          fit={table.showFit ? profile.fit : undefined}
          memory={profile.memory}
          pending={table.memoryPending}
          unsupported={Boolean(profile.scenarioUnsupported)}
        />
      ),
    },
    model: {
      cell: modelCell,
      childCell: (profile, table) => (
        <ProfileNameCell profile={profile} status={scenarioStatus(profile, table)} />
      ),
    },
    quantization: {
      cell: (row) => <Tags mono values={row.quantizations} />,
      childCell: (profile) => (
        <Tags mono values={profile.quantization ? [profile.quantization] : []} />
      ),
    },
    totalContext: {
      cell: (row) => <ContextValue context={row.totalContext} />,
      childCell: (profile) => <ContextValue context={profile.totalContext} />,
    },
    // A model lists the methods it offers; a profile shows its default method, or "Off".
    speculative: {
      cell: ({ row }) => <Tags values={row.speculativeMethods} />,
      childCell: (profile) => (
        <Tags values={speculativeDefaultMethod(profile.speculativeOptions)} />
      ),
    },
  },
  definition: catalogTableDefinition,
  fields: { ...profileFieldUi, runsOn: runsOnFieldUi },
  label: "Model catalog",
  noun: { one: "model", other: "models" },
  panelId: modelInspectorPanelId,
};

/**
 * The Catalog table of a site whose read evaluated model execution targets: with Runs on, and a
 * view that starts with its chip.
 */
export const catalogRunsOnTableUi: typeof catalogTableUi = {
  ...catalogTableUi,
  codec: catalogViewCodec,
  definition: catalogRunsOnTableDefinition,
};

function modelCell(row: TableModelRow): ReactNode {
  const { row: model } = row;
  // A model whose profiles filters hide reads "3 of 6 profiles"; removing the filter shows the rest.
  const profiles = row.hiddenChildCount
    ? `${row.children.length} of ${formatCount(model.profileCount, "profile")}`
    : formatCount(model.profileCount, "profile");
  return (
    <>
      <div className="flex flex-wrap items-center gap-1.5 text-sm font-semibold">
        <span>
          <ResourceMarker colorKey={model.colorKey} />
          {model.displayName}
        </span>
        {model.runningStatus ? <StatusBadge status={model.runningStatus} /> : null}
      </div>
      <div className="mt-0.5 text-xs text-muted-foreground">
        {[model.supportsVision ? "Vision" : "", requestKindsLabel(model.requestKinds), profiles]
          .filter(Boolean)
          .join(" · ")}
      </div>
    </>
  );
}

// Why a profile can't serve the scenario, everywhere or on the selected machine's hardware.
function scenarioStatus(profile: ProfileRow, table: CatalogTableModel): ProfileStatus | undefined {
  if (table.memoryPending) return undefined;

  const text =
    profile.scenarioUnsupported ??
    (table.showFit && profile.fit?.fit === "unsupported" ? profile.fit.reason : undefined);
  return text ? { text, tone: "blocked" } : undefined;
}
