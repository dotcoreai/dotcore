import type { PanelEntry, PanelPage, PanelScreen } from "./panel-stack";

/** A resource name a page knows, keyed by the panel entry that names the resource. */
export interface PanelTitle {
  entry: PanelEntry;
  name: string;
}

/** A title for the resource a page, screen and ID name. */
export function panelTitle(
  page: PanelPage,
  screen: PanelScreen,
  resourceId: string,
  name: string,
): PanelTitle {
  return { entry: { page, screen, resourceId }, name };
}

/**
 * The key of the resource a panel entry names, or undefined for an entry that names none (a create
 * form). Screens of one resource share its key, so a name published for a deployment also serves its
 * edit, logs and configuration screens, and a catalog profile shares its catalog model's key.
 */
export function panelTitleKey(entry: PanelEntry): string | undefined {
  if (!entry.resourceId) return undefined;
  if (entry.page === "machines" && entry.screen === "execution-target") {
    return `machines/execution-target:${entry.resourceId}`;
  }

  return `${entry.page}:${entry.resourceId}`;
}

/** The generic name of what an entry names, shown while its own name is not known. */
export function panelKindLabel(entry: PanelEntry): string {
  switch (entry.page) {
    case "apps":
      return "application deployment";
    case "app-catalog":
      return "application";
    case "machines":
      return entry.screen === "execution-target" ? "execution target" : "machine";
    case "model-catalog":
      return "model";
    case "models":
      return "model deployment";
    case "routes":
      return "route";
  }
}

/**
 * Session-wide names of the resources the console has loaded, published by the pages that own them
 * and read by the inspector header to label its back target. Publishing an unchanged name does not
 * notify, so a polling page does not re-render every subscriber each tick.
 */
export class PanelTitleStore {
  private readonly titles = new Map<string, string>();
  private readonly listeners = new Set<() => void>();

  get(key: string): string | undefined {
    return this.titles.get(key);
  }

  /** Stores the titles and returns whether any name was added or changed. */
  publish(titles: readonly PanelTitle[]): boolean {
    let changed = false;
    for (const { entry, name } of titles) {
      const key = panelTitleKey(entry);
      if (!key || !name || this.titles.get(key) === name) continue;

      this.titles.set(key, name);
      changed = true;
    }
    if (changed) this.listeners.forEach((listener) => listener());

    return changed;
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }
}
