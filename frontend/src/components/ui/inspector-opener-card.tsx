import type { ReactNode } from "react";

import { Card } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import { usePanelNavigationPending } from "@/panels/panel-navigation-pending";

// Declared once so every inspector-opener card renders identical geometry.
const GEOMETRY =
  "gap-2.5 min-w-0 cursor-pointer rounded-lg px-4 py-3.75 focus-visible:ring-3 focus-visible:ring-ring/50";

export interface InspectorOpenerCardProps {
  className?: string;
  children: ReactNode;
  label: string;
  onSelect: (opener: HTMLElement) => void;
  panelId: string;
  selected: boolean;
}

/**
 * A `Card` that opens an inspector panel on click or keyboard activation, and reflects its
 * selection through `data-selected` and `aria-expanded`. Shared by every page that discloses
 * detail in an `InspectorShell` panel, so they carry identical disclosure geometry and semantics.
 */
export function InspectorOpenerCard({
  className,
  children,
  label,
  onSelect,
  panelId,
  selected,
}: InspectorOpenerCardProps) {
  const navigationPending = usePanelNavigationPending();

  return (
    <Card
      aria-controls={panelId}
      aria-expanded={selected}
      aria-label={label}
      className={cn(GEOMETRY, className)}
      data-panel-navigation=""
      onClick={(event) => {
        if (!navigationPending) onSelect(event.currentTarget);
      }}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          if (!navigationPending) onSelect(event.currentTarget);
        }
      }}
      role="button"
      selected={selected}
      size="sm"
      tabIndex={0}
    >
      {children}
    </Card>
  );
}
