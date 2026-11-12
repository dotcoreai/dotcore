import { CircleQuestionMark } from "lucide-react";
import { type ReactNode, useRef, useState } from "react";

import { FieldError } from "@/components/ui/field-error";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { timing } from "@/lib/timing";
import { cn } from "@/lib/utils";

/** Props a field hands its control, so the label, help and error all point at it. */
interface FieldControlProps {
  "aria-describedby": string | undefined;
  "aria-invalid": "true" | undefined;
  id: string;
}

interface FieldProps {
  /** Buttons that change the value for the operator, shown under the control. */
  actions?: ReactNode;
  /** Renders the control from the props it must carry. */
  children: (control: FieldControlProps) => ReactNode;
  className?: string;
  /** Element ID of the control; the description and error IDs derive from it. */
  controlId: string;
  /** IDs of other elements describing the control, read after the help and before the error. */
  describedBy?: string;
  error?: string;
  /** Facts about the value, such as its range and unit, shown only in the popover. */
  facts?: ReactNode;
  /** What the field does and the facts that bound it, shown in the help popover. */
  help?: ReactNode;
  label: string;
  /** `inline` puts a compact control, such as a switch, on the label's row. */
  layout?: "inline" | "stacked";
}

/**
 * One form field: its label with an optional help popover, the control, a validation error and
 * buttons acting on the value. Help stays out of the layout; the control keeps it as its
 * accessible description whether or not the popover is open.
 */
function Field({
  actions,
  children,
  className,
  controlId,
  describedBy,
  error,
  facts,
  help,
  label,
  layout = "stacked",
}: FieldProps) {
  const descriptionId = `${controlId}-description`;
  const errorId = `${controlId}-error`;
  const control = children({
    "aria-describedby":
      [help ? descriptionId : "", describedBy ?? "", error ? errorId : ""]
        .filter(Boolean)
        .join(" ") || undefined,
    "aria-invalid": error ? "true" : undefined,
    id: controlId,
  });
  const labelRow = (
    <div className="flex min-w-0 items-center gap-1.5">
      <label htmlFor={controlId}>{label}</label>
      {help || facts ? (
        <FieldHelp descriptionId={descriptionId} facts={facts} label={label}>
          {help}
        </FieldHelp>
      ) : null}
    </div>
  );

  return (
    <div className={cn("grid gap-1", className)} data-slot="field">
      {layout === "inline" ? (
        <div className="flex items-center justify-between gap-3">
          {labelRow}
          {control}
        </div>
      ) : (
        <>
          {labelRow}
          {control}
        </>
      )}
      {error ? <FieldError id={errorId}>{error}</FieldError> : null}
      {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
    </div>
  );
}

/**
 * The `?` button beside a field label and its help popover. It opens on hover, keyboard focus,
 * click and tap. Because the popover unmounts while closed, a hidden copy of the help carries
 * `descriptionId` for the control's `aria-describedby`.
 */
function FieldHelp({
  children,
  descriptionId,
  facts,
  label,
}: {
  children?: ReactNode;
  descriptionId: string;
  /** Facts shown after the help in the popover but left out of the accessible description. */
  facts?: ReactNode;
  label: string;
}) {
  const [open, setOpen] = useState(false);
  const openedByFocus = useRef(false);
  const pointerDown = useRef(false);

  const change = (nextOpen: boolean) => {
    openedByFocus.current = false;
    setOpen(nextOpen);
  };

  return (
    <>
      <Popover
        onOpenChange={(nextOpen, details) => {
          // A press on a trigger that keyboard focus opened keeps the help open instead of
          // toggling it shut.
          if (!nextOpen && openedByFocus.current && details.reason === "trigger-press") {
            openedByFocus.current = false;
            return;
          }
          change(nextOpen);
        }}
        open={open}
      >
        <PopoverTrigger
          aria-label={`About ${label}`}
          className="relative inline-flex touch-target items-center justify-center rounded-full text-muted-foreground outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/50 data-popup-open:text-foreground"
          delay={timing().tooltipOpenMs}
          onBlur={() => {
            pointerDown.current = false;
            if (openedByFocus.current) change(false);
          }}
          onFocus={() => {
            // A pointer press focuses the button too; its click opens the help instead.
            if (pointerDown.current || open) return;
            openedByFocus.current = true;
            setOpen(true);
          }}
          onPointerDown={() => {
            pointerDown.current = true;
          }}
          onPointerUp={() => {
            pointerDown.current = false;
          }}
          onPointerCancel={() => {
            pointerDown.current = false;
          }}
          openOnHover
          type="button"
        >
          <CircleQuestionMark aria-hidden="true" className="size-3.5" />
        </PopoverTrigger>
        <PopoverContent
          align="start"
          aria-label={`About ${label}`}
          className="w-auto max-w-xs gap-1.5 text-xs"
          collisionPadding={16}
          finalFocus={false}
          initialFocus={false}
          side="top"
        >
          {children}
          {facts ? <div className="text-muted-foreground">{facts}</div> : null}
        </PopoverContent>
      </Popover>
      {children ? (
        <div hidden id={descriptionId}>
          {children}
        </div>
      ) : null}
    </>
  );
}

export { Field, FieldHelp };
export type { FieldControlProps, FieldProps };
