import { type ReactNode, type Ref, useEffect, useMemo, useState } from "react";

import { DataTable, type DataTableUiDefinition } from "@/components/data-table/data-table";
import {
  type DataTableChipSetting,
  DataTableToolbar,
} from "@/components/data-table/data-table-toolbar";
import type { DataTablePredicate } from "@/components/data-table/data-table-view";
import { Button } from "@/components/ui/button";

import type { ProfileRow } from "./catalog-model-row";
import {
  ContextValue,
  IntelligenceValue,
  MemoryValue,
  ProfileNameCell,
} from "./catalog-profile-cells";
import type { CatalogHiddenReason } from "./catalog-profile-compatibility";
import {
  profileFieldUi,
  type ProfileMetricColumnId,
  runsOnFieldUi,
} from "./catalog-profile-fields";
import {
  buildProfileTable,
  type ProfileAvailability,
  type ProfileColumnId,
  profileTableCodec,
  profileTableDefinition,
  type ProfileTableField,
  type ProfileTableModel,
  type ProfileTableRunsOn,
  type ProfileTableView,
  withRunsOn,
} from "./profile-table-model";

export interface ProfileTableProps {
  /** A compatibility source; profiles it rules out are listed last and can't be selected. */
  availability?: (profile: ProfileRow) => ProfileAvailability | undefined;
  /** Settings the filter chips carry beside their values, such as the Runs on chip's "Sized for". */
  chipSettings?: Partial<Record<ProfileTableField, DataTableChipSetting>>;
  /** The metric columns to show after Profile; every one by default. */
  columns?: readonly ProfileMetricColumnId[];
  /**
   * The metric columns shown at first and after Reset. With it, the View menu shows or hides the
   * others; without it, every column shows and none can be hidden.
   */
  defaultColumns?: readonly ProfileMetricColumnId[];
  /** Disables every row, such as while a deployment is pending. */
  disabled?: boolean;
  estimatesPending: boolean;
  /** Follows the footer's profile count, given the profiles Runs on hides for their memory fit. */
  footnote?: (hiddenByFit: Record<CatalogHiddenReason, number>) => ReactNode;
  /** Receives the selected profile's radio, or the first selectable one, for focusing. */
  inputRef?: Ref<HTMLInputElement>;
  onBlur?: () => void;
  /** Opens a profile from its row; a table with `onSelect` selects rows instead. */
  onOpen?: (profileId: string, opener: HTMLElement) => void;
  /** Receives the Runs on chip's values whenever the operator changes them; empty without a chip. */
  onRunsOnChange?: (values: readonly string[]) => void;
  /** Receives the ids of the profiles the view shows, in order, after each change to them. */
  onShownChange?: (profileIds: readonly string[]) => void;
  /**
   * Makes each row a radio selecting its profile. A search or filter that hides the selected row
   * clears the selection with an empty id.
   */
  onSelect?: (profileId: string) => void;
  /** The profiles of one model, in any order. */
  profiles: readonly ProfileRow[];
  /** The Runs on filter; without it, the table has none. */
  runsOn?: ProfileRunsOn;
  selectedProfileId?: string;
  /**
   * The chips the table starts with and Reset returns to, such as those copied from the catalog.
   * A Runs on chip among them applies once `runsOn` is known, and comes back on its values whenever
   * they change; the other chips apply from the start.
   */
  start?: readonly DataTablePredicate<ProfileTableField>[];
}

/** The Runs on filter of a profile table, and what the table says when no profile runs. */
export interface ProfileRunsOn extends ProfileTableRunsOn {
  /** What the table says when no profile runs in the chip's values. */
  noneRun: (values: readonly string[]) => ProfileNoneRun;
}

/** Why no profile runs in the Runs on scope. */
export interface ProfileNoneRun {
  /** Why the profiles can't run, one line each. */
  reasons: readonly string[];
  title: string;
}

/**
 * The profiles of one model as the catalog page's table: search, the Runs on, Engine,
 * Quantization, and Capabilities filters, and sortable columns. Rows either select one
 * profile or open it. The view starts with the given chips and lives in local state, so it resets
 * when the table unmounts.
 */
export function ProfileTable({
  availability,
  chipSettings,
  columns,
  defaultColumns,
  disabled = false,
  estimatesPending,
  footnote,
  inputRef,
  onBlur,
  onOpen,
  onRunsOnChange,
  onSelect,
  onShownChange,
  profiles,
  runsOn,
  selectedProfileId,
  start,
}: ProfileTableProps) {
  const runsOnField = runsOn?.field;
  // The starting chips as one key, so new arrays with the same chips change nothing; a Runs on
  // chip waits for its field.
  const startKey = JSON.stringify(
    (start ?? []).filter(({ field }) => runsOnField || field !== "runsOn"),
  );
  // The table's definition depends on the field, not on the host's empty-state copy.
  const ui = useMemo(
    () =>
      profileTableUi(
        runsOnField && { field: runsOnField },
        columns,
        defaultColumns,
        JSON.parse(startKey) as DataTablePredicate<ProfileTableField>[],
      ),
    [columns, defaultColumns, runsOnField, startKey],
  );
  const startRunsOn = runsOnValuesOf(ui.codec.defaultView);
  const startRunsOnKey = ui.codec.defaultView.predicates.some(({ field }) => field === "runsOn")
    ? startRunsOn.join("\0")
    : undefined;
  const [view, setView] = useState(ui.codec.defaultView);
  const [appliedRunsOn, setAppliedRunsOn] = useState(startRunsOnKey);
  if (appliedRunsOn !== startRunsOnKey) {
    setAppliedRunsOn(startRunsOnKey);
    setView(withRunsOn(view, startRunsOnKey === undefined ? undefined : startRunsOn));
  }

  const tableInput = {
    availability,
    definition: ui.definition,
    estimatesPending,
    profiles,
    runsOn,
  };
  const table = useMemo(
    () =>
      buildProfileTable({
        availability,
        definition: ui.definition,
        estimatesPending,
        profiles,
        runsOn,
        view,
      }),
    [availability, estimatesPending, profiles, runsOn, ui, view],
  );
  const shownKey = table.rows.map(({ row }) => row.profileId).join("\0");
  useEffect(() => {
    onShownChange?.(shownKey ? shownKey.split("\0") : []);
  }, [onShownChange, shownKey]);

  // Only the operator's view changes clear a hidden selection, so a refreshed read that hides it
  // leaves the host to explain why it can't run.
  const changeView = (next: ProfileTableView) => {
    setView(next);
    for (const { field } of view.predicates)
      if (!next.predicates.some((predicate) => predicate.field === field))
        chipSettings?.[field]?.clear?.();
    const nextRunsOn = runsOnValuesOf(next);
    if (onRunsOnChange && nextRunsOn.join("\0") !== runsOnValuesOf(view).join("\0"))
      onRunsOnChange(nextRunsOn);
    if (!onSelect || !selectedProfileId) return;

    const shown = buildProfileTable({ ...tableInput, view: next }).rows;
    if (!shown.some(({ row }) => row.profileId === selectedProfileId)) onSelect("");
  };
  const showAll = () => changeView(withRunsOn(view, undefined));
  const runsOnValues = runsOnValuesOf(view);

  return (
    <div className="flex min-w-0 flex-col gap-2">
      <DataTableToolbar
        chipSettings={chipSettings}
        facets={table.facets}
        onViewChange={changeView}
        ui={ui}
        view={view}
      />
      <DataTable
        empty={
          runsOn && table.noneRun ? (
            <NoneRun {...runsOn.noneRun(runsOnValues)} onShowAll={showAll} />
          ) : undefined
        }
        footnote={footnote?.(table.hiddenByFit)}
        onOpenRow={onOpen ? ({ row }, opener) => onOpen(row.profileId, opener) : undefined}
        onViewChange={changeView}
        select={
          onSelect
            ? {
                disabled: (row) => disabled || row.availability?.selectable === false,
                inputRef,
                onBlur,
                onSelect: ({ row }) => onSelect(row.profileId),
                selectedId: selectedProfileId,
              }
            : undefined
        }
        table={table}
        ui={ui}
        view={view}
      />
    </div>
  );
}

type ProfileTableUi = DataTableUiDefinition<
  ProfileRow,
  ProfileRow,
  ProfileColumnId,
  ProfileTableField,
  ProfileTableModel
>;

function profileTableUi(
  runsOn: ProfileTableRunsOn | undefined,
  columns: readonly ProfileMetricColumnId[] | undefined,
  defaultColumns: readonly ProfileMetricColumnId[] | undefined,
  start: readonly DataTablePredicate<ProfileTableField>[],
): ProfileTableUi {
  const definition = profileTableDefinition(runsOn, columns, defaultColumns);
  return {
    codec: profileTableCodec(definition, start),
    columns: {
      intelligence: {
        cell: ({ row }) => <IntelligenceValue score={row.referenceScore} />,
        showFrom: "32rem",
      },
      maximumContext: {
        cell: ({ row }) => <ContextValue context={row.maximumContext} />,
        showFrom: "32rem",
      },
      memory: {
        cell: ({ row }, table) => <MemoryValue memory={row.memory} pending={table.memoryPending} />,
      },
      profile: {
        cell: ({ availability, row }) => (
          <ProfileNameCell profile={row} status={availability?.status} />
        ),
      },
      totalContext: {
        cell: ({ row }) => <ContextValue context={row.totalContext} />,
        showFrom: "32rem",
      },
    },
    definition,
    fields: { ...profileFieldUi, runsOn: runsOnFieldUi },
    fixedColumns: !defaultColumns,
    label: "Model profiles",
    noun: { one: "profile", other: "profiles" },
  };
}

function runsOnValuesOf(view: ProfileTableView): readonly string[] {
  return view.predicates.find(({ field }) => field === "runsOn")?.values ?? [];
}

/** The table body when no profile runs in the Runs on scope: why, and how to see every profile. */
function NoneRun({ onShowAll, reasons, title }: ProfileNoneRun & { onShowAll: () => void }) {
  return (
    <div className="grid justify-items-center gap-1">
      <p className="font-semibold text-foreground">{title}</p>
      {reasons.map((reason) => (
        <p key={reason}>{reason}</p>
      ))}
      <Button className="mt-1" onClick={onShowAll} size="xs" type="button" variant="link">
        Show all profiles
      </Button>
    </div>
  );
}
