import type { ReactNode } from "react";

import { Detail, DetailList } from "@/components/ui/detail-list";
import { InspectorRow, InspectorRowList } from "@/components/ui/inspector-row";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { StatusBadge } from "@/components/ui/status-badge";
import { formatCount } from "@/lib/format";
import type { StatusProjection } from "@/lib/status-projection";

/**
 * An inspector section listing deployments related to the inspected resource, with their count in
 * the header. A list whose read failed shows as unreachable rather than partial. Without deployments,
 * the section shows its empty text, or nothing when it has none.
 */
export function DeploymentListSection({
  children,
  count,
  emptyText,
  headingId,
  listLabel,
  title,
  unreachable = false,
}: {
  children: ReactNode;
  count: number;
  emptyText?: string;
  headingId: string;
  listLabel: string;
  title: string;
  unreachable?: boolean;
}) {
  if (!unreachable && !count && !emptyText) return null;

  return (
    <section aria-labelledby={headingId}>
      <InspectorSectionHeader
        headingId={headingId}
        meta={unreachable ? undefined : formatCount(count, "deployment")}
        title={title}
      />
      {unreachable ? (
        <DetailList>
          <Detail
            label="Status"
            value={<StatusBadge status={{ label: "Unreachable", tone: "destructive" }} />}
          />
        </DetailList>
      ) : count ? (
        <InspectorRowList ariaLabel={listLabel}>{children}</InspectorRowList>
      ) : (
        <p className="mt-3 text-xs text-muted-foreground">{emptyText}</p>
      )}
    </section>
  );
}

/** One deployment of a `DeploymentListSection`, opening the deployment's inspector on select. */
export function DeploymentRow({
  description,
  name,
  onSelect,
  status,
}: {
  /** Description parts joined with a middle dot; missing parts are skipped. */
  description: readonly (string | undefined)[];
  name: string;
  onSelect: () => void;
  status: StatusProjection;
}) {
  const parts = description.filter(Boolean);

  return (
    <InspectorRow
      accessibleLabel={`View deployment ${name}, ${status.label}`}
      description={parts.length ? parts.join(" · ") : undefined}
      label={name}
      onSelect={onSelect}
      trailing={<StatusBadge status={status} />}
    />
  );
}
