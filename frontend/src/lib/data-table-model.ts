import type {
  DataTablePredicate,
  DataTableSortDirection,
  DataTableView,
  DataTableViewColumn,
  DataTableViewDefinition,
  DataTableViewField,
} from "@/lib/data-table-view";

/**
 * A data table: rows, their optional children, the columns that show and sort them, and the fields
 * that filter them. Filters, sort values, and facet counts read the matching unit: each child of a
 * table with children, or the row itself in a table without.
 */
export interface DataTableDefinition<
  Row,
  Unit,
  C extends string,
  F extends string,
> extends DataTableViewDefinition<C, F> {
  /**
   * A row's children. Without it, each row is its own matching unit (`Unit` is `Row`), and rows
   * neither expand nor hide children.
   */
  children?: (row: Row) => readonly Unit[];
  columns: readonly DataTableColumn<Unit, C>[];
  /**
   * Orders children after the sort value, and alone under a name sort, whatever its direction.
   * Children keep their declared order without it.
   */
  compareChildren?: (left: Unit, right: Unit) => number;
  fields: readonly DataTableField<Unit, F>[];
  rowId: (row: Row) => string;
  /** The display name, which the name sort compares ignoring case and accents. */
  rowName: (row: Row) => string;
  /** Texts the search matches, ignoring case. */
  searchText: (row: Row) => readonly string[];
}

export interface DataTableColumn<Unit, C extends string> extends DataTableViewColumn<C> {
  sort?: DataTableColumnSort<Unit>;
}

/**
 * How a column sorts rows. A name sort orders rows by display name, then id. A value sort orders
 * rows by their lowest matching value ascending and their highest descending, missing values last,
 * then by name, and orders each row's children by their own value. A text sort does the same on
 * strings, comparing ignoring case and accents.
 */
export type DataTableColumnSort<Unit> =
  | { key?: string; kind: "name" }
  | { key?: string; kind: "value"; value: (unit: Unit) => number | undefined }
  | { key?: string; kind: "text"; value: (unit: Unit) => string | undefined };

/**
 * A filter field. Values of an "any" field are alternatives; values of an "all" field must all hold
 * on the same matching unit.
 */
export interface DataTableField<Unit, F extends string> extends DataTableViewField<F> {
  match: "all" | "any";
  values: (unit: Unit) => readonly string[];
}

/** A shown row with its matching children, in display order. */
export interface DataTableRow<Row, Child> {
  /** Matching children in display order; always empty in a table without children. */
  children: Child[];
  /** Whether the row expands: at least one child matches. */
  expandable: boolean;
  /** Children hidden by the filters. */
  hiddenChildCount: number;
  row: Row;
}

/** The ordered rows, counts, and filter menus of a table for one view. */
export interface DataTableResult<R, F extends string> {
  facets: Record<F, FacetValue[]>;
  rows: R[];
  shownCount: number;
  totalCount: number;
}

/** One value of a filter chip menu and the number of rows it would show. */
export interface FacetValue {
  count: number;
  label: string;
  value: string;
}

export interface ValueRange {
  max: number;
  min: number;
}

/**
 * Searches, filters, and orders rows for a view, and counts each filter value. A row shows when one
 * of its units matches every predicate; with no selected filter value, a row without children
 * shows too. Facet counts apply the search and every other predicate: an "any" field drops its own
 * selection, an "all" field keeps its other selected values. A selected value missing from the data
 * stays listed with a zero count, so it can be unselected.
 */
export function buildDataTable<Row, Unit, C extends string, F extends string>(
  definition: DataTableDefinition<Row, Unit, C, F>,
  rows: readonly Row[],
  view: DataTableView<C, F>,
): DataTableResult<DataTableRow<Row, Unit>, F> {
  const hasChildren = Boolean(definition.children);
  const sort = sortOf(definition, view);
  const filtered = view.predicates.some((predicate) => predicate.values.length > 0);
  const compareChildren = definition.compareChildren ?? (() => 0);

  const shown = rows
    .filter((row) => matchesSearch(definition, row, view.search))
    .flatMap((row) => {
      const units = unitsOf(definition, row);
      const matching = units.filter((unit) => matchesPredicates(definition, unit, view.predicates));
      return matching.length || !filtered ? [{ matching, row, units }] : [];
    })
    .sort(
      (left, right) =>
        compareRows(definition, sort, left, right) || compareNames(definition, left.row, right.row),
    );

  return {
    facets: facets(definition, rows, view),
    rows: shown.map(({ matching, row, units }) => ({
      children: hasChildren
        ? [...matching].sort(
            (left, right) => compareUnits(sort, left, right) || compareChildren(left, right),
          )
        : [],
      expandable: hasChildren && matching.length > 0,
      hiddenChildCount: units.length - matching.length,
      row,
    })),
    shownCount: shown.length,
    totalCount: rows.length,
  };
}

/** The lowest and highest present value, or undefined without one. */
export function valueRange(values: readonly (number | undefined)[]): ValueRange | undefined {
  const present = values.filter((value) => value !== undefined);
  return present.length ? { max: Math.max(...present), min: Math.min(...present) } : undefined;
}

/**
 * Whether a unit matches a chip of the field holding `values`: any of them, or every one for an
 * "all" field. A chip without values matches every unit.
 */
export function matchesField<Unit, F extends string>(
  field: DataTableField<Unit, F>,
  unit: Unit,
  values: readonly string[],
): boolean {
  if (!values.length) return true;

  const carried = field.values(unit);
  return field.match === "all"
    ? values.every((value) => carried.includes(value))
    : values.some((value) => carried.includes(value));
}

/** Formats a range as "min–max", or one value when both ends read the same. */
export function formatRange(range: ValueRange, format: (value: number) => string): string {
  const min = format(range.min);
  const max = format(range.max);
  return min === max ? min : `${min}–${max}`;
}

type ResolvedSort<Unit> =
  | { direction: DataTableSortDirection; kind: "name" }
  | {
      direction: DataTableSortDirection;
      kind: "value";
      value: (unit: Unit) => number | undefined;
    }
  | {
      direction: DataTableSortDirection;
      kind: "text";
      value: (unit: Unit) => string | undefined;
    };

interface MatchedRow<Row, Unit> {
  matching: readonly Unit[];
  row: Row;
  units: readonly Unit[];
}

// A view sorted on an unsortable or unknown column orders rows by name.
function sortOf<Row, Unit, C extends string, F extends string>(
  definition: DataTableDefinition<Row, Unit, C, F>,
  view: DataTableView<C, F>,
): ResolvedSort<Unit> {
  const sort = definition.columns.find(({ id }) => id === view.sort.column)?.sort;
  return sort?.kind === "value" || sort?.kind === "text"
    ? ({ direction: view.sort.direction, kind: sort.kind, value: sort.value } as ResolvedSort<Unit>)
    : { direction: sort ? view.sort.direction : "asc", kind: "name" };
}

function unitsOf<Row, Unit, C extends string, F extends string>(
  definition: DataTableDefinition<Row, Unit, C, F>,
  row: Row,
): readonly Unit[] {
  // Without children, the definition's Unit is its Row.
  return definition.children ? definition.children(row) : [row as unknown as Unit];
}

function matchesSearch<Row, Unit, C extends string, F extends string>(
  definition: DataTableDefinition<Row, Unit, C, F>,
  row: Row,
  search: string,
): boolean {
  const needle = search.trim().toLowerCase();
  return !needle || definition.searchText(row).some((text) => text.toLowerCase().includes(needle));
}

function matchesPredicates<Row, Unit, C extends string, F extends string>(
  definition: DataTableDefinition<Row, Unit, C, F>,
  unit: Unit,
  predicates: readonly DataTablePredicate<F>[],
  skip?: { field: F; value: string },
): boolean {
  return predicates.every((predicate) => {
    const field = definition.fields.find(({ id }) => id === predicate.field);
    if (!field) return true;

    const values =
      skip?.field === predicate.field
        ? // The facet's own field gives way to the counted value; an "all" field keeps the rest.
          field.match === "all"
          ? predicate.values.filter((value) => value !== skip.value)
          : []
        : predicate.values;
    return matchesField(field, unit, values);
  });
}

function facets<Row, Unit, C extends string, F extends string>(
  definition: DataTableDefinition<Row, Unit, C, F>,
  rows: readonly Row[],
  view: DataTableView<C, F>,
): Record<F, FacetValue[]> {
  const searched = rows.filter((row) => matchesSearch(definition, row, view.search));
  const count = (field: DataTableField<Unit, F>, value: string): number =>
    searched.filter((row) =>
      unitsOf(definition, row).some(
        (unit) =>
          field.values(unit).includes(value) &&
          matchesPredicates(definition, unit, view.predicates, { field: field.id, value }),
      ),
    ).length;

  const fieldFacets = (field: DataTableField<Unit, F>): FacetValue[] => {
    const { domain } = field;
    if (domain.kind === "closed") {
      return domain.values.map(({ label, value }) => ({
        count: count(field, value),
        label,
        value,
      }));
    }

    const present = unique(
      rows.flatMap((row) => unitsOf(definition, row).flatMap((unit) => field.values(unit))),
    ).sort((left, right) => left.localeCompare(right));
    const selected = view.predicates.find((predicate) => predicate.field === field.id)?.values;
    return unique([...present, ...(selected ?? [])]).map((value) => ({
      count: count(field, value),
      label: domain.label?.(value) ?? value,
      value,
    }));
  };

  return Object.fromEntries(
    definition.fields.map((field) => [field.id, fieldFacets(field)]),
  ) as Record<F, FacetValue[]>;
}

function compareRows<Row, Unit, C extends string, F extends string>(
  definition: DataTableDefinition<Row, Unit, C, F>,
  sort: ResolvedSort<Unit>,
  left: MatchedRow<Row, Unit>,
  right: MatchedRow<Row, Unit>,
): number {
  if (sort.kind === "name") {
    const byName = compareNames(definition, left.row, right.row);
    return sort.direction === "asc" ? byName : -byName;
  }

  if (sort.kind === "text") {
    // Ascending orders rows by their lowest matching text and descending by their highest.
    const texts = (matched: MatchedRow<Row, Unit>) =>
      matched.matching.flatMap((unit) => sort.value(unit) ?? []).sort(compareTexts);
    const summarize = (matched: MatchedRow<Row, Unit>) => {
      const sorted = texts(matched);
      return sort.direction === "asc" ? sorted[0] : sorted.at(-1);
    };
    return compareOptionalTexts(summarize(left), summarize(right), sort.direction);
  }

  // Ascending orders rows by their lowest matching value and descending by their highest.
  const summary = ({ matching }: MatchedRow<Row, Unit>) => {
    const range = valueRange(matching.map(sort.value));
    return sort.direction === "asc" ? range?.min : range?.max;
  };
  return compareValues(summary(left), summary(right), sort.direction);
}

function compareUnits<Unit>(sort: ResolvedSort<Unit>, left: Unit, right: Unit): number {
  if (sort.kind === "value")
    return compareValues(sort.value(left), sort.value(right), sort.direction);
  if (sort.kind === "text") {
    return compareOptionalTexts(sort.value(left), sort.value(right), sort.direction);
  }
  return 0;
}

function compareTexts(left: string, right: string): number {
  return left.localeCompare(right, undefined, { sensitivity: "base" });
}

// A missing text sorts after every present text in both directions.
function compareOptionalTexts(
  left: string | undefined,
  right: string | undefined,
  direction: DataTableSortDirection,
): number {
  if (left === undefined || right === undefined) {
    return left === right ? 0 : left === undefined ? 1 : -1;
  }
  return direction === "asc" ? compareTexts(left, right) : compareTexts(right, left);
}

function compareNames<Row, Unit, C extends string, F extends string>(
  definition: DataTableDefinition<Row, Unit, C, F>,
  left: Row,
  right: Row,
): number {
  return (
    definition
      .rowName(left)
      .localeCompare(definition.rowName(right), undefined, { sensitivity: "base" }) ||
    definition.rowId(left).localeCompare(definition.rowId(right))
  );
}

// A missing value sorts after every present value in both directions.
function compareValues(
  left: number | undefined,
  right: number | undefined,
  direction: DataTableSortDirection,
): number {
  if (left === undefined || right === undefined) {
    return left === right ? 0 : left === undefined ? 1 : -1;
  }
  return direction === "asc" ? left - right : right - left;
}

function unique<T>(values: readonly T[]): T[] {
  return [...new Set(values)];
}
