import type { ReactNode } from "react";

import { CardDescription, CardTitle } from "@/components/ui/card";
import { InspectorOpenerCard } from "@/components/ui/inspector-opener-card";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { StatusBadge } from "@/components/ui/status-badge";
import type { StatusProjection } from "@/lib/status-projection";

export interface ResourceCardProps {
  accessibleName: string;
  colorKey?: string;
  description?: ReactNode;
  name: string;
  onSelect: (opener: HTMLElement) => void;
  panelId: string;
  selected: boolean;
  status?: StatusProjection;
}

/**
 * The card every console collection renders for one resource. It fills three slots: the identity
 * heading and one status badge sharing the heading row, and one description line beneath. A card
 * omits a slot it cannot fill and never reorders them.
 *
 * The badge holds the row's right edge and carries a single status word, so every card corner reads
 * the same and a card's line count cannot change as a resource transitions, fails and recovers.
 * Detail qualifying the status — load progress, a failure message — reaches the reader through the
 * badge's tooltip and the resource's inspector, never as card text.
 */
export function ResourceCard({
  accessibleName,
  colorKey,
  description,
  name,
  onSelect,
  panelId,
  selected,
  status,
}: ResourceCardProps) {
  return (
    <article aria-label={accessibleName} className="h-full min-w-0">
      <InspectorOpenerCard
        className="h-full min-h-23"
        label={`View details for ${accessibleName}`}
        onSelect={onSelect}
        panelId={panelId}
        selected={selected}
      >
        {/* Fixed height: the badge's line box is taller than the heading's, so without it a badged
            card would seat its heading a fraction lower than a badgeless one in the same row. */}
        <div className="flex min-h-6 items-center justify-between gap-2.5">
          <CardTitle className="min-w-0">
            {/* Truncates rather than wrapping, so the badge keeps the row's right edge. */}
            <h3
              className="m-0 truncate text-[15px] leading-[22.5px] font-semibold text-foreground"
              title={name}
            >
              {colorKey ? <ResourceMarker colorKey={colorKey} /> : null}
              {name}
            </h3>
          </CardTitle>
          {status ? <StatusBadge status={status} /> : null}
        </div>
        {description ? (
          <CardDescription className="m-0 mt-auto text-[11.5px] leading-[17.25px] text-console-subtle">
            {description}
          </CardDescription>
        ) : null}
      </InspectorOpenerCard>
    </article>
  );
}
