import { type DataTableField, matchesField } from "@/components/data-table/data-table-model";
import type { DataTablePredicate } from "@/components/data-table/data-table-view";
import {
  type CatalogPredicate,
  catalogViewCodec,
} from "@/features/models/catalog/catalog-view-state";

import type { ProfileRow } from "./model-row";
import { runsOnScopeKey } from "./profile-compatibility";
import { profileFields, type RunsOnField } from "./profile-fields";
import type { ProfileTableField } from "./profile-table-model";

/** The catalog's chips that a profile table of one model starts from, taken when it opens. */
export type CatalogChips = readonly CatalogPredicate[];

/** The catalog's default chips, "Runs on · this site", for a table opened without the catalog's. */
export const defaultCatalogChips: CatalogChips = catalogViewCodec.defaultView.predicates;

/**
 * The chips a profile table of one model starts with and returns to on Reset: the catalog's
 * profile chips and, once the table's read offers the Runs on field, its Runs on chip, leading the
 * others. Runs on keeps the scopes the field offers, else falls back to this site. The catalog's
 * search, sort and columns stay behind, since its search matches models rather than profiles. A
 * chip that would hide the named profile, such as one a dialog opens for, is left out.
 */
export function profileTableStart(
  chips: CatalogChips,
  runsOn: DataTableField<ProfileRow, RunsOnField> | undefined,
  named?: ProfileRow,
): DataTablePredicate<ProfileTableField>[] {
  const runsOnChip = chips.find(({ field }) => field === "runsOn");
  const start = [
    ...(runsOn && runsOnChip
      ? [{ field: "runsOn" as const, values: offered(runsOn, runsOnChip.values) }]
      : []),
    ...chips
      .filter(({ field }) => field !== "runsOn")
      .map(({ field, values }) => ({ field, values: [...values] })),
  ];
  return named ? start.filter((chip) => chipShows(chip, named, runsOn)) : start;
}

/**
 * Whether a profile table starting with the chips shows the profile, judging a Runs on chip by
 * the field when given.
 */
export function startShows(
  start: readonly DataTablePredicate<ProfileTableField>[],
  profile: ProfileRow,
  runsOn?: DataTableField<ProfileRow, RunsOnField>,
): boolean {
  return start.every((chip) => chipShows(chip, profile, runsOn));
}

// A chip on a field the table doesn't know, such as Runs on before its read, hides nothing.
function chipShows(
  { field, values }: DataTablePredicate<ProfileTableField>,
  profile: ProfileRow,
  runsOn: DataTableField<ProfileRow, RunsOnField> | undefined,
): boolean {
  const definition = field === "runsOn" ? runsOn : profileFields.find(({ id }) => id === field);
  return !definition || matchesField(definition, profile, values);
}

// The chip's values the field offers; when it offers none of them, this site.
function offered(
  runsOn: DataTableField<ProfileRow, RunsOnField>,
  values: readonly string[],
): string[] {
  const { domain } = runsOn;
  const known = domain.kind === "closed" ? domain.values.map(({ value }) => value) : [];
  const kept = values.filter((value) => known.includes(value));
  return values.length && !kept.length ? [runsOnScopeKey({ kind: "site" })] : kept;
}
