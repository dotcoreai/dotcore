import type { KeyboardEvent, RefObject } from "react";
import { useLocation } from "react-router-dom";

import { usePanelNavigation } from "@/app/console-layout";
import { usePanelNavigationPending } from "@/app/panel-navigation-pending";
import { ResourceMarker } from "@/components/ui/resource-marker";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { activePanel, type PanelEntry, selectPanelLocation } from "@/lib/panel-stack";
import { cn } from "@/lib/utils";

/** One figure column of an overview table and the table width from which it shows. */
export interface OverviewTableColumn {
  key: string;
  label: string;
  /** Narrow tables drop the column instead of scrolling; the first columns stay longest. */
  minWidth?:
    "@sm:table-cell" | "@md:table-cell" | "@lg:table-cell" | "@xl:table-cell" | "@2xl:table-cell";
}

/** One resource of an overview table: its name and subtitle, its figures, and its sidebar. */
export interface OverviewTableRow {
  /** The figures in column order. */
  cells: readonly string[];
  colorKey: string;
  /** The sidebar the row opens. */
  entry: PanelEntry;
  id: string;
  name: string;
  subtitle: string;
}

/**
 * A plain table of the overview: a name column with a marker and a subtitle, then right-aligned
 * figures. A row opens its resource's sidebar by click or Enter and shows as selected while that
 * sidebar is open.
 */
export function OverviewTable({
  columns,
  label,
  openerRef,
  rows,
}: {
  columns: readonly OverviewTableColumn[];
  label: string;
  /** Receives the row that opened the sidebar, so focus returns to it when the sidebar closes. */
  openerRef: RefObject<HTMLElement | null>;
  rows: readonly OverviewTableRow[];
}) {
  const location = useLocation();
  const panelNavigation = usePanelNavigation();
  const navigationPending = usePanelNavigationPending();
  const active = activePanel(location.search);

  const open = (entry: PanelEntry, opener: HTMLElement) => {
    if (navigationPending) return;

    openerRef.current = opener;
    panelNavigation.navigatePanel(selectPanelLocation(location, entry));
  };

  return (
    <div className="@container min-w-0 overflow-hidden rounded-xl border border-border bg-card">
      <Table aria-label={label}>
        <TableHeader>
          <TableRow className="hover:bg-transparent">
            <TableHead className="h-auto w-full px-3 py-2 text-xs text-muted-foreground">
              Name
            </TableHead>
            {columns.map((column) => (
              <TableHead
                className={cn(
                  "h-auto px-3 py-2 text-right text-xs text-muted-foreground",
                  columnVisibility(column),
                )}
                key={column.key}
              >
                {column.label}
              </TableHead>
            ))}
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((row) => (
            <TableRow
              aria-label={row.name}
              className="cursor-pointer data-[state=selected]:bg-accent"
              data-panel-navigation=""
              data-state={
                active?.page === row.entry.page && active.resourceId === row.entry.resourceId
                  ? "selected"
                  : undefined
              }
              key={row.id}
              onClick={(event) => open(row.entry, event.currentTarget)}
              onKeyDown={(event) => openOnEnter(event, () => open(row.entry, event.currentTarget))}
              tabIndex={0}
            >
              <TableCell className="max-w-0 px-3 py-3 text-[13px]">
                <div className="truncate font-medium text-foreground" title={row.name}>
                  <ResourceMarker colorKey={row.colorKey} />
                  {row.name}
                </div>
                <div className="truncate text-[11.5px] text-console-subtle" title={row.subtitle}>
                  {row.subtitle}
                </div>
              </TableCell>
              {columns.map((column, index) => (
                <TableCell
                  className={cn(
                    "px-3 py-3 text-right text-[13px] tabular-nums",
                    columnVisibility(column),
                  )}
                  key={column.key}
                >
                  {row.cells[index]}
                </TableCell>
              ))}
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  );
}

function columnVisibility(column: OverviewTableColumn): string | undefined {
  return column.minWidth ? `hidden ${column.minWidth}` : undefined;
}

function openOnEnter(event: KeyboardEvent<HTMLElement>, open: () => void) {
  if (event.target !== event.currentTarget || (event.key !== "Enter" && event.key !== " ")) return;

  event.preventDefault();
  open();
}
