import { createContext, useContext, useEffect, useSyncExternalStore } from "react";

/** Sets the hovered deployment, or clears it with `undefined`; inert outside a provider. */
export function useMemoryHoverSetter(): (deploymentId: string | undefined) => void {
  return useContext(MemoryHoverContext)?.set ?? ignoreHover;
}

/** The hovered deployment ID, or `undefined` when nothing is hovered or no provider exists. */
export function useHoveredDeployment(): string | undefined {
  const store = useContext(MemoryHoverContext);
  return useSyncExternalStore(store?.subscribe ?? subscribeNever, store?.get ?? getNothing);
}

/** Clears the link when this deployment's hovered element unmounts without a pointer leave. */
export function useReleaseHoverOnUnmount(deploymentId: string): void {
  const store = useContext(MemoryHoverContext);
  useEffect(
    () => () => {
      if (store?.get() === deploymentId) store.set(undefined);
    },
    [deploymentId, store],
  );
}

/**
 * How one deployment reads while another may be hovered: `active` when it is the hovered one,
 * `dimmed` when a different one is, and `undefined` when nothing is hovered.
 */
export function useDeploymentHoverState(deploymentId: string): "active" | "dimmed" | undefined {
  const store = useContext(MemoryHoverContext);
  return useSyncExternalStore(store?.subscribe ?? subscribeNever, () => {
    const hovered = store?.get();
    if (hovered === undefined) return undefined;
    return hovered === deploymentId ? "active" : "dimmed";
  });
}

export interface MemoryHoverStore {
  get: () => string | undefined;
  set: (deploymentId: string | undefined) => void;
  subscribe: (listener: () => void) => () => void;
}

export const MemoryHoverContext = createContext<MemoryHoverStore | undefined>(undefined);

export function createMemoryHoverStore(): MemoryHoverStore {
  let hovered: string | undefined;
  const listeners = new Set<() => void>();

  return {
    get: () => hovered,
    set: (deploymentId) => {
      if (deploymentId === hovered) return;
      hovered = deploymentId;
      listeners.forEach((listener) => listener());
    },
    subscribe: (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

function ignoreHover() {}

function subscribeNever() {
  return () => undefined;
}

function getNothing() {
  return undefined;
}
