import { type RouteSidebarHostProps, useRouteSidebarHost } from "./use-route-sidebar-host";

/** The route sidebar of a page that shows no route list of its own, such as the overview. */
export function RouteSidebarHost(props: RouteSidebarHostProps) {
  return useRouteSidebarHost(props).sidebar;
}
