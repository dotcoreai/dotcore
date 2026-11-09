import { CpuIcon, FastForwardIcon, LayersIcon, ServerIcon, SparklesIcon } from "lucide-react";

import type { DataTableFieldUi } from "@/components/data-table/data-table";
import type { DataTableColumn, DataTableField } from "@/components/data-table/data-table-model";

import { engineKindLabel, type ProfileRow } from "./catalog-model-row";
import { type RunsOnScope, runsOnScopeKey } from "./profile-compatibility";

/** A filter field every table of model profiles shares. */
export type ProfilePredicateField = "engine" | "quantization" | "capability" | "speculative";

/** The Runs on filter field, which tables of model profiles add when they have compatibility. */
export type RunsOnField = "runsOn";

/** A scope the Runs on filter offers, with its menu and chip label. */
export interface RunsOnOption {
  label: string;
  scope: RunsOnScope;
}

/** A metric column every table of model profiles shares. */
export type ProfileMetricColumnId = "intelligence" | "memory" | "maximumContext" | "totalContext";

/** A capability filter value. */
export type ProfileCapability = "image";

/**
 * A line under a profile's name, such as why it can't run on a machine. Its tone colors it: a
 * blocking reason, an advisory on a profile that can run, or an unknown status.
 */
export interface ProfileStatus {
  text: string;
  tone: "advisory" | "blocked" | "unknown";
}

/**
 * The filter fields of every table of model profiles, in the Filter menu's order: Engine,
 * Quantization, Capabilities, and Speculative, with the URL keys `engine`, `quant`, `cap`, and
 * `spec`. Speculative matches the draft methods a profile's configuration offers, whether or not
 * the profile turns one on by default.
 */
export const profileFields: readonly DataTableField<ProfileRow, ProfilePredicateField>[] = [
  {
    domain: { kind: "open", label: engineKindLabel },
    id: "engine",
    key: "engine",
    label: "Engine",
    match: "any",
    values: (profile) => profile.engineKinds,
  },
  {
    domain: { kind: "open" },
    id: "quantization",
    key: "quant",
    label: "Quantization",
    match: "any",
    values: (profile) => (profile.quantization ? [profile.quantization] : []),
  },
  {
    domain: {
      kind: "closed",
      values: [{ label: "Image input", value: "image" }],
    },
    id: "capability",
    key: "cap",
    label: "Capabilities",
    // Capabilities add up: each selected one must hold on the same profile.
    match: "all",
    values: profileCapabilities,
  },
  {
    domain: { kind: "open" },
    id: "speculative",
    key: "spec",
    label: "Speculative",
    match: "any",
    values: (profile) => profile.speculativeOptions.map(({ displayName }) => displayName),
  },
];

/** The icon and value rendering of each profile filter field. */
export const profileFieldUi: Record<ProfilePredicateField, DataTableFieldUi> = {
  capability: { icon: SparklesIcon },
  engine: { icon: CpuIcon },
  quantization: {
    icon: LayersIcon,
    renderValue: ({ label }) => <span className="truncate font-mono text-xs">{label}</span>,
  },
  speculative: { icon: FastForwardIcon },
};

/**
 * The Runs on filter field over the offered scopes, with the URL key `runs`. A profile carries the
 * key of each scope where `runs` holds, so a selected scope keeps the profiles `runs` accepts
 * there, such as those a target there runs.
 */
export function runsOnField(
  options: readonly RunsOnOption[],
  runs: (profile: ProfileRow, scope: RunsOnScope) => boolean,
): DataTableField<ProfileRow, RunsOnField> {
  return {
    domain: {
      kind: "closed",
      values: options.map(({ label, scope }) => ({ label, value: runsOnScopeKey(scope) })),
    },
    id: "runsOn",
    key: "runs",
    label: "Runs on",
    match: "any",
    values: (profile) =>
      options.filter(({ scope }) => runs(profile, scope)).map(({ scope }) => runsOnScopeKey(scope)),
  };
}

/** Whether the Runs on chip has values and none of the profiles runs in any of them. */
export function noneRun(
  field: Pick<DataTableField<ProfileRow, string>, "values">,
  profiles: readonly ProfileRow[],
  values: readonly string[],
): boolean {
  return (
    values.length > 0 &&
    !profiles.some((profile) => field.values(profile).some((value) => values.includes(value)))
  );
}

/** The icon of the Runs on filter field, whose values are alternative scopes that a click replaces. */
export const runsOnFieldUi: DataTableFieldUi = { icon: ServerIcon, replaceOnSelect: true };

/**
 * The Intelligence, Memory, Maximum context, and Total context columns, shown by default and sorted
 * by profile values.
 */
export const profileMetricColumns: readonly DataTableColumn<ProfileRow, ProfileMetricColumnId>[] = [
  {
    defaultVisible: true,
    id: "intelligence",
    key: "intelligence",
    label: "Intelligence",
    sort: { kind: "value", value: (profile) => profile.referenceScore },
  },
  {
    defaultVisible: true,
    id: "memory",
    key: "memory",
    label: "Memory",
    sort: { kind: "value", value: (profile) => profile.memory?.max },
  },
  {
    defaultVisible: true,
    id: "maximumContext",
    key: "max-context",
    label: "Maximum context",
    sort: { kind: "value", value: (profile) => profile.maximumContext },
  },
  {
    defaultVisible: true,
    id: "totalContext",
    key: "total-context",
    label: "Total context",
    sort: { kind: "value", value: (profile) => profile.totalContext },
  },
];

/**
 * Orders the profiles of one model by memory estimate, largest first, as a proxy for the highest
 * precision that fits. Profiles without a memory value follow, then display name, then id.
 */
export function compareProfiles(left: ProfileRow, right: ProfileRow): number {
  const leftBytes = left.memory?.max;
  const rightBytes = right.memory?.max;
  const byMemory =
    leftBytes === undefined || rightBytes === undefined
      ? Number(leftBytes === undefined) - Number(rightBytes === undefined)
      : rightBytes - leftBytes;
  return (
    byMemory ||
    left.displayName.localeCompare(right.displayName) ||
    left.profileId.localeCompare(right.profileId)
  );
}

/** The capability filter values that hold for a profile. */
export function profileCapabilities(profile: ProfileRow): ProfileCapability[] {
  return profile.imageInput ? ["image"] : [];
}
