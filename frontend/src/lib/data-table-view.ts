import { panelParameter } from "@/lib/panel-stack";

/** The parts of a data table definition that its URL view reads. */
export interface DataTableViewDefinition<C extends string, F extends string> {
  /** Columns in display order. */
  columns: readonly DataTableViewColumn<C>[];
  /**
   * The chips of the default view. A view listing other chips, even none, replaces them, so the
   * URL records a removed default chip.
   */
  defaultPredicates?: readonly DataTablePredicate<F>[];
  /**
   * The sort of the default view, on a sortable column that is pinned or shown by default. While
   * the column is hidden, the view sorts by the first pinned sortable column, ascending.
   */
  defaultSort: DataTableSort<C>;
  /** Filter fields in the Filter menu's order. */
  fields: readonly DataTableViewField<F>[];
}

export interface DataTableViewColumn<C extends string> {
  /** Whether a view without `cols` shows the column. Pinned columns always show. */
  defaultVisible?: boolean;
  id: C;
  /** The column's value in `cols`. */
  key: string;
  label: string;
  /** An always-visible column that can't be hidden. */
  pinned?: boolean;
  /** A sortable column; its value in `sort` defaults to the column key. */
  sort?: { key?: string };
}

export interface DataTableViewField<F extends string> {
  domain: DataTableValueDomain;
  id: F;
  /** The URL key repeating the field's selected values, also its value in `f`. */
  key: string;
  label: string;
}

/**
 * The values a filter field offers: a closed list, whose unknown URL values are dropped, or open
 * values read from the table data, labeled by an optional function.
 */
export type DataTableValueDomain =
  | { kind: "closed"; values: readonly { label: string; value: string }[] }
  | { kind: "open"; label?: (value: string) => string };

export type DataTableSortDirection = "asc" | "desc";

export interface DataTableSort<C extends string> {
  column: C;
  direction: DataTableSortDirection;
}

/** A shown filter chip. A predicate without values is shown and matches everything. */
export interface DataTablePredicate<F extends string> {
  field: F;
  values: string[];
}

/** Everything about a data table a URL can carry. */
export interface DataTableView<C extends string, F extends string> {
  /** Visible columns in display order, pinned columns included. */
  columns: C[];
  /** Shown chips in display order. */
  predicates: DataTablePredicate<F>[];
  search: string;
  sort: DataTableSort<C>;
}

/**
 * Reads and writes one table's view in a location search. Defaults are omitted, so the default
 * view writes nothing, and unknown fields, values, columns, and sorts are ignored.
 */
export interface DataTableViewCodec<C extends string, F extends string> {
  readonly defaultColumns: readonly C[];
  readonly defaultView: DataTableView<C, F>;
  /**
   * Replaces the view parameters of `search` with `view`, leaving every other parameter. The view
   * comes before the panel parameters, the order the panel helpers write.
   */
  apply(search: string | URLSearchParams, view: DataTableView<C, F>): string;
  /** Whether the view equals the default view, so no Reset control is needed. */
  isDefault(view: DataTableView<C, F>): boolean;
  /** Reads the view of a search. A sort on a hidden column falls back to the default sort. */
  parse(search: string | URLSearchParams): DataTableView<C, F>;
  /** Writes the view as a search string, empty for the default view. */
  serialize(view: DataTableView<C, F>): string;
  /** Sets the visible columns in display order, keeping pinned ones and resetting a hidden sort. */
  setColumns(view: DataTableView<C, F>, columns: readonly C[]): DataTableView<C, F>;
}

/**
 * Builds the URL codec of a table definition. Throws when a key is malformed, a field key collides
 * with a reserved or another field key, two columns or sorts share a key, the default sort is not
 * on a sortable column shown by default, or the table has no pinned sortable column to fall back to.
 */
export function createDataTableViewCodec<C extends string, F extends string>(
  definition: DataTableViewDefinition<C, F>,
): DataTableViewCodec<C, F> {
  validateDefinition(definition);

  const { columns, defaultSort, fields } = definition;
  const defaultColumns = columns.filter(isDefaultColumn).map(({ id }) => id);
  const defaultPredicates = definition.defaultPredicates ?? [];
  const defaultView: DataTableView<C, F> = {
    columns: [...defaultColumns],
    predicates: copyPredicates(defaultPredicates),
    search: "",
    sort: defaultSort,
  };
  // Validated above: a pinned sortable column exists.
  const pinnedSort = pinnedSortOf(columns)!;
  const viewKeys = [searchKey, fieldsKey, sortKey, columnsKey, ...fields.map(({ key }) => key)];

  // The sort of a view showing these columns, which a view writes nothing for.
  const defaultSortFor = (visible: readonly C[]): DataTableSort<C> =>
    visible.includes(defaultSort.column) ? defaultSort : pinnedSort;

  const normalize = (view: DataTableView<C, F>): DataTableView<C, F> => {
    const visible = columns
      .filter(({ id, pinned }) => pinned || view.columns.includes(id))
      .map(({ id }) => id);
    const sorted = columns.find(({ id }) => id === view.sort.column);
    return {
      columns: visible,
      predicates: view.predicates,
      search: view.search,
      sort: sorted?.sort && visible.includes(sorted.id) ? view.sort : defaultSortFor(visible),
    };
  };

  const viewParams = (view: DataTableView<C, F>): URLSearchParams => {
    const normalized = normalize(view);
    const params = new URLSearchParams();

    if (normalized.search) params.set(searchKey, normalized.search);
    if (!samePredicates(normalized.predicates, defaultPredicates)) {
      const keyed = normalized.predicates.map(({ field, values }) => ({
        key: fieldById(fields, field).key,
        values,
      }));
      params.set(fieldsKey, keyed.map(({ key }) => key).join(","));
      for (const { key, values } of keyed) {
        for (const value of values) params.append(key, value);
      }
    }
    if (!sameSort(normalized.sort, defaultSortFor(normalized.columns))) {
      const column = columns.find(({ id }) => id === normalized.sort.column)!;
      params.set(
        sortKey,
        `${normalized.sort.direction === "desc" ? "-" : ""}${columnSortKey(column)}`,
      );
    }
    if (normalized.columns.join() !== defaultColumns.join()) {
      params.set(
        columnsKey,
        normalized.columns.map((id) => columns.find((column) => column.id === id)!.key).join(","),
      );
    }
    return params;
  };

  const serialize = (view: DataTableView<C, F>): string => searchString(viewParams(view));

  return {
    defaultColumns,
    defaultView,
    apply: (search, view) => {
      const params = new URLSearchParams(search);
      const panels = params.getAll(panelParameter);
      for (const key of viewKeys) params.delete(key);
      params.delete(panelParameter);
      for (const [key, value] of viewParams(view)) params.append(key, value);
      for (const panel of panels) params.append(panelParameter, panel);
      return searchString(params);
    },
    isDefault: (view) => serialize(view) === "",
    parse: (search) => {
      const params = new URLSearchParams(search);
      const listed = (params.get(fieldsKey) ?? "")
        .split(",")
        .flatMap((key) => fields.find((field) => field.key === key) ?? []);
      const withValues = fields.filter(({ key }) => params.has(key));
      // A search naming chips, even none, replaces the default chips.
      const explicit = params.has(fieldsKey) || withValues.length > 0;
      const predicates = explicit
        ? unique([...listed, ...withValues]).map((field) => ({
            field: field.id,
            values: domainValues(field.domain, params.getAll(field.key)),
          }))
        : copyPredicates(defaultPredicates);

      return normalize({
        columns: parseColumns(columns, defaultColumns, params.get(columnsKey)),
        predicates,
        search: params.get(searchKey) ?? "",
        sort: parseSort(columns, defaultSort, params.get(sortKey)),
      });
    },
    serialize,
    setColumns: (view, visible) => {
      const followed = sameSort(view.sort, defaultSortFor(view.columns));
      const next = normalize({ ...view, columns: [...visible] });
      // A view that shows the default sort keeps showing it as columns hide and return.
      return followed ? { ...next, sort: defaultSortFor(next.columns) } : next;
    },
  };
}

const searchKey = "q";
const fieldsKey = "f";
const sortKey = "sort";
const columnsKey = "cols";

// URL keys every data table view owns or must leave alone.
const reservedKeys: readonly string[] = [searchKey, fieldsKey, sortKey, columnsKey, panelParameter];

// Keys travel in URL lists (`f`, `cols`) and after the `-` of a descending sort.
const keyPattern = /^[a-z0-9][a-z0-9_-]*$/i;

function validateDefinition<C extends string, F extends string>(
  definition: DataTableViewDefinition<C, F>,
): void {
  const fieldKeys = definition.fields.map(({ key }) => key);
  const columnKeys = definition.columns.map(({ key }) => key);
  const sortKeys = definition.columns.flatMap((column) =>
    column.sort ? [columnSortKey(column)] : [],
  );

  for (const key of [...fieldKeys, ...columnKeys, ...sortKeys]) {
    if (!keyPattern.test(key)) throw new Error(`Data table key "${key}" is not a plain URL key.`);
  }
  for (const key of fieldKeys) {
    if (reservedKeys.includes(key)) {
      throw new Error(`Data table field key "${key}" collides with a reserved view key.`);
    }
  }
  for (const [kind, keys] of [
    ["field", fieldKeys],
    ["column", columnKeys],
    ["sort", sortKeys],
  ] as const) {
    const duplicate = keys.find((key, index) => keys.indexOf(key) !== index);
    if (duplicate !== undefined) {
      throw new Error(`Data table ${kind} key "${duplicate}" is used more than once.`);
    }
  }

  const sorted = definition.columns.find(({ id }) => id === definition.defaultSort.column);
  if (!sorted?.sort || !isDefaultColumn(sorted)) {
    throw new Error(
      `Data table default sort "${definition.defaultSort.column}" is not a sortable column shown by default.`,
    );
  }
  if (!pinnedSortOf(definition.columns)) {
    throw new Error("Data table has no pinned sortable column to fall back to.");
  }
}

// The sort a view falls back to while its default sort's column is hidden.
function pinnedSortOf<C extends string>(
  columns: readonly DataTableViewColumn<C>[],
): DataTableSort<C> | undefined {
  const pinned = columns.find((column) => column.pinned && column.sort);
  return pinned && { column: pinned.id, direction: "asc" };
}

function copyPredicates<F extends string>(
  predicates: readonly DataTablePredicate<F>[],
): DataTablePredicate<F>[] {
  return predicates.map(({ field, values }) => ({ field, values: [...values] }));
}

function samePredicates<F extends string>(
  left: readonly DataTablePredicate<F>[],
  right: readonly DataTablePredicate<F>[],
): boolean {
  return (
    left.length === right.length &&
    left.every(
      (predicate, index) =>
        predicate.field === right[index].field &&
        predicate.values.length === right[index].values.length &&
        predicate.values.every((value, at) => value === right[index].values[at]),
    )
  );
}

function isDefaultColumn<C extends string>(column: DataTableViewColumn<C>): boolean {
  return Boolean(column.pinned || column.defaultVisible);
}

function columnSortKey<C extends string>(column: DataTableViewColumn<C>): string {
  return column.sort?.key ?? column.key;
}

function fieldById<F extends string>(
  fields: readonly DataTableViewField<F>[],
  id: F,
): DataTableViewField<F> {
  return fields.find((field) => field.id === id)!;
}

function parseColumns<C extends string>(
  columns: readonly DataTableViewColumn<C>[],
  defaultColumns: readonly C[],
  value: string | null,
): C[] {
  if (value === null) return [...defaultColumns];
  const keys = value.split(",");
  return columns.filter(({ key }) => keys.includes(key)).map(({ id }) => id);
}

function parseSort<C extends string>(
  columns: readonly DataTableViewColumn<C>[],
  defaultSort: DataTableSort<C>,
  value: string | null,
): DataTableSort<C> {
  if (!value) return defaultSort;
  const direction = value.startsWith("-") ? "desc" : "asc";
  const key = value.replace(/^-/, "");
  const column = columns.find((candidate) => candidate.sort && columnSortKey(candidate) === key);
  return column ? { column: column.id, direction } : defaultSort;
}

function domainValues(domain: DataTableValueDomain, values: readonly string[]): string[] {
  const known =
    domain.kind === "closed"
      ? values.filter((value) => domain.values.some((candidate) => candidate.value === value))
      : values.filter(Boolean);
  return unique(known);
}

function sameSort<C extends string>(left: DataTableSort<C>, right: DataTableSort<C>): boolean {
  return left.column === right.column && left.direction === right.direction;
}

function searchString(params: URLSearchParams): string {
  const serialized = params.toString();
  return serialized ? `?${serialized}` : "";
}

function unique<T>(values: readonly T[]): T[] {
  return [...new Set(values)];
}
