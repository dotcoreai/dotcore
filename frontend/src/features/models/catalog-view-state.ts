import type { DataTableDefinition, DataTableField } from "@/lib/data-table-model";
import {
  createDataTableViewCodec,
  type DataTablePredicate,
  type DataTableSort,
  type DataTableView,
} from "@/lib/data-table-view";

import type { ModelRow, ProfileRow } from "./catalog-model-row";
import {
  compareProfiles,
  profileFields,
  profileMetricColumns,
  type ProfilePredicateField,
  type RunsOnField,
} from "./catalog-profile-fields";
import { runsOnScopeKey } from "./profile-compatibility";

/** A column of the Catalog table, in display order. */
export type CatalogColumnId =
  | "model"
  | "intelligence"
  | "memory"
  | "maximumContext"
  | "totalContext"
  | "engine"
  | "quantization"
  | "speculative"
  | "license";

/** A filter field of the Catalog table: the shared profile fields, and Runs on when known. */
export type CatalogPredicateField = ProfilePredicateField | RunsOnField;

export type CatalogSort = DataTableSort<CatalogColumnId>;

/** A shown filter chip. A predicate without values is shown and matches everything. */
export type CatalogPredicate = DataTablePredicate<CatalogPredicateField>;

/** Everything about the Catalog table a URL can carry. */
export type CatalogViewState = DataTableView<CatalogColumnId, CatalogPredicateField>;

/**
 * The model catalog as a data table: models expanding into their profiles, which filters, sort
 * values, and facet counts read. Models sort by intelligence, highest first, and profiles by
 * memory, largest first. This definition has no Runs on field; the table adds the site's.
 */
export const catalogTableDefinition: DataTableDefinition<
  ModelRow,
  ProfileRow,
  CatalogColumnId,
  CatalogPredicateField
> = {
  children: (model) => model.profiles,
  columns: [
    {
      id: "model",
      key: "model",
      label: "Model",
      pinned: true,
      sort: { key: "name", kind: "name" },
    },
    ...profileMetricColumns,
    { id: "engine", key: "engine", label: "Engine" },
    { id: "quantization", key: "quant", label: "Quantization" },
    { id: "speculative", key: "speculative", label: "Speculative" },
    { id: "license", key: "license", label: "License" },
  ],
  compareChildren: compareProfiles,
  defaultSort: { column: "intelligence", direction: "desc" },
  fields: profileFields,
  rowId: (model) => model.modelId,
  rowName: (model) => model.displayName,
  searchText: (model) => [model.displayName, model.modelId],
};

// Stands for the site's Runs on field in the URL codec. Its open domain keeps machine values in a
// URL before the compatibility read lands; the table supplies the real field.
const runsOnPlaceholder: DataTableField<ProfileRow, RunsOnField> = {
  domain: { kind: "open" },
  id: "runsOn",
  key: "runs",
  label: "Runs on",
  match: "any",
  values: () => [],
};

/** The Catalog table with the Runs on field in its place, for a site that has model targets. */
export const catalogRunsOnTableDefinition: typeof catalogTableDefinition = {
  ...catalogTableDefinition,
  fields: [runsOnPlaceholder, ...profileFields],
};

/**
 * The Catalog view in the URL for a site that has model targets: `q`, `f`, `runs`, `engine`,
 * `quant`, `cap`, `spec`, `sort`, and `cols`. The Runs on chip's sizing scenario rides beside it in
 * `ctx` and `req` (see `sizing-scenario.ts`). The default view starts with the "Runs on · this
 * site" chip, and `f=` records that it was removed.
 */
export const catalogViewCodec = createDataTableViewCodec({
  ...catalogRunsOnTableDefinition,
  defaultPredicates: [{ field: "runsOn", values: [runsOnScopeKey({ kind: "site" })] }],
});

/**
 * The Catalog view in the URL while Runs on isn't known, such as before the compatibility read
 * lands or on a site without model targets: the same parameters, with no default chip. It still
 * reads `runs` values, so a machine in a URL survives until the field is known.
 */
export const catalogPlainViewCodec = createDataTableViewCodec(catalogRunsOnTableDefinition);
