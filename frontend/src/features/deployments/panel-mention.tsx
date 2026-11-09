import type { ReactNode } from "react";

import { ResourceMarker } from "@/components/ui/resource-marker";
import { PanelLink } from "@/panels/panel-navigation";

/** A route a deployment inspector mentions, with the link that pushes its route inspector. */
export interface RouteMention {
  href: string;
  inferenceRouteId: string;
  name: string;
}

/**
 * A marked mention of another resource inside an inspector. With a destination it is a link that
 * pushes that resource's inspector onto the panel stack; without one it is plain marked text.
 */
export function PanelMention({
  children,
  colorKey,
  href,
}: {
  children: ReactNode;
  colorKey: string;
  href?: string;
}) {
  const mention = (
    <>
      <ResourceMarker colorKey={colorKey} />
      {children}
    </>
  );

  return href ? (
    <PanelLink className="text-primary underline-offset-4 hover:underline" to={href}>
      {mention}
    </PanelLink>
  ) : (
    mention
  );
}

/** The routes containing a model deployment, each linking to its route inspector. */
export function RouteMentions({ routes }: { routes?: readonly RouteMention[] }) {
  if (!routes?.length) return <span className="text-muted-foreground">No route configured.</span>;

  return (
    <span className="flex flex-wrap justify-end gap-x-2.5 gap-y-1">
      {routes.map((route) => (
        <PanelMention
          colorKey={route.inferenceRouteId}
          href={route.href}
          key={route.inferenceRouteId}
        >
          {route.name}
        </PanelMention>
      ))}
    </span>
  );
}
