import { ChevronRightIcon } from "lucide-react";
import type { ReactNode, Ref } from "react";

import { Button } from "@/components/ui/button";

interface InspectorRowListProps {
  ariaLabel: string;
  children: ReactNode;
}

interface InspectorRowBaseProps {
  /** A muted line under the label, such as a profile's engine and quantization. */
  description?: ReactNode;
  label: ReactNode;
  trailing?: ReactNode;
}

/**
 * A row is either a control that opens a sub-screen, which needs its own accessible name, or static
 * collection content, whose visible text is its name. An accessible label on a non-interactive row
 * would name an element with no role, so the two forms are mutually exclusive in the type.
 */
type InspectorRowProps = InspectorRowBaseProps &
  (
    | { accessibleLabel: string; onSelect: () => void; ref?: Ref<HTMLButtonElement> }
    | { accessibleLabel?: never; onSelect?: never; ref?: never }
  );

/**
 * A bordered list of inspector rows. A row either opens one sub-screen of the inspector that
 * renders the list, or presents one member of a collection the inspector does not navigate into.
 */
function InspectorRowList({ ariaLabel, children }: InspectorRowListProps) {
  return (
    <ul
      aria-label={ariaLabel}
      className="mt-3 overflow-hidden rounded-lg border border-border bg-card"
    >
      {children}
    </ul>
  );
}

/**
 * One row of an `InspectorRowList`. It shows a label, an optional description line under it, and an
 * optional trailing element such as a badge. A row with `onSelect` is a control and carries a chevron; a row without one is static
 * content, so it renders neither.
 */
function InspectorRow({
  accessibleLabel,
  description,
  label,
  onSelect,
  ref,
  trailing,
}: InspectorRowProps) {
  const content = description ? (
    <span className="min-w-0">
      <span className="block truncate">{label}</span>
      <span className="block truncate text-xs font-normal text-muted-foreground">
        {description}
      </span>
    </span>
  ) : (
    <span className="min-w-0 truncate">{label}</span>
  );

  if (!onSelect) {
    return (
      <li className="border-b border-border last:border-b-0">
        <div className="flex min-h-11 w-full items-center justify-between gap-3 px-3 py-2.5 text-left text-sm font-medium">
          {content}
          {trailing ? <span className="min-w-0 shrink-0 text-right">{trailing}</span> : null}
        </div>
      </li>
    );
  }

  return (
    <li className="border-b border-border last:border-b-0">
      <Button
        aria-label={accessibleLabel}
        className="h-auto min-h-11 w-full justify-between gap-3 rounded-none px-3 py-2.5 text-left font-medium hover:bg-muted/70 focus-visible:z-10"
        onClick={onSelect}
        ref={ref}
        type="button"
        variant="ghost"
      >
        {content}
        <span className="flex min-w-0 shrink-0 items-center gap-2">
          {trailing}
          <ChevronRightIcon aria-hidden="true" className="size-4 text-muted-foreground" />
        </span>
      </Button>
    </li>
  );
}

export { InspectorRow, InspectorRowList };
export type { InspectorRowListProps, InspectorRowProps };
