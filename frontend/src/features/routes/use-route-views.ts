import { useEffect, useMemo, useState } from "react";

import { useWatchedModelDeployments, useWatchedRoutes } from "@/app/resource-watch";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";

/**
 * An accepted route mutation shown over the streamed routes until the stream shows it. A `put`
 * (create or replace) shows the accepted view until the stream lists it at that generation or later;
 * a `remove` hides the route until the stream no longer lists it.
 */
type RouteOverlay =
  | {
      /** Whether the route was not listed yet when accepted, so its absence is not a delete. */
      created: boolean;
      kind: "put";
      view: InferenceRouteView;
    }
  | { kind: "remove" };

/** The routes and model deployments of the routes page, from the console's resource watch. */
export interface RouteViews {
  /** Shows an accepted route create or replace until the stream shows it. */
  acceptRoute: (view: InferenceRouteView) => void;
  deployments?: ModelDeployment[];
  /** Why the routes could not be loaded, while the watch is down before their first payload. */
  loadError?: string;
  /** Hides a route whose deletion was accepted until the stream no longer lists it. */
  removeRoute: (inferenceRouteId: string) => void;
  /** Absent until the first payload. */
  routes?: InferenceRouteView[];
}

/**
 * Routes and model deployments streamed from the resource watch with accepted route mutations laid
 * over them. While `hold` is set (a route is being edited or saved) the streamed lists stay as they
 * were when the hold began, so a stream update cannot replace the form's route; accepted mutations
 * still show.
 */
export function useRouteViews(hold: boolean): RouteViews {
  const watchedRoutes = useWatchedRoutes();
  const watchedDeployments = useWatchedModelDeployments();
  const streamedRoutes = useHeld(watchedRoutes?.value?.routes, hold);
  const deployments = useHeld(watchedDeployments?.value?.deployments, hold);
  const [overlays, setOverlays] = useState<ReadonlyMap<string, RouteOverlay>>(() => new Map());

  const merged = useMemo(
    () => (streamedRoutes ? mergeRouteOverlays(streamedRoutes, overlays) : undefined),
    [overlays, streamedRoutes],
  );

  // Overlays the stream shows are dropped, so a later change made elsewhere is not hidden by them.
  useEffect(() => {
    if (!merged?.settled.length) return;

    setOverlays((current) => {
      const next = new Map(current);
      for (const routeId of merged.settled) next.delete(routeId);
      return next;
    });
  }, [merged]);

  const acceptRoute = (view: InferenceRouteView) => {
    const routeId = view.route?.inferenceRouteId;
    if (!routeId) return;

    const created = !streamedRoutes?.some(
      (candidate) => candidate.route?.inferenceRouteId === routeId,
    );
    setOverlays((current) => new Map(current).set(routeId, { created, kind: "put", view }));
  };

  const removeRoute = (inferenceRouteId: string) =>
    setOverlays((current) => new Map(current).set(inferenceRouteId, { kind: "remove" }));

  return {
    acceptRoute,
    deployments,
    loadError:
      watchedRoutes?.failed && !watchedRoutes.value
        ? "Loading routes failed: the resource watch is unavailable."
        : undefined,
    removeRoute,
    routes: merged?.routes,
  };
}

/**
 * Applies the overlays to the streamed routes and names the overlays the stream already shows. A
 * payload read before a mutation but delivered after its response still shows the earlier
 * generation, so it does not settle the mutation.
 */
function mergeRouteOverlays(
  streamed: readonly InferenceRouteView[],
  overlays: ReadonlyMap<string, RouteOverlay>,
): { routes: InferenceRouteView[]; settled: string[] } {
  const routes = [...streamed];
  const settled: string[] = [];

  for (const [routeId, overlay] of overlays) {
    const index = routes.findIndex((view) => view.route?.inferenceRouteId === routeId);

    if (overlay.kind === "remove") {
      if (index < 0) settled.push(routeId);
      else routes.splice(index, 1);
    } else if (index < 0) {
      // A created route the stream has not listed yet stays; any other absence is a delete.
      if (overlay.created) routes.push(overlay.view);
      else settled.push(routeId);
    } else if ((routes[index].route?.generation ?? 0n) >= (overlay.view.route?.generation ?? 0n)) {
      settled.push(routeId);
    } else {
      routes[index] = overlay.view;
    }
  }

  return { routes, settled };
}

/** The value as it was when `hold` began, until `hold` ends; a value still loading is not held. */
function useHeld<T>(value: T | undefined, hold: boolean): T | undefined {
  const [held, setHeld] = useState<{ value: T }>();

  if (hold && !held && value !== undefined) setHeld({ value });
  else if (!hold && held) setHeld(undefined);

  return hold && held ? held.value : value;
}
