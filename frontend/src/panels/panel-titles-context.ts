import { createContext, useContext, useEffect, useRef, useSyncExternalStore } from "react";

import type { PanelEntry } from "./panel-stack";
import {
  panelKindLabel,
  type PanelTitle,
  panelTitleKey,
  type PanelTitleStore,
} from "./panel-titles";

/** Reads one resource's name from the backend, or undefined when it is not known. */
export type PanelTitleLookup = (
  entry: PanelEntry,
  signal: AbortSignal,
) => Promise<string | undefined>;

/** The entry the sidebar's back button returns to, provided by the panel navigation. */
export const PanelParentContext = createContext<PanelEntry | undefined>(undefined);

export const PanelTitleStoreContext = createContext<PanelTitleStore | undefined>(undefined);
export const PanelTitleLookupContext = createContext<PanelTitleLookup | undefined>(undefined);

/**
 * Publishes the names of the resources a page has loaded so an inspector opened from them, on this
 * or another page, can label its back target. Call it with the page's lists on every render; only a
 * changed name notifies.
 */
export function usePublishPanelTitles(titles: readonly PanelTitle[]): void {
  const store = useContext(PanelTitleStoreContext);

  useEffect(() => {
    store?.publish(titles);
  });
}

/**
 * The name a page published for `entry`'s resource this session, or undefined until one has. Lets a
 * panel opened on another page name its resource before its own list is read.
 */
export function usePanelTitle(entry: PanelEntry | undefined): string | undefined {
  const store = useContext(PanelTitleStoreContext);
  const key = entry ? panelTitleKey(entry) : undefined;

  return useSyncExternalStore(
    (listener) => store?.subscribe(listener) ?? noop,
    () => (key ? store?.get(key) : undefined),
  );
}

/**
 * The entry the sidebar's back button returns to and its display label: the published name, else a
 * name read once through the provider's lookup, else the kind of resource. The entry is the one
 * beneath the entry the sidebar shows, which a log window drawn over the sidebar does not change.
 * Undefined when the sidebar shows the first entry in the stack.
 *
 * Every header reads this same parent, so a header rendered for another entry shows the same label.
 */
export function usePanelParent(): { entry: PanelEntry; label: string } | undefined {
  const entry = useContext(PanelParentContext);
  const store = useContext(PanelTitleStoreContext);
  const lookup = useContext(PanelTitleLookupContext);
  const key = entry ? panelTitleKey(entry) : undefined;
  const entryRef = useRef(entry);

  useEffect(() => {
    entryRef.current = entry;
  });

  const published = usePanelTitle(entry);

  useEffect(() => {
    const target = entryRef.current;
    if (!store || !lookup || !target || !key || store.get(key) !== undefined) return;

    const controller = new AbortController();
    lookup(target, controller.signal)
      .then((name) => {
        if (name && !controller.signal.aborted) store.publish([{ entry: target, name }]);
      })
      // The kind label stays when the read fails; there is nothing to present.
      .catch(() => undefined);
    return () => controller.abort();
  }, [key, lookup, store]);

  if (!entry) return undefined;

  return { entry, label: published ?? panelKindLabel(entry) };
}

function noop() {}
