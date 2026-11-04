import { useSyncExternalStore } from "react";

import { getBrowserStorage } from "@/lib/browser-storage";
import {
  CONSOLE_PREFERENCES_STORAGE_KEY,
  type DeveloperSettings,
  readDeveloperSettings,
  subscribeToDeveloperSettings,
} from "@/lib/console-preferences";

/** Returns the stored developer settings and re-renders when this tab or another tab changes them. */
export function useDeveloperSettings(): DeveloperSettings {
  return useSyncExternalStore(subscribe, getSnapshot);
}

// useSyncExternalStore needs a referentially stable snapshot, so the decoded value is reused
// while the stored string is unchanged.
let cachedSnapshot: { raw: string | null; settings: DeveloperSettings } | undefined;

function getSnapshot(): DeveloperSettings {
  const storage = getBrowserStorage();
  let raw: string | null = null;

  try {
    raw = storage?.getItem(CONSOLE_PREFERENCES_STORAGE_KEY) ?? null;
  } catch {
    raw = null;
  }

  if (cachedSnapshot?.raw !== raw || !cachedSnapshot) {
    cachedSnapshot = { raw, settings: readDeveloperSettings(storage) };
  }

  return cachedSnapshot.settings;
}

function subscribe(onChange: () => void): () => void {
  return subscribeToDeveloperSettings(globalThis.window, getBrowserStorage(), onChange);
}
