import type { LucideIcon } from "lucide-react";
import {
  ArrowDownIcon,
  ArrowUpIcon,
  ChevronRightIcon,
  ChevronsUpDownIcon,
  EyeOffIcon,
} from "lucide-react";
import {
  type KeyboardEvent,
  type MouseEvent,
  type ReactNode,
  type Ref,
  useId,
  useState,
} from "react";

import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import type {
  DataTableDefinition,
  DataTableResult,
  DataTableRow,
  FacetValue,
} from "@/lib/data-table-model";
import type {
  DataTableSort,
  DataTableSortDirection,
  DataTableView,
  DataTableViewCodec,
} from "@/lib/data-table-view";
import { formatCount } from "@/lib/format";
import { cn } from "@/lib/utils";
import { usePanelNavigationPending } from "@/panels/panel-navigation-pending";

/**
 * A data table definition with what the table and toolbar render: cells per column, an icon and
 * value rendering per filter field, and the nouns their texts read. `T` is the table result the
 * cells receive, which may extend each row with summarized values.
 */
export interface DataTableUiDefinition<
  Row,
  Unit,
  C extends string,
  F extends string,
  T extends DataTableResult<DataTableRow<Row, unknown>, F> = DataTableResult<
    DataTableRow<Row, Unit>,
    F
  >,
> {
  /** Child rows of a table whose definition has children. */
  child?: DataTableChildUi<T>;
  codec: DataTableViewCodec<C, F>;
  columns: Record<C, DataTableColumnUi<T>>;
  definition: DataTableDefinition<Row, Unit, C, F>;
  fields: Record<F, DataTableFieldUi>;
  /** Whether the columns never change: the toolbar has no View menu and headers only sort. */
  fixedColumns?: boolean;
  /** The table's accessible name. */
  label: string;
  /** What a row is, for the footer, search, facet counts, and the no-match text. */
  noun: DataTableNoun;
  /** The id of the panel rows open, which rows name in `aria-controls`. */
  panelId?: string;
}

export interface DataTableColumnUi<T extends ResultRows> {
  cell: (row: RowOf<T>, table: T) => ReactNode;
  childCell?: (child: ChildOf<T>, table: T) => ReactNode;
  /** Shows the column only once the table is at least this wide, so narrow tables keep what matters. */
  showFrom?: DataTableShowFrom;
}

/** Table widths at which a column appears; each maps to a fixed class so Tailwind sees it. */
export type DataTableShowFrom = keyof typeof showFromClasses;

export interface DataTableChildUi<T extends ResultRows> {
  id: (child: ChildOf<T>) => string;
  name: (child: ChildOf<T>) => string;
  /** What a child is, for the expand control and the row's accessible texts. */
  noun: DataTableNoun;
}

export interface DataTableFieldUi {
  icon: LucideIcon;
  /**
   * Makes a plain click on a value select only that value, for a field whose values are
   * alternatives rather than parts of a set. Shift-click still adds or removes a value.
   */
  replaceOnSelect?: boolean;
  /**
   * Renders a value in the field's menu, such as an identifier in a monospace font. The result is a
   * flex item, so it sets its own type size and truncation.
   */
  renderValue?: (facet: FacetValue) => ReactNode;
}

export interface DataTableNoun {
  one: string;
  other: string;
}

/** The row, or one of its children, whose panel is open. */
export interface DataTableSelection {
  childId?: string;
  rowId: string;
}

/**
 * Single-row selection: each row leads with a radio of one group, and clicking a row selects it.
 * Arrow keys move the selection between enabled rows. A disabled row stays listed, but its radio
 * can't be focused or selected.
 */
export interface DataTableRowSelection<R> {
  disabled?: (row: R) => boolean;
  /** Receives the selected row's radio, or the first enabled radio without one, for focusing. */
  inputRef?: Ref<HTMLInputElement>;
  onBlur?: () => void;
  onSelect: (row: R) => void;
  selectedId?: string;
}

export interface DataTableProps<
  Row,
  Unit,
  C extends string,
  F extends string,
  T extends DataTableResult<DataTableRow<Row, unknown>, F>,
> {
  /** Replaces the no-match text when no row shows, such as when the default view hides every row. */
  empty?: ReactNode;
  /** Follows the footer's row count, such as what a filter hides beyond the count. */
  footnote?: ReactNode;
  onOpenChild?: (row: RowOf<T>, child: ChildOf<T>, opener: HTMLElement) => void;
  /** Opens a row's panel; a table with `select` selects rows instead. */
  onOpenRow?: (row: RowOf<T>, opener: HTMLElement) => void;
  onViewChange: (view: DataTableView<C, F>) => void;
  select?: DataTableRowSelection<RowOf<T>>;
  selection?: DataTableSelection;
  table: T;
  ui: DataTableUiDefinition<Row, Unit, C, F, T>;
  view: DataTableView<C, F>;
}

/**
 * A definition-driven table: one row per shown row, expandable into its matching children when the
 * definition has children, with header menus that sort and hide columns and a footer counting the
 * shown rows, with an optional footnote. Clicking or pressing Enter on a row opens its panel; the chevron only expands the row.
 * With `select`, rows are single-selection radios instead of panel openers.
 */
export function DataTable<
  Row,
  Unit,
  C extends string,
  F extends string,
  T extends DataTableResult<DataTableRow<Row, unknown>, F>,
>({
  empty,
  footnote,
  onOpenChild,
  onOpenRow,
  onViewChange,
  select,
  selection,
  table,
  ui,
  view,
}: DataTableProps<Row, Unit, C, F, T>) {
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(() => new Set());
  const radioGroup = useId();
  const navigationPending = usePanelNavigationPending();
  const { definition } = ui;
  // Rows expand only when the definition has children.
  const child = definition.children ? ui.child : undefined;
  const columns = view.columns.map((id) => definition.columns.find((column) => column.id === id)!);
  const columnSpan = columns.length + (select ? 1 : 0);
  // The radio that takes focus: the selected row's when shown and enabled, else the first enabled.
  const enabledRowIds = select
    ? table.rows.filter((row) => !select.disabled?.(row)).map((row) => definition.rowId(row.row))
    : [];
  const focusRowId = enabledRowIds.find((id) => id === select?.selectedId) ?? enabledRowIds[0];

  const toggle = (rowId: string) =>
    setExpanded((current) => {
      const next = new Set(current);
      if (!next.delete(rowId)) next.add(rowId);
      return next;
    });
  const opener = (open: (element: HTMLElement) => void) => ({
    onClick: (event: MouseEvent<HTMLElement>) => {
      if (!navigationPending) open(event.currentTarget);
    },
    onKeyDown: (event: KeyboardEvent<HTMLElement>) => {
      if (event.target !== event.currentTarget || (event.key !== "Enter" && event.key !== " "))
        return;
      event.preventDefault();
      if (!navigationPending) open(event.currentTarget);
    },
  });

  return (
    // A size container, so the empty-table text can span the visible width of a scrolled table.
    <div className="@container min-w-0 overflow-hidden rounded-xl border border-border bg-card">
      <Table aria-label={ui.label}>
        <TableHeader>
          <TableRow className="hover:bg-transparent has-aria-expanded:bg-transparent">
            {select ? (
              // As wide as a row's radio cell, so headers stay put when no row shows.
              <TableHead className="h-auto w-0 min-w-7 py-2 pr-0 pl-3">
                <span className="sr-only">Selected</span>
              </TableHead>
            ) : null}
            {columns.map((column) => (
              <TableHead
                aria-sort={ariaSort(view.sort, column.id)}
                // The pinned column takes the spare width, so expanding a row doesn't move the others.
                className={cn(
                  "h-auto px-3 py-2 text-xs text-muted-foreground",
                  column.pinned && "w-full",
                  showFromClass(ui.columns[column.id]),
                )}
                key={column.id}
              >
                {column.sort ? (
                  <ColumnMenu
                    hideable={!column.pinned && !ui.fixedColumns}
                    label={column.label}
                    nameSort={column.sort.kind !== "value"}
                    onHide={() =>
                      onViewChange(
                        ui.codec.setColumns(
                          view,
                          view.columns.filter((candidate) => candidate !== column.id),
                        ),
                      )
                    }
                    onSort={(direction) =>
                      onViewChange({ ...view, sort: { column: column.id, direction } })
                    }
                    sort={view.sort.column === column.id ? view.sort.direction : undefined}
                  />
                ) : (
                  column.label
                )}
              </TableHead>
            ))}
          </TableRow>
        </TableHeader>
        <TableBody>
          {table.rows.length ? (
            table.rows.flatMap((row: RowOf<T>) => {
              const rowId = definition.rowId(row.row);
              const rowName = definition.rowName(row.row);
              const open = row.expandable && expanded.has(rowId);
              const cells = columns.map((column) => (
                <TableCell
                  className={cn("px-3 py-3 text-[13px]", showFromClass(ui.columns[column.id]))}
                  key={column.id}
                >
                  {column.pinned ? (
                    <PinnedCell
                      expansion={
                        child
                          ? {
                              expandable: row.expandable,
                              expanded: open,
                              label: `${open ? "Hide" : "Show"} ${child.noun.other} for ${rowName}`,
                              onToggle: () => toggle(rowId),
                            }
                          : undefined
                      }
                    >
                      {ui.columns[column.id].cell(row, table)}
                    </PinnedCell>
                  ) : (
                    ui.columns[column.id].cell(row, table)
                  )}
                </TableCell>
              ));
              if (select) {
                return [
                  <SelectableRow
                    disabled={Boolean(select.disabled?.(row))}
                    inputRef={rowId === focusRowId ? select.inputRef : undefined}
                    key={rowId}
                    label={rowName}
                    name={radioGroup}
                    onBlur={select.onBlur}
                    onSelect={() => select.onSelect(row)}
                    selected={rowId === select.selectedId}
                    value={rowId}
                  >
                    {cells}
                  </SelectableRow>,
                ];
              }
              return [
                <TableRow
                  aria-controls={ui.panelId}
                  aria-label={rowName}
                  className="cursor-pointer data-[state=selected]:bg-accent"
                  data-panel-navigation=""
                  data-state={
                    selection?.rowId === rowId && !selection.childId ? "selected" : undefined
                  }
                  key={rowId}
                  tabIndex={0}
                  {...opener((element) => onOpenRow?.(row, element))}
                >
                  {cells}
                </TableRow>,
                ...(open && child
                  ? row.children.map((unit: ChildOf<T>) => {
                      const childId = child.id(unit);
                      return (
                        <TableRow
                          aria-controls={ui.panelId}
                          aria-label={`${rowName} · ${child.name(unit)}`}
                          className="cursor-pointer bg-background/40 data-[state=selected]:bg-accent"
                          data-panel-navigation=""
                          data-state={
                            selection?.rowId === rowId && selection.childId === childId
                              ? "selected"
                              : undefined
                          }
                          key={`${rowId}/${childId}`}
                          tabIndex={0}
                          {...opener((element) => onOpenChild?.(row, unit, element))}
                        >
                          {columns.map((column) => {
                            const content = ui.columns[column.id].childCell?.(unit, table);
                            return (
                              <TableCell
                                className={cn(
                                  "px-3 py-2 text-[12.5px]",
                                  showFromClass(ui.columns[column.id]),
                                )}
                                key={column.id}
                              >
                                {column.pinned ? (
                                  // Indented past the parent's chevron, so names line up.
                                  <div className="min-w-40 pl-[calc(var(--console-control-xs)+--spacing(2))] whitespace-normal @lg:min-w-65">
                                    {content}
                                  </div>
                                ) : (
                                  content
                                )}
                              </TableCell>
                            );
                          })}
                        </TableRow>
                      );
                    })
                  : []),
              ];
            })
          ) : (
            <TableRow className="hover:bg-transparent">
              <TableCell
                className="px-3 py-8 text-center text-[13px] text-muted-foreground"
                colSpan={columnSpan}
              >
                {/* Pinned to the visible width, so the text stays in view when the table scrolls. */}
                <div className="sticky left-3 w-[calc(100cqw-1.5rem)] whitespace-normal">
                  {empty ?? (
                    <>
                      {`No ${ui.noun.other} match.`}{" "}
                      <Button
                        onClick={() =>
                          onViewChange({
                            ...view,
                            predicates: ui.codec.defaultView.predicates,
                            search: "",
                          })
                        }
                        size="xs"
                        type="button"
                        variant="link"
                      >
                        Reset filters
                      </Button>
                    </>
                  )}
                </div>
              </TableCell>
            </TableRow>
          )}
        </TableBody>
      </Table>
      <div className="border-t border-border px-3.5 py-2.5 text-xs text-muted-foreground">
        {`${table.shownCount} of ${formatCount(table.totalCount, ui.noun.one, ui.noun.other)}`}
        {footnote ? <> · {footnote}</> : null}
      </div>
    </div>
  );
}

// The rows of a table result, which cell renderers and open handlers receive.
interface ResultRows {
  rows: readonly DataTableRow<unknown, unknown>[];
}

type RowOf<T extends ResultRows> = T["rows"][number];

type ChildOf<T extends ResultRows> = RowOf<T>["children"][number];

/**
 * A row of a selection table: a leading radio that selects it, and a click anywhere on the row that
 * does the same. A disabled row reads muted and ignores clicks.
 */
function SelectableRow({
  children,
  disabled,
  inputRef,
  label,
  name,
  onBlur,
  onSelect,
  selected,
  value,
}: {
  children: ReactNode;
  disabled: boolean;
  inputRef?: Ref<HTMLInputElement>;
  label: string;
  name: string;
  onBlur?: () => void;
  onSelect: () => void;
  selected: boolean;
  value: string;
}) {
  return (
    <TableRow
      aria-disabled={disabled || undefined}
      aria-label={label}
      className="cursor-pointer data-[selected=true]:bg-accent aria-disabled:cursor-not-allowed aria-disabled:text-muted-foreground aria-disabled:hover:bg-transparent"
      data-selected={selected}
      onClick={() => {
        if (!disabled && !selected) onSelect();
      }}
    >
      <TableCell className="w-0 py-3 pr-0 pl-3">
        {/* Chrome ignores a native radio's border radius, so a round wrapper draws its focus ring. */}
        <span className="flex size-4 rounded-full has-focus-visible:ring-2 has-focus-visible:ring-ring has-focus-visible:ring-offset-2 has-focus-visible:ring-offset-background">
          <input
            aria-label={label}
            checked={selected}
            className="size-4 cursor-[inherit] accent-primary outline-none"
            disabled={disabled}
            name={name}
            onBlur={onBlur}
            onChange={onSelect}
            // The row's own click handler would select a second time.
            onClick={(event) => event.stopPropagation()}
            ref={inputRef}
            type="radio"
            value={value}
          />
        </span>
      </TableCell>
      {children}
    </TableRow>
  );
}

const showFromClasses = {
  "32rem": "hidden @min-[32rem]:table-cell",
  "34rem": "hidden @min-[34rem]:table-cell",
  "40rem": "hidden @min-[40rem]:table-cell",
  "46rem": "hidden @min-[46rem]:table-cell",
  "56rem": "hidden @min-[56rem]:table-cell",
  "64rem": "hidden @min-[64rem]:table-cell",
} as const;

function showFromClass(column: { showFrom?: DataTableShowFrom }): string | undefined {
  return column.showFrom && showFromClasses[column.showFrom];
}

function ColumnMenu({
  hideable,
  label,
  nameSort,
  onHide,
  onSort,
  sort,
}: {
  hideable: boolean;
  label: string;
  nameSort: boolean;
  onHide: () => void;
  onSort: (direction: DataTableSortDirection) => void;
  sort?: DataTableSortDirection;
}) {
  const SortIcon =
    sort === "asc" ? ArrowUpIcon : sort === "desc" ? ArrowDownIcon : ChevronsUpDownIcon;
  const [ascending, descending] = nameSort ? ["A → Z", "Z → A"] : ["Low → high", "High → low"];

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        aria-label={`${label} column options`}
        data-panel-navigation=""
        render={
          <Button
            className={cn(
              "-mx-1.5 px-1.5 text-xs font-medium text-muted-foreground",
              sort && "font-semibold text-foreground",
            )}
            size="xs"
            type="button"
            variant="ghost"
          />
        }
      >
        {label}
        <SortIcon aria-hidden="true" className={cn(!sort && "opacity-55")} />
      </DropdownMenuTrigger>
      <DropdownMenuContent className="w-auto min-w-40" data-panel-navigation="">
        <DropdownMenuItem onClick={() => onSort("asc")}>
          <ArrowUpIcon aria-hidden="true" />
          {ascending}
        </DropdownMenuItem>
        <DropdownMenuItem onClick={() => onSort("desc")}>
          <ArrowDownIcon aria-hidden="true" />
          {descending}
        </DropdownMenuItem>
        {hideable ? (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuItem onClick={onHide}>
              <EyeOffIcon aria-hidden="true" />
              Hide column
            </DropdownMenuItem>
          </>
        ) : null}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/**
 * The pinned column's cell content. In a table with children, a chevron that only expands the row
 * comes first, or a spacer of its width when the row doesn't expand.
 */
function PinnedCell({
  children,
  expansion,
}: {
  children: ReactNode;
  expansion?: { expandable: boolean; expanded: boolean; label: string; onToggle: () => void };
}) {
  if (!expansion) return <div className="min-w-40 whitespace-normal @lg:min-w-65">{children}</div>;

  return (
    <div className="flex min-w-40 items-center gap-2 whitespace-normal @lg:min-w-65">
      {expansion.expandable ? (
        <Button
          aria-expanded={expansion.expanded}
          aria-label={expansion.label}
          className="text-muted-foreground"
          onClick={(event) => {
            event.stopPropagation();
            expansion.onToggle();
          }}
          onKeyDown={(event) => event.stopPropagation()}
          size="icon-xs"
          type="button"
          variant="ghost"
        >
          <ChevronRightIcon
            aria-hidden="true"
            className={cn("transition-transform", expansion.expanded && "rotate-90")}
          />
        </Button>
      ) : (
        <span aria-hidden="true" className="w-control-xs shrink-0" />
      )}
      <div className="min-w-0">{children}</div>
    </div>
  );
}

function ariaSort<C extends string>(
  sort: DataTableSort<C>,
  column: C,
): "ascending" | "descending" | undefined {
  if (sort.column !== column) return undefined;
  return sort.direction === "asc" ? "ascending" : "descending";
}
