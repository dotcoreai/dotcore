import {
  buildDataTable,
  type DataTableField,
  type DataTableResult,
  type DataTableRow,
  formatRange,
  type ValueRange,
  valueRange,
} from "@/components/data-table/data-table-model";
import { formatByteRange, formatCompactCount } from "@/lib/format";

import type {
  AcceleratorMemoryKind,
  ModelRow,
  ProfileMemory,
  ProfileRow,
} from "./catalog-model-row";
import type { CatalogHiddenReason } from "./catalog-profile-compatibility";
import { noneRun, type RunsOnField } from "./catalog-profile-fields";
import {
  type CatalogPredicateField,
  catalogRunsOnTableDefinition,
  catalogTableDefinition,
  catalogViewCodec,
  type CatalogViewState,
} from "./catalog-view-state";
import type { MemoryBasis, MemoryFit } from "./profile-compatibility";

/** A range of accelerator memory in bytes, with where its values come from. */
export interface MemoryRange extends ValueRange {
  kinds: AcceleratorMemoryKind[];
}

export interface CatalogTableInput {
  /** Whether profile memory estimates are still loading. Memory then sorts by name. */
  estimatesPending: boolean;
  rows: readonly ModelRow[];
  /**
   * The Runs on field, once the compatibility read evaluated a target, with the same field without
   * memory fit and why the fit hides a profile, for counting hidden profiles. Without it, a Runs on
   * chip in the view matches everything.
   */
  runsOn?: {
    field: DataTableField<ProfileRow, RunsOnField>;
    hiddenReason?: (
      profile: ProfileRow,
      values: readonly string[],
    ) => CatalogHiddenReason | undefined;
    supportField?: DataTableField<ProfileRow, RunsOnField>;
  };
  /** Whether profile memory cells show the fit at the catalog's location: one machine. */
  showFit?: boolean;
  view: CatalogViewState;
}

/** The ordered rows, counts, and filter menus of the Catalog table for one view. */
export interface CatalogTableModel extends DataTableResult<TableModelRow, CatalogPredicateField> {
  /**
   * The profiles that run in the Runs on chip's scopes and match the other filters, but that their
   * memory fit hides, counted by why.
   */
  hiddenByFit: Record<CatalogHiddenReason, number>;
  /** Whether memory values are still pending, so the table shows a placeholder for them. */
  memoryPending: boolean;
  /** Whether the Runs on chip has values and no profile of any model runs in them. */
  noneRun: boolean;
  /** Whether profile memory cells show the fit at the catalog's location. */
  showFit: boolean;
}

/** A model shown through its matching profiles, with values summarized over those profiles. */
export interface TableModelRow extends DataTableRow<ModelRow, ProfileRow> {
  /** Engine kind keys of the matching profiles. */
  engineKinds: string[];
  /** The maximum context of the matching profiles: the model's, or a span when one lowers it. */
  maximumContext?: ValueRange;
  memory?: MemoryRange;
  quantizations: string[];
  totalContext?: ValueRange;
}

/**
 * Filters, summarizes, and orders the Catalog rows for a view. While estimates load, a memory sort
 * orders by name, and the header keeps showing the view's sort. A predicate on the Runs on field
 * applies only once the field is known, so a model without its profiles' compatibility isn't hidden.
 */
export function buildCatalogTable(input: CatalogTableInput): CatalogTableModel {
  const { estimatesPending, rows, runsOn, view } = input;
  const definition = runsOn
    ? { ...catalogRunsOnTableDefinition, fields: [runsOn.field, ...catalogTableDefinition.fields] }
    : catalogTableDefinition;
  const known = view.predicates.filter(({ field }) =>
    definition.fields.some(({ id }) => id === field),
  );
  const ordered = {
    ...view,
    predicates: known,
    sort:
      estimatesPending && view.sort.column === "memory"
        ? // The default sort for the shown columns, so a hidden column never orders the table.
          catalogViewCodec.setColumns(catalogViewCodec.defaultView, view.columns).sort
        : view.sort,
  };
  const table = buildDataTable(definition, rows, ordered);
  const runsOnValues = known.find(({ field }) => field === "runsOn")?.values ?? [];

  return {
    ...table,
    // A table without the Runs on field has no values for it.
    facets: { ...table.facets, runsOn: table.facets.runsOn ?? [] },
    hiddenByFit: hiddenByFit(input, ordered, table.rows, runsOnValues),
    memoryPending: estimatesPending,
    noneRun:
      runsOn !== undefined &&
      noneRun(
        runsOn.field,
        rows.flatMap((model) => model.profiles),
        runsOnValues,
      ),
    rows: table.rows.map(tableRow),
    showFit: input.showFit ?? false,
  };
}

/**
 * The memory text of a model or profile row, such as "16.1–32.8 GiB". A value made only of declared
 * minimums reads "≥9.3 GiB", and a value made only of system RAM carries a "system RAM" label.
 * Memory uses the console's binary units, so it reads the same as the deploy dialog and the memory
 * view.
 */
export function formatMemory(memory: MemoryRange | ProfileMemory | undefined): {
  label?: string;
  value: string;
} {
  if (!memory) return { value: "—" };

  const range = "kind" in memory ? { ...memory, kinds: [memory.kind] } : memory;
  const minimumOnly = range.kinds.every((kind) => kind === "minimum");
  const systemRamOnly = range.kinds.every((kind) => kind === "system-ram");
  return {
    label: systemRamOnly ? "system RAM" : undefined,
    value: `${minimumOnly ? "≥" : ""}${formatByteRange(BigInt(range.min), BigInt(range.max))}`,
  };
}

/**
 * A memory fit in a few words, such as "fits" or "exceeds free memory", naming the basis where an
 * excess on free memory would read as one on the hardware.
 */
export function formatMemoryFit(fit: MemoryFit, basis: MemoryBasis): string {
  switch (fit) {
    case "exceeds":
      return basis === "free" ? "exceeds free memory" : "exceeds memory";
    case "fits":
      return "fits";
    case "unknown":
      return "fit unknown";
    case "unsupported":
      return "unsupported";
  }
}

/** Context sizes compacted by `formatCompactCount`, such as "32k" or "32k–128k". */
export function formatContext(context: ValueRange | number | undefined): string {
  if (context === undefined) return "—";
  return formatRange(
    typeof context === "number" ? { max: context, min: context } : context,
    formatCompactCount,
  );
}

/** A reference score as the source shows it, with at most one decimal, such as "34" or "41.5". */
export function formatReferenceScore(score: number): string {
  return referenceScoreFormat.format(score);
}

const referenceScoreFormat = new Intl.NumberFormat("en-US", { maximumFractionDigits: 1 });

const memoryKindOrder: readonly AcceleratorMemoryKind[] = ["estimate", "minimum", "system-ram"];

// The profiles the same view shows when Runs on ignores memory fit, less those it shows, by why.
function hiddenByFit(
  { rows, runsOn }: CatalogTableInput,
  view: CatalogViewState,
  shown: readonly DataTableRow<ModelRow, ProfileRow>[],
  runsOnValues: readonly string[],
): Record<CatalogHiddenReason, number> {
  const counts: Record<CatalogHiddenReason, number> = { exceeds: 0, unsupported: 0 };
  if (!runsOn?.supportField || !runsOn.hiddenReason || !runsOnValues.length) return counts;

  const shownProfiles = new Set(
    shown.flatMap(({ children }) =>
      children.map(({ modelId, profileId }) => `${modelId}\0${profileId}`),
    ),
  );
  const supported = buildDataTable(
    {
      ...catalogRunsOnTableDefinition,
      fields: [runsOn.supportField, ...catalogTableDefinition.fields],
    },
    rows,
    view,
  );
  for (const profile of supported.rows.flatMap(({ children }) => children)) {
    if (shownProfiles.has(`${profile.modelId}\0${profile.profileId}`)) continue;
    const reason = runsOn.hiddenReason(profile, runsOnValues);
    if (reason) counts[reason] += 1;
  }
  return counts;
}

function tableRow(row: DataTableRow<ModelRow, ProfileRow>): TableModelRow {
  const matching = row.children;
  const memories = matching.flatMap((profile) => (profile.memory ? [profile.memory] : []));
  const memory = valueRange(memories.flatMap(({ max, min }) => [min, max]));

  return {
    ...row,
    engineKinds: unique(matching.flatMap((profile) => profile.engineKinds)),
    maximumContext: valueRange(matching.map((profile) => profile.maximumContext)),
    memory: memory && {
      ...memory,
      kinds: memoryKindOrder.filter((kind) =>
        memories.some((candidate) => candidate.kind === kind),
      ),
    },
    quantizations: unique(matching.flatMap((profile) => profile.quantization ?? [])),
    totalContext: valueRange(matching.map((profile) => profile.totalContext)),
  };
}

function unique<T>(values: readonly T[]): T[] {
  return [...new Set(values)];
}
