import { Columns3Icon, ListFilterIcon, SearchIcon, XIcon } from "lucide-react";
import { type ReactNode, useState } from "react";

import type {
  DataTableResult,
  DataTableRow,
  FacetValue,
} from "@/components/data-table/data-table-model";
import type { DataTablePredicate, DataTableView } from "@/components/data-table/data-table-view";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";

import type { DataTableFieldUi, DataTableNoun, DataTableUiDefinition } from "./data-table";

export interface DataTableToolbarProps<
  Row,
  Unit,
  C extends string,
  F extends string,
  T extends DataTableResult<DataTableRow<Row, unknown>, F>,
> {
  /** Table-specific settings some filter chips carry beside their values. */
  chipSettings?: Partial<Record<F, DataTableChipSetting>>;
  facets: T["facets"];
  /**
   * Restores the default view and every chip setting in one update, for a host that keeps them
   * together, such as in the URL. Without it, Reset restores each setting and then the view.
   */
  onReset?: () => void;
  onViewChange: (view: DataTableView<C, F>) => void;
  ui: DataTableUiDefinition<Row, Unit, C, F, T>;
  view: DataTableView<C, F>;
}

/**
 * A setting a filter chip carries beside its values, such as how the field judges them: a section
 * of the chip's menu after the values, and a summary the chip shows after theirs when set.
 */
export interface DataTableChipSetting {
  /**
   * Drops the setting, for a host to call when its chip leaves the view, since the setting belongs
   * to the chip. A host keeping view and settings together drops them in one update instead.
   */
  clear?: () => void;
  menu: ReactNode;
  /** Restores the setting's default; present only while the setting differs from it. */
  reset?: () => void;
  summary?: string;
}

/**
 * A data table toolbar: row search, one chip per shown filter predicate with its value menu and any
 * setting it carries, the Filter menu that adds predicates, Reset while the view or a shown chip's
 * setting differs from its default, and the View menu that toggles columns unless the columns are fixed.
 */
export function DataTableToolbar<
  Row,
  Unit,
  C extends string,
  F extends string,
  T extends DataTableResult<DataTableRow<Row, unknown>, F>,
>({
  chipSettings,
  facets,
  onReset,
  onViewChange,
  ui,
  view,
}: DataTableToolbarProps<Row, Unit, C, F, T>) {
  const { codec, definition } = ui;
  // The chip whose value menu is open, so a predicate added from the Filter menu opens its values.
  const [openField, setOpenField] = useState<F>();
  const hiddenFields = definition.fields.filter(
    ({ id }) => !view.predicates.some((predicate) => predicate.field === id),
  );

  const setPredicates = (predicates: DataTablePredicate<F>[]) =>
    onViewChange({ ...view, predicates });
  const setValues = (field: F, values: string[]) =>
    setPredicates(
      view.predicates.map((predicate) =>
        predicate.field === field ? { field, values } : predicate,
      ),
    );
  const removePredicate = (field: F) =>
    setPredicates(view.predicates.filter((predicate) => predicate.field !== field));

  // A setting counts only while its chip shows, so Reset never stands for a setting out of view;
  // Reset restores every setting, since the default view may bring back a removed chip.
  const settingResets = view.predicates.flatMap(({ field }) => chipSettings?.[field]?.reset ?? []);
  const reset =
    onReset ??
    (() => {
      for (const setting of Object.values<DataTableChipSetting | undefined>(chipSettings ?? {}))
        setting?.reset?.();
      onViewChange(codec.defaultView);
    });

  return (
    // View controls are panel navigation targets, so changing the view keeps an open panel.
    <div className="flex flex-wrap items-center gap-x-3 gap-y-2" data-panel-navigation="">
      <div className="relative w-60 max-sm:w-full">
        <SearchIcon
          aria-hidden="true"
          className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground"
        />
        <Input
          aria-label={`Filter ${ui.noun.other}`}
          className="pl-8 text-[13px] md:text-[13px]"
          onChange={(event) => onViewChange({ ...view, search: event.target.value })}
          placeholder={`Filter ${ui.noun.other}…`}
          type="search"
          value={view.search}
        />
      </div>
      {view.predicates.map((predicate) => {
        // A predicate on a field the table doesn't offer, such as one whose data isn't loaded yet,
        // stays in the view without a chip.
        const field = definition.fields.find(({ id }) => id === predicate.field);
        return field ? (
          <PredicateChip
            facets={facets[predicate.field]}
            field={ui.fields[predicate.field]}
            key={predicate.field}
            label={field.label}
            noun={ui.noun}
            onOpenChange={(open) => setOpenField(open ? predicate.field : undefined)}
            onRemove={() => removePredicate(predicate.field)}
            onValuesChange={(values) => setValues(predicate.field, values)}
            open={openField === predicate.field}
            setting={chipSettings?.[predicate.field]}
            values={predicate.values}
          />
        ) : null;
      })}
      {hiddenFields.length ? (
        <DropdownMenu>
          <DropdownMenuTrigger render={<Button size="sm" type="button" variant="ghost" />}>
            <ListFilterIcon aria-hidden="true" />
            Filter
          </DropdownMenuTrigger>
          <DropdownMenuContent className="w-auto min-w-44" data-panel-navigation="">
            <DropdownMenuGroup>
              <DropdownMenuLabel>Add filter</DropdownMenuLabel>
              {hiddenFields.map(({ id: field, label }) => {
                const Icon = ui.fields[field].icon;
                return (
                  <DropdownMenuItem
                    key={field}
                    onClick={() => {
                      setPredicates([...view.predicates, { field, values: [] }]);
                      setOpenField(field);
                    }}
                  >
                    <Icon aria-hidden="true" />
                    {label}
                  </DropdownMenuItem>
                );
              })}
            </DropdownMenuGroup>
          </DropdownMenuContent>
        </DropdownMenu>
      ) : null}
      {codec.isDefault(view) && !settingResets.length ? null : (
        <Button onClick={reset} size="sm" type="button" variant="ghost">
          Reset
          <XIcon aria-hidden="true" data-icon="inline-end" />
        </Button>
      )}
      {ui.fixedColumns ? null : (
        <>
          <span className="flex-1" />
          <DropdownMenu>
            <DropdownMenuTrigger render={<Button size="sm" type="button" variant="outline" />}>
              <Columns3Icon aria-hidden="true" />
              View
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="w-auto min-w-48" data-panel-navigation="">
              <DropdownMenuGroup>
                <DropdownMenuLabel>Details</DropdownMenuLabel>
                {definition.columns
                  .filter(({ pinned }) => !pinned)
                  .map(({ id, label }) => (
                    <DropdownMenuCheckboxItem
                      checked={view.columns.includes(id)}
                      key={id}
                      onCheckedChange={(checked) =>
                        onViewChange(
                          codec.setColumns(
                            view,
                            checked
                              ? [...view.columns, id]
                              : view.columns.filter((column) => column !== id),
                          ),
                        )
                      }
                    >
                      {label}
                    </DropdownMenuCheckboxItem>
                  ))}
              </DropdownMenuGroup>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                onClick={() => onViewChange(codec.setColumns(view, codec.defaultColumns))}
              >
                Reset columns
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </>
      )}
    </div>
  );
}

function PredicateChip({
  facets,
  field,
  label,
  noun,
  onOpenChange,
  onRemove,
  onValuesChange,
  open,
  setting,
  values,
}: {
  facets: readonly FacetValue[];
  field: DataTableFieldUi;
  label: string;
  noun: DataTableNoun;
  onOpenChange: (open: boolean) => void;
  onRemove: () => void;
  onValuesChange: (values: string[]) => void;
  open: boolean;
  setting?: DataTableChipSetting;
  values: readonly string[];
}) {
  const Icon = field.icon;
  const valueLabel = (value: string) =>
    facets.find((facet) => facet.value === value)?.label ?? value;
  const summary =
    values.length > 2 ? `${values.length} selected` : values.map(valueLabel).join(", ");
  const settingSummary = setting?.summary;

  return (
    // The chip clips its buttons to its rounded border, so their focus rings sit inside it. The
    // trigger stacks above the remove button so the remove tap area can't cover its edge. On a
    // narrow toolbar the setting's summary truncates so the remove button stays in view.
    <div className="inline-flex h-control-sm max-w-full min-w-0 items-stretch overflow-hidden rounded-lg border border-border text-[12.8px]">
      <DropdownMenu onOpenChange={onOpenChange} open={open}>
        <DropdownMenuTrigger
          aria-label={`${label} filter: ${summary || "any"}${settingSummary ? `, ${settingSummary}` : ""}`}
          render={
            <Button
              className="z-1 h-full min-w-0 shrink rounded-none border-0 px-2 font-normal focus-visible:ring-inset"
              size="sm"
              type="button"
              variant="ghost"
            />
          }
        >
          <Icon aria-hidden="true" className="text-muted-foreground" />
          <span className="text-muted-foreground">{label}</span>
          <span className={cn(summary ? "font-semibold" : "text-muted-foreground italic")}>
            {summary || "any"}
          </span>
          {settingSummary ? (
            <>
              <span aria-hidden="true" className="text-muted-foreground">
                ·
              </span>
              <span className="min-w-0 truncate font-semibold">{settingSummary}</span>
            </>
          ) : null}
        </DropdownMenuTrigger>
        {/* A setting holds inputs, so the menu takes a fixed width that fits them. */}
        <DropdownMenuContent
          className={cn("w-auto max-w-72 min-w-52", setting && "w-80 max-w-none")}
          data-panel-navigation=""
        >
          <DropdownMenuGroup>
            <DropdownMenuLabel>{label}</DropdownMenuLabel>
            {/* Touch has no shift key, so the hint would only mislead there. */}
            {field.replaceOnSelect ? (
              <p className="px-1.5 pb-1 text-xs text-muted-foreground pointer-coarse:hidden">
                Shift-click to select more than one
              </p>
            ) : null}
            {facets.map((facet) => (
              <DropdownMenuCheckboxItem
                checked={values.includes(facet.value)}
                key={facet.value}
                onCheckedChange={(checked, { event }) =>
                  onValuesChange(
                    toggledValues(values, facet.value, checked, {
                      replace:
                        field.replaceOnSelect === true && !("shiftKey" in event && event.shiftKey),
                    }),
                  )
                }
              >
                {field.renderValue ? (
                  <span className="flex min-w-0 flex-1">{field.renderValue(facet)}</span>
                ) : (
                  <span className="min-w-0 flex-1 truncate">{facet.label}</span>
                )}
                <span
                  aria-label={`${facet.count} ${facet.count === 1 ? noun.one : noun.other}`}
                  className="ml-3 font-mono text-[11.5px] text-muted-foreground"
                >
                  {facet.count}
                </span>
              </DropdownMenuCheckboxItem>
            ))}
          </DropdownMenuGroup>
          {setting ? (
            <>
              <DropdownMenuSeparator />
              {setting.menu}
            </>
          ) : null}
          <DropdownMenuSeparator />
          <DropdownMenuItem onClick={onRemove}>
            <XIcon aria-hidden="true" />
            Remove filter
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <Button
        aria-label={`Remove ${label} filter`}
        className="h-full rounded-none border-0 border-l border-border px-1.5 text-muted-foreground focus-visible:ring-inset"
        onClick={onRemove}
        size="sm"
        type="button"
        variant="ghost"
      >
        <XIcon aria-hidden="true" />
      </Button>
    </div>
  );
}

// A replacing toggle selects only the value, and clears it when it was the only one selected.
function toggledValues(
  values: readonly string[],
  value: string,
  checked: boolean,
  { replace }: { replace: boolean },
): string[] {
  if (replace) return checked || values.length > 1 ? [value] : [];
  return checked ? [...values, value] : values.filter((other) => other !== value);
}
