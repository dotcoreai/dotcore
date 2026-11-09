import {
  buildDataTable,
  type DataTableDefinition,
  type DataTableField,
  type DataTableResult,
  type DataTableRow,
} from "@/components/data-table/data-table-model";
import {
  createDataTableViewCodec,
  type DataTablePredicate,
  type DataTableView,
  type DataTableViewCodec,
} from "@/components/data-table/data-table-view";

import type { ProfileRow } from "./model-row";
import type { CatalogHiddenReason } from "./profile-availability";
import {
  noneRun,
  profileFields,
  type ProfileMetricColumnId,
  profileMetricColumns,
  type ProfilePredicateField,
  type ProfileStatus,
  type RunsOnField,
} from "./profile-fields";

/** A column of a profile table, in display order. */
export type ProfileColumnId = "profile" | ProfileMetricColumnId;

/** A filter field of a profile table: the shared profile fields, then Runs on when known. */
export type ProfileTableField = ProfilePredicateField | RunsOnField;

export type ProfileTableView = DataTableView<ProfileColumnId, ProfileTableField>;

export type ProfileTableDefinition = DataTableDefinition<
  ProfileRow,
  ProfileRow,
  ProfileColumnId,
  ProfileTableField
>;

export type ProfileTableCodec = DataTableViewCodec<ProfileColumnId, ProfileTableField>;

/** What a compatibility source says about one profile in a profile table. */
export interface ProfileAvailability {
  /** Whether the profile can be selected; one that can't is listed after those that can. */
  selectable: boolean;
  status?: ProfileStatus;
}

/**
 * The Runs on filter of a profile table: its field and, for counting the profiles its memory fit
 * hides, the same field without memory fit and why the fit hides a profile.
 */
export interface ProfileTableRunsOn {
  field: DataTableField<ProfileRow, RunsOnField>;
  hiddenReason?: (
    profile: ProfileRow,
    values: readonly string[],
  ) => CatalogHiddenReason | undefined;
  supportField?: DataTableField<ProfileRow, RunsOnField>;
}

export interface ProfileTableInput {
  /** The availability of a profile, or undefined when nothing is known about it. */
  availability?: (profile: ProfileRow) => ProfileAvailability | undefined;
  definition: ProfileTableDefinition;
  /** Whether profile memory estimates are still loading. Memory then sorts by name. */
  estimatesPending: boolean;
  profiles: readonly ProfileRow[];
  /** The Runs on filter whose field the definition holds, for counting what its memory fit hides. */
  runsOn?: Pick<ProfileTableRunsOn, "hiddenReason" | "supportField">;
  view: ProfileTableView;
}

/** The ordered rows, counts, and filter menus of a profile table for one view. */
export interface ProfileTableModel extends DataTableResult<ProfileTableRow, ProfileTableField> {
  /**
   * The profiles that run in the Runs on chip's scopes and match the other filters, but that their
   * memory fit hides, counted by why.
   */
  hiddenByFit: Record<CatalogHiddenReason, number>;
  /** Whether memory values are still pending, so the table shows a placeholder for them. */
  memoryPending: boolean;
  /** Whether the Runs on chip has values and no profile runs in any of them, whatever the filters. */
  noneRun: boolean;
}

export interface ProfileTableRow extends DataTableRow<ProfileRow, ProfileRow> {
  availability?: ProfileAvailability;
}

/**
 * The profiles of one model as a flat data table, with the catalog page's profile filters and
 * metric columns, plus the Runs on filter when given. `columns` narrows the metric columns, such as
 * for a narrow panel, and `defaultColumns` picks the ones shown by default, leaving the others to
 * show on demand. Its view stays local to its host rather than in the URL.
 */
export function profileTableDefinition(
  runsOn?: ProfileTableRunsOn,
  columns?: readonly ProfileMetricColumnId[],
  defaultColumns?: readonly ProfileMetricColumnId[],
): ProfileTableDefinition {
  const metricColumns = profileMetricColumns
    .filter(({ id }) => !columns || columns.includes(id))
    .map((column) =>
      defaultColumns ? { ...column, defaultVisible: defaultColumns.includes(column.id) } : column,
    );
  return {
    columns: [
      {
        id: "profile",
        key: "profile",
        label: "Profile",
        pinned: true,
        sort: { key: "name", kind: "name" },
      },
      ...metricColumns,
    ],
    // Largest memory first, the order of `compareProfiles`, whenever the table shows memory.
    defaultSort: metricColumns.some(({ id }) => id === "memory")
      ? { column: "memory", direction: "desc" }
      : { column: "profile", direction: "asc" },
    fields: runsOn ? [runsOn.field, ...profileFields] : profileFields,
    rowId: (profile) => profile.profileId,
    rowName: (profile) => profile.displayName,
    searchText: (profile) => [profile.displayName, profile.profileId],
  };
}

/**
 * The view rules of a profile table. The default view starts with the given chips, such as those
 * copied from the catalog, so Reset and "Reset filters" restore them.
 */
export function profileTableCodec(
  definition: ProfileTableDefinition,
  start: readonly DataTablePredicate<ProfileTableField>[] = [],
): ProfileTableCodec {
  const codec = createDataTableViewCodec(definition);
  if (!start.length) return codec;

  const defaultView = {
    ...codec.defaultView,
    predicates: start.map(({ field, values }) => ({ field, values: [...values] })),
  };
  return {
    ...codec,
    defaultView,
    isDefault: (view) => codec.serialize(view) === codec.serialize(defaultView),
  };
}

/** Replaces a view's Runs on chip with one holding `values`, leading the other chips. */
export function withRunsOn(
  view: ProfileTableView,
  values: readonly string[] | undefined,
): ProfileTableView {
  const others = view.predicates.filter(({ field }) => field !== "runsOn");
  return {
    ...view,
    predicates:
      values === undefined ? others : [{ field: "runsOn", values: [...values] }, ...others],
  };
}

/**
 * Filters and orders the profiles for a view. Selectable profiles come before the others, and the
 * view's sort orders each group. While estimates load, a memory sort orders by name, and the header
 * keeps showing the view's sort.
 */
export function buildProfileTable(input: ProfileTableInput): ProfileTableModel {
  const { availability, definition, estimatesPending, profiles, view } = input;
  const ordered =
    estimatesPending && view.sort.column === "memory"
      ? { ...view, sort: { column: "profile" as const, direction: "asc" as const } }
      : view;
  const table = buildDataTable(definition, profiles, ordered);
  const rows = table.rows.map((row) => ({ ...row, availability: availability?.(row.row) }));

  const runsOn = view.predicates.find(({ field }) => field === "runsOn")?.values ?? [];
  const runsOnField = definition.fields.find(({ id }) => id === "runsOn");

  return {
    ...table,
    hiddenByFit: hiddenByFit(input, table.rows, runsOn),
    memoryPending: estimatesPending,
    noneRun: runsOnField !== undefined && noneRun(runsOnField, profiles, runsOn),
    // A stable sort keeps the view's order within each rank.
    rows: rows.sort((left, right) => rank(left) - rank(right)),
  };
}

// The profiles the same view shows when Runs on ignores memory fit, less those it shows, by why.
function hiddenByFit(
  { definition, profiles, runsOn, view }: ProfileTableInput,
  shown: readonly DataTableRow<ProfileRow, ProfileRow>[],
  runsOnValues: readonly string[],
): Record<CatalogHiddenReason, number> {
  const counts: Record<CatalogHiddenReason, number> = { exceeds: 0, unsupported: 0 };
  const supportField = runsOn?.supportField;
  if (!supportField || !runsOn.hiddenReason || !runsOnValues.length) return counts;

  const shownProfiles = new Set(shown.map(({ row }) => row.profileId));
  const supported = buildDataTable(
    {
      ...definition,
      fields: definition.fields.map((field) => (field.id === "runsOn" ? supportField : field)),
    },
    profiles,
    view,
  );
  for (const { row } of supported.rows) {
    if (shownProfiles.has(row.profileId)) continue;
    const reason = runsOn.hiddenReason(row, runsOnValues);
    if (reason) counts[reason] += 1;
  }
  return counts;
}

function rank({ availability }: ProfileTableRow): number {
  return availability?.selectable === false ? 1 : 0;
}
