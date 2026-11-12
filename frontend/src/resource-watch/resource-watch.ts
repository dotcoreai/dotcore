import { createContext, useCallback, useContext, useEffect, useSyncExternalStore } from "react";

import type { KeyedResource } from "@/gen/dotcore/watch/v1/service_pb";
import type { ResourceWatchStore } from "@/resource-watch/resource-watch-store";

/** A resource kind the console watches, named as its payload on the watch stream. */
export type ResourceKind = NonNullable<KeyedResource["payload"]["case"]>;

/** The owner read each resource kind delivers. */
export type ResourcePayload<Kind extends ResourceKind> = Extract<
  KeyedResource["payload"],
  { case: Kind }
>["value"];

/**
 * The latest read of one watched kind: no value while loading, before its first payload; `failed`
 * while its stream is down or reconnecting, with the last value kept in view.
 */
export interface ResourceWatchRead<T> {
  failed: boolean;
  value?: T;
}

/**
 * The manager process the watch last heard from. `generation` counts manager restarts seen since
 * the console loaded, starting at 0; one-shot reads of data immutable per manager process, such as
 * catalogs and metric descriptors, depend on it to read again after a restart.
 */
export interface ManagerSession {
  generation: number;
  id?: string;
}

export const ResourceWatchContext = createContext<ResourceWatchStore | null>(null);

/** Configured machines, watched while mounted and `enabled`. */
export function useWatchedMachines(options?: WatchOptions) {
  return useWatchedResource("machines", options);
}

/** Current hardware snapshot of every configured machine, watched while mounted and `enabled`. */
export function useWatchedHardwareSnapshots(options?: WatchOptions) {
  return useWatchedResource("hardwareSnapshots", options);
}

/** Model deployments, watched while mounted and `enabled`. */
export function useWatchedModelDeployments(options?: WatchOptions) {
  return useWatchedResource("modelDeployments", options);
}

/** Application deployments, watched while mounted and `enabled`. */
export function useWatchedApplicationDeployments(options?: WatchOptions) {
  return useWatchedResource("applicationDeployments", options);
}

/**
 * Every site node's memory pools with what occupies them, as the capacity component draws them,
 * watched while mounted and `enabled`.
 */
export function useWatchedNodeMemory(options?: WatchOptions) {
  return useWatchedResource("nodeMemory", options);
}

/** The site-wide console settings, watched while mounted and `enabled`. */
export function useWatchedConsoleSettings(options?: WatchOptions) {
  return useWatchedResource("consoleSettings", options);
}

/** Inference routes, watched while mounted and `enabled`. */
export function useWatchedRoutes(options?: WatchOptions) {
  return useWatchedResource("routes", options);
}

/** Configured execution targets, watched while mounted and `enabled`. */
export function useWatchedExecutionTargets(options?: WatchOptions) {
  return useWatchedResource("executionTargets", options);
}

/** The manager session of the console's resource watch. */
export function useManagerSession(): ManagerSession {
  const store = useResourceWatchStore();

  return useSyncExternalStore(store.subscribe, store.session);
}

/**
 * The manager session of the console's resource watch, or a session that never changes outside a
 * watch provider, for components that also render in isolation.
 */
export function useOptionalManagerSession(): ManagerSession {
  const store = useContext(ResourceWatchContext);

  return useSyncExternalStore(store?.subscribe ?? subscribeNever, store?.session ?? noSession);
}

export interface WatchOptions {
  /** Whether to watch; defaults to true. A disabled watch reads `undefined`. */
  enabled?: boolean;
}

/**
 * Registers `kind` with the console's resource watch while mounted and enabled, and reads its
 * latest payload. Components needing the same kind share one subscription on the one stream.
 */
function useWatchedResource<Kind extends ResourceKind>(
  kind: Kind,
  { enabled = true }: WatchOptions = {},
): ResourceWatchRead<ResourcePayload<Kind>> | undefined {
  const store = useResourceWatchStore();
  const read = useSyncExternalStore(
    store.subscribe,
    useCallback(() => store.read(kind), [kind, store]),
  );

  useEffect(() => {
    if (!enabled) return;

    return store.register(kind);
  }, [enabled, kind, store]);

  return enabled ? read : undefined;
}

const noManagerSession: ManagerSession = { generation: 0 };

function noSession(): ManagerSession {
  return noManagerSession;
}

function subscribeNever(): () => void {
  return () => {};
}

function useResourceWatchStore(): ResourceWatchStore {
  const store = useContext(ResourceWatchContext);

  if (!store) {
    throw new Error("resource watch hooks must be used within a ResourceWatchProvider");
  }

  return store;
}
