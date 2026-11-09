import { cn } from "@/lib/utils";
import { useResourceColors } from "@/theme/resource-color-context";

interface ResourceMarkerProps {
  colorKey: string;
  className?: string;
}

interface ResourceTitleProps {
  colorKey: string;
  name: string;
  screen?: string;
}

/**
 * A resource title led by its identity marker: `■ name`, or `■ name · screen` for a sub-screen or
 * dialog scoped to that resource. The marker is hidden from assistive technology, so the accessible
 * title is the visible text alone.
 */
function ResourceTitle({ colorKey, name, screen }: ResourceTitleProps) {
  return (
    <>
      <ResourceMarker colorKey={colorKey} />
      {screen ? `${name} · ${screen}` : name}
    </>
  );
}

/**
 * The small filled, rounded identity marker placed before a resource's name. Its color is the
 * resource's site-wide marker color and is decorative only: visible text always identifies the
 * resource.
 */
function ResourceMarker({ colorKey, className }: ResourceMarkerProps) {
  const { colorOf } = useResourceColors();

  return (
    <span
      aria-hidden="true"
      className={cn("mr-1.5 inline-block size-2.5 shrink-0 rounded-[3px] align-middle", className)}
      data-slot="resource-marker"
      style={{ backgroundColor: colorOf(colorKey) }}
    />
  );
}

export { ResourceMarker, ResourceTitle };
