import type { ReactNode } from "react";

interface InspectorSectionHeaderBaseProps {
  /** Id of the rendered heading, so the owning `section` can name itself through `aria-labelledby`. */
  headingId: string;
  title: string;
}

/**
 * A section carries at most one right-hand slot, so a meta value and an action are mutually
 * exclusive rather than merely discouraged.
 */
type InspectorSectionHeaderProps = InspectorSectionHeaderBaseProps &
  ({ action?: never; meta?: ReactNode } | { action?: ReactNode; meta?: never });

/**
 * Header line of an inspector section: its heading and at most one right-hand slot, holding either
 * a meta value such as a count or a single action. The section wrapper owns its own spacing and
 * separator, so this owns the header line and the heading type scale only.
 */
function InspectorSectionHeader({ action, headingId, meta, title }: InspectorSectionHeaderProps) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <h2 className="text-sm font-semibold" id={headingId}>
        {title}
      </h2>
      {meta ? <span className="text-xs text-muted-foreground">{meta}</span> : action}
    </div>
  );
}

export { InspectorSectionHeader };
export type { InspectorSectionHeaderProps };
