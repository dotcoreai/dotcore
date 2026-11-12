import { Combobox as ComboboxPrimitive } from "@base-ui/react/combobox";
import { CheckIcon, ChevronDownIcon, XIcon } from "lucide-react";
import * as React from "react";

import { useInspectorFloatingLayerRegistration } from "@/components/ui/inspector-shell";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { StatusDot } from "@/components/ui/status-badge";
import type { StatusProjection } from "@/lib/status-projection";
import { cn } from "@/lib/utils";

export type ComboboxOption = {
  id: string;
  label: string;
  disabled?: boolean;
  /**
   * Short text shown beside the label and in the accessible name instead of the ID, for options
   * whose ID is a composite key that isn't meant to be read. Filtering matches it in place of the ID.
   */
  detail?: string;
  /**
   * Stable resource key for a decorative identity marker before the label in the option and its
   * chip. It is excluded from filtering and accessible labels.
   */
  markerColorKey?: string;
  /** The option's current status, shown beside the option and as a dot on its chip. */
  status?: StatusProjection;
};

type ComboboxSharedProps = {
  label: string;
  options: readonly ComboboxOption[];
  ariaInvalid?: boolean;
  className?: string;
  description?: string;
  disabled?: boolean;
  errorDescriptionId?: string;
  /** Keeps the label for assistive technology only, where the surrounding layout names the field. */
  hideLabel?: boolean;
  inputRef?: React.Ref<HTMLInputElement>;
  loading?: boolean;
  loadingMessageVisible?: boolean;
  onBlur?: React.FocusEventHandler<HTMLInputElement>;
  placeholder?: string;
};

type SingleComboboxProps = ComboboxSharedProps & {
  multiple?: false;
  /** Whether a clear button empties the value. On by default; off where a pick is always required. */
  clearable?: boolean;
  /**
   * Whether typing filters the options. A short list reads better as a plain pick list, so callers
   * turn it off there; the field keeps its role and name and still opens on click and keyboard.
   */
  searchable?: boolean;
  value: string | null;
  onValueChange: (value: string | null) => void;
};

type MultipleComboboxProps = ComboboxSharedProps & {
  multiple: true;
  /**
   * Whether picking an option closes the list. Off by default so a list can be built in one open
   * session; on where each pick completes the interaction. Removing a chip never opens the list.
   */
  closeOnSelect?: boolean;
  value: string[];
  onValueChange: (value: string[]) => void;
};

export type ComboboxProps = SingleComboboxProps | MultipleComboboxProps;

function Combobox(props: ComboboxProps) {
  return props.multiple ? <MultipleCombobox {...props} /> : <SingleCombobox {...props} />;
}

function SingleCombobox({
  ariaInvalid,
  className,
  clearable = true,
  description,
  disabled,
  errorDescriptionId,
  hideLabel = false,
  inputRef,
  label,
  loading = false,
  loadingMessageVisible = loading,
  onBlur,
  onValueChange,
  options,
  placeholder,
  searchable = true,
  value,
}: SingleComboboxProps) {
  const descriptionId = React.useId();
  const inputId = React.useId();
  const labelId = React.useId();
  const describedBy = [description ? descriptionId : undefined, errorDescriptionId]
    .filter(Boolean)
    .join(" ");
  const registerFloatingLayer = useInspectorFloatingLayerRegistration();

  return (
    <ComboboxPrimitive.Root<string>
      disabled={disabled}
      filter={createOptionFilter(options)}
      itemToStringLabel={createOptionLabel(options)}
      items={options.map((option) => option.id)}
      onOpenChange={registerFloatingLayer}
      onValueChange={onValueChange}
      value={value}
    >
      <div className={cn("grid grid-cols-[minmax(0,1fr)] gap-1.5", className)}>
        <ComboboxLabel hidden={hideLabel} id={labelId} inputId={inputId} label={label} />
        <ComboboxPrimitive.InputGroup className="flex h-control-md items-center rounded-lg border border-input bg-card pr-1 transition-colors focus-within:border-ring focus-within:ring-3 focus-within:ring-ring/50 dark:bg-input/30">
          <ComboboxPrimitive.Input
            id={inputId}
            aria-describedby={describedBy || undefined}
            aria-labelledby={labelId}
            aria-invalid={ariaInvalid || undefined}
            className="min-w-0 flex-1 bg-transparent px-2.5 py-1 text-sm outline-none pointer-coarse:text-base placeholder:text-muted-foreground disabled:cursor-not-allowed"
            onBlur={onBlur}
            placeholder={placeholder ?? (searchable ? "Search options" : "Select an option")}
            readOnly={!searchable}
            ref={inputRef}
          />
          {clearable ? <ClearButton label={label} /> : null}
          <ComboboxPrimitive.Trigger
            aria-label={`Show ${label} options`}
            className="flex size-control-sm shrink-0 items-center justify-center rounded-md text-muted-foreground hover:bg-muted disabled:pointer-events-none disabled:opacity-50"
          >
            <ChevronDownIcon />
          </ComboboxPrimitive.Trigger>
        </ComboboxPrimitive.InputGroup>
        {description ? (
          <p id={descriptionId} className="text-sm text-muted-foreground">
            {description}
          </p>
        ) : null}
      </div>
      <ComboboxPopup
        loading={loading}
        loadingMessageVisible={loadingMessageVisible}
        options={options}
        selectedValues={value ? [value] : []}
      />
    </ComboboxPrimitive.Root>
  );
}

function MultipleCombobox({
  ariaInvalid,
  className,
  closeOnSelect = false,
  description,
  disabled,
  errorDescriptionId,
  hideLabel = false,
  inputRef,
  label,
  loading = false,
  loadingMessageVisible = loading,
  onBlur,
  onValueChange,
  options,
  placeholder = "Search options",
  value,
}: MultipleComboboxProps) {
  const descriptionId = React.useId();
  const inputId = React.useId();
  const labelId = React.useId();
  const describedBy = [description ? descriptionId : undefined, errorDescriptionId]
    .filter(Boolean)
    .join(" ");
  const optionLabel = createOptionLabel(options);
  const findOption = (id: string) => options.find((option) => option.id === id);
  const registerFloatingLayer = useInspectorFloatingLayerRegistration();
  const [open, setOpen] = React.useState(false);

  return (
    <ComboboxPrimitive.Root<string, true>
      disabled={disabled}
      filter={createOptionFilter(options)}
      itemToStringLabel={optionLabel}
      items={options.map((option) => option.id)}
      multiple
      onOpenChange={(nextOpen) => {
        setOpen(nextOpen);
        registerFloatingLayer(nextOpen);
      }}
      open={open}
      onValueChange={(next, details) => {
        // Base UI clears every chip on Escape while the popup is closed. A list the user is
        // building should not vanish on a dismissal key, so Escape keeps the selection and reaches
        // the surrounding surface instead.
        if (details.reason === "escape-key") {
          details.allowPropagation();
          return;
        }
        if (closeOnSelect && details.reason === "item-press") {
          setOpen(false);
          registerFloatingLayer(false);
        }
        onValueChange(next);
      }}
      value={value}
    >
      <div className={cn("grid grid-cols-[minmax(0,1fr)] gap-1.5", className)}>
        <ComboboxLabel hidden={hideLabel} id={labelId} inputId={inputId} label={label} />
        <ComboboxPrimitive.InputGroup className="flex min-h-control-md items-start gap-1 rounded-lg border border-input bg-card p-1 transition-colors focus-within:border-ring focus-within:ring-3 focus-within:ring-ring/50 dark:bg-input/30">
          {/* Chips and the input wrap as one flow beside the trigger, which stays on the first line.
              The vertical padding centers the first line on the taller trigger. */}
          <div className="flex min-w-0 flex-1 flex-wrap items-center gap-1 py-0.5">
            <ComboboxPrimitive.Chips className="contents">
              {value.map((selectedValue) => (
                <ComboboxPrimitive.Chip
                  key={selectedValue}
                  className="flex items-center gap-1 rounded-md bg-muted py-0.5 pr-1 pl-2 text-sm"
                >
                  <OptionMarker
                    className="mr-0"
                    colorKey={findOption(selectedValue)?.markerColorKey}
                  />
                  {optionLabel(selectedValue)}
                  <OptionStatus status={findOption(selectedValue)?.status} />
                  <ComboboxPrimitive.ChipRemove
                    aria-label={`Remove ${optionLabel(selectedValue)}`}
                    className="relative rounded-sm text-muted-foreground touch-target hover:text-foreground"
                  >
                    <XIcon className="size-3" />
                  </ComboboxPrimitive.ChipRemove>
                </ComboboxPrimitive.Chip>
              ))}
            </ComboboxPrimitive.Chips>
            <ComboboxPrimitive.Input
              id={inputId}
              aria-describedby={describedBy || undefined}
              aria-labelledby={labelId}
              aria-invalid={ariaInvalid || undefined}
              // Positioned so it stacks above the last chip's remove tap area.
              className="relative min-w-24 flex-1 bg-transparent px-1.5 py-0.5 text-sm outline-none pointer-coarse:text-base placeholder:text-muted-foreground disabled:cursor-not-allowed"
              onBlur={onBlur}
              onKeyDown={preventImplicitSubmit}
              // Chips already show the selection, so the placeholder would only crowd the input.
              placeholder={value.length ? undefined : placeholder}
              ref={inputRef}
            />
          </div>
          <ComboboxPrimitive.Trigger
            aria-label={`Show ${label} options`}
            className="flex size-control-sm shrink-0 items-center justify-center rounded-md text-muted-foreground hover:bg-muted disabled:pointer-events-none disabled:opacity-50"
          >
            <ChevronDownIcon />
          </ComboboxPrimitive.Trigger>
        </ComboboxPrimitive.InputGroup>
        {description ? (
          <p id={descriptionId} className="text-sm text-muted-foreground">
            {description}
          </p>
        ) : null}
      </div>
      <ComboboxPopup
        loading={loading}
        loadingMessageVisible={loadingMessageVisible}
        options={options}
        selectedValues={value}
      />
    </ComboboxPrimitive.Root>
  );
}

/**
 * The field label. The input names itself through `aria-labelledby` rather than `htmlFor` alone:
 * while the popup is open Base UI hides everything outside it and the input from assistive
 * technology, label included, and only a label reference survives that hiding.
 */
function ComboboxLabel({
  hidden,
  id,
  inputId,
  label,
}: {
  hidden: boolean;
  id: string;
  inputId: string;
  label: string;
}) {
  return (
    <label
      className={cn("text-sm font-medium text-foreground", hidden && "sr-only")}
      htmlFor={inputId}
      id={id}
    >
      {label}
    </label>
  );
}

/**
 * Keeps Enter in a multiple-selection input from submitting its form. Enter toggles options in a
 * list the user is still building, so it never means "done"; Base UI still handles the key.
 */
function preventImplicitSubmit(event: React.KeyboardEvent<HTMLInputElement>) {
  if (event.key === "Enter") event.preventDefault();
}

function ClearButton({ label }: { label: string }) {
  return (
    <ComboboxPrimitive.Clear
      aria-label={`Clear ${label}`}
      className="flex size-control-sm shrink-0 items-center justify-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground data-invisible:hidden"
    >
      <XIcon className="size-3.5" />
    </ComboboxPrimitive.Clear>
  );
}

function ComboboxPopup({
  loading,
  loadingMessageVisible,
  options,
  selectedValues,
}: Pick<ComboboxSharedProps, "loading" | "loadingMessageVisible" | "options"> & {
  selectedValues: readonly string[];
}) {
  return (
    <ComboboxPrimitive.Portal>
      <ComboboxPrimitive.Positioner className="z-popup" sideOffset={4}>
        <ComboboxPrimitive.Popup className="max-h-(--available-height) w-(--anchor-width) overflow-auto rounded-lg bg-popover p-1 text-popover-foreground shadow-md ring-1 ring-foreground/10">
          {loading ? (
            loadingMessageVisible ? (
              <p className="px-2 py-1.5 text-sm text-muted-foreground" role="status">
                Loading options…
              </p>
            ) : null
          ) : (
            <>
              <ComboboxPrimitive.List>
                {(id: string) => {
                  const option = options.find((candidate) => candidate.id === id);
                  if (!option) return null;

                  const selected = selectedValues.includes(option.id);

                  return (
                    <ComboboxPrimitive.Item
                      key={option.id}
                      aria-label={[option.label, option.detail ?? option.id, option.status?.label]
                        .filter(Boolean)
                        .join(" ")}
                      aria-selected={selected}
                      disabled={option.disabled}
                      value={option.id}
                      className="flex min-h-menu-row cursor-default items-center gap-2 rounded-md px-2 py-1.5 text-sm outline-none select-none data-highlighted:bg-accent data-highlighted:text-accent-foreground data-disabled:pointer-events-none data-disabled:opacity-50"
                    >
                      {selected ? (
                        <CheckIcon aria-hidden="true" className="size-4" />
                      ) : (
                        <span className="size-4" />
                      )}
                      <span className="min-w-0 flex-1 wrap-anywhere sm:truncate">
                        <OptionMarker colorKey={option.markerColorKey} />
                        {option.label}
                      </span>
                      {option.status ? (
                        <span className="flex shrink-0 items-center gap-1.5 text-xs text-muted-foreground">
                          <StatusDot status={option.status} />
                          {option.status.label}
                        </span>
                      ) : null}
                      <span className="max-w-[40%] truncate text-xs text-muted-foreground">
                        {option.detail ?? option.id}
                      </span>
                    </ComboboxPrimitive.Item>
                  );
                }}
              </ComboboxPrimitive.List>
              <ComboboxPrimitive.Empty className="px-2 py-1.5 text-sm text-muted-foreground">
                {options.length === 0 ? "No options available." : "No matching options."}
              </ComboboxPrimitive.Empty>
            </>
          )}
        </ComboboxPrimitive.Popup>
      </ComboboxPrimitive.Positioner>
    </ComboboxPrimitive.Portal>
  );
}

/** A selected chip's status: a dot, with its label kept for assistive technology. */
function OptionStatus({ status }: { status?: StatusProjection }) {
  if (!status) return null;

  return (
    <>
      <StatusDot status={status} />
      <span className="sr-only">{status.label}</span>
    </>
  );
}

function OptionMarker({ className, colorKey }: { className?: string; colorKey?: string }) {
  return colorKey ? <ResourceMarker className={className} colorKey={colorKey} /> : null;
}

function createOptionFilter(options: readonly ComboboxOption[]) {
  return (id: string, query: string) => {
    const option = options.find((candidate) => candidate.id === id);
    const normalizedQuery = query.toLocaleLowerCase();
    return (
      option?.label.toLocaleLowerCase().includes(normalizedQuery) ||
      (option?.detail ?? option?.id)?.toLocaleLowerCase().includes(normalizedQuery) ||
      false
    );
  };
}

function createOptionLabel(options: readonly ComboboxOption[]) {
  return (id: string) => options.find((option) => option.id === id)?.label ?? id;
}

export { Combobox };
