import { create, fromBinary, toBinary } from "@bufbuild/protobuf";

import {
  ColorScheme,
  type ConsolePreferences,
  ConsolePreferencesSchema,
  DeveloperSettingsSchema,
  MemoryView,
} from "@/gen/dotcore/console/v1/preferences_pb";
import { getBrowserStorage, type StorageLike } from "@/lib/browser-storage";

export const CONSOLE_PREFERENCES_STORAGE_KEY = "dotcore.console.preferences.v1";

export type ColorSchemeSubscriber = (colorScheme: ColorScheme) => void;
export type SidebarCollapsedSubscriber = (collapsed: boolean) => void;
export type MemoryViewSubscriber = (memoryView: MemoryView) => void;
export type GetStartedDismissedSubscriber = (dismissed: boolean) => void;
export type DeveloperSettingsSubscriber = (settings: DeveloperSettings) => void;

/** Synthetic latency applied to every Console API call while enabled. */
export interface DeveloperSettings {
  latencyEnabled: boolean;
  latencyMinMs: number;
  latencyMaxMs: number;
}

/** Upper bound of the synthetic latency, above every request deadline the Console uses. */
export const MAX_LATENCY_MS = 30_000;

export const DEFAULT_DEVELOPER_SETTINGS: DeveloperSettings = {
  latencyEnabled: false,
  latencyMinMs: 0,
  latencyMaxMs: 0,
};

// Same-tab writes fire no `storage` event, so writers notify in-process listeners directly.
const developerSettingsListeners = new Set<() => void>();
const getStartedDismissedListeners = new Set<() => void>();

export function readColorScheme(storage = getBrowserStorage()): ColorScheme {
  try {
    return decodeColorScheme(storage?.getItem(CONSOLE_PREFERENCES_STORAGE_KEY) ?? null);
  } catch {
    return ColorScheme.DARK;
  }
}

export function writeColorScheme(storage: StorageLike | undefined, colorScheme: ColorScheme): void {
  try {
    // Decode-modify-encode: a write must round-trip the sibling fields' current stored values
    // rather than starting from a blank message, or it would blank them.
    const preferences =
      decodePreferences(storage?.getItem(CONSOLE_PREFERENCES_STORAGE_KEY) ?? null) ??
      create(ConsolePreferencesSchema);
    preferences.colorScheme =
      colorScheme === ColorScheme.DARK ? ColorScheme.DARK : ColorScheme.LIGHT;

    storage?.setItem(CONSOLE_PREFERENCES_STORAGE_KEY, encodePreferences(preferences));
  } catch {
    // Browser storage is an optional convenience; the in-memory selection remains usable.
  }
}

export function subscribeToColorScheme(
  eventTarget: EventTarget | undefined,
  storage: StorageLike | undefined,
  subscriber: ColorSchemeSubscriber,
): () => void {
  if (!eventTarget) {
    return () => undefined;
  }

  const handleStorage = (event: Event) => {
    const storageEvent = event as StorageEvent;

    if (
      storageEvent.key !== CONSOLE_PREFERENCES_STORAGE_KEY ||
      (storageEvent.storageArea !== null &&
        storage !== undefined &&
        storageEvent.storageArea !== storage)
    ) {
      return;
    }

    subscriber(decodeColorScheme(storageEvent.newValue));
  };

  eventTarget.addEventListener("storage", handleStorage);

  return () => eventTarget.removeEventListener("storage", handleStorage);
}

export function readSidebarCollapsed(storage = getBrowserStorage()): boolean {
  try {
    return decodeSidebarCollapsed(storage?.getItem(CONSOLE_PREFERENCES_STORAGE_KEY) ?? null);
  } catch {
    return false;
  }
}

export function writeSidebarCollapsed(storage: StorageLike | undefined, collapsed: boolean): void {
  try {
    // Decode-modify-encode: a write must round-trip the sibling fields' current stored values
    // rather than starting from a blank message, or it would blank them.
    const preferences =
      decodePreferences(storage?.getItem(CONSOLE_PREFERENCES_STORAGE_KEY) ?? null) ??
      create(ConsolePreferencesSchema);
    preferences.sidebarCollapsed = collapsed;

    storage?.setItem(CONSOLE_PREFERENCES_STORAGE_KEY, encodePreferences(preferences));
  } catch {
    // Browser storage is an optional convenience; the in-memory selection remains usable.
  }
}

export function subscribeToSidebarCollapsed(
  eventTarget: EventTarget | undefined,
  storage: StorageLike | undefined,
  subscriber: SidebarCollapsedSubscriber,
): () => void {
  if (!eventTarget) {
    return () => undefined;
  }

  const handleStorage = (event: Event) => {
    const storageEvent = event as StorageEvent;

    if (
      storageEvent.key !== CONSOLE_PREFERENCES_STORAGE_KEY ||
      (storageEvent.storageArea !== null &&
        storage !== undefined &&
        storageEvent.storageArea !== storage)
    ) {
      return;
    }

    subscriber(decodeSidebarCollapsed(storageEvent.newValue));
  };

  eventTarget.addEventListener("storage", handleStorage);

  return () => eventTarget.removeEventListener("storage", handleStorage);
}

export function readMemoryView(storage = getBrowserStorage()): MemoryView {
  try {
    return decodeMemoryView(storage?.getItem(CONSOLE_PREFERENCES_STORAGE_KEY) ?? null);
  } catch {
    return MemoryView.ALL_MACHINES;
  }
}

export function writeMemoryView(storage: StorageLike | undefined, memoryView: MemoryView): void {
  try {
    // Decode-modify-encode keeps the sibling fields, as the other writers do.
    const preferences =
      decodePreferences(storage?.getItem(CONSOLE_PREFERENCES_STORAGE_KEY) ?? null) ??
      create(ConsolePreferencesSchema);
    preferences.memoryView =
      memoryView === MemoryView.BY_MACHINE ? MemoryView.BY_MACHINE : MemoryView.ALL_MACHINES;

    storage?.setItem(CONSOLE_PREFERENCES_STORAGE_KEY, encodePreferences(preferences));
  } catch {
    // Browser storage is an optional convenience; the in-memory selection remains usable.
  }
}

export function subscribeToMemoryView(
  eventTarget: EventTarget | undefined,
  storage: StorageLike | undefined,
  subscriber: MemoryViewSubscriber,
): () => void {
  if (!eventTarget) {
    return () => undefined;
  }

  const handleStorage = (event: Event) => {
    const storageEvent = event as StorageEvent;

    if (
      storageEvent.key !== CONSOLE_PREFERENCES_STORAGE_KEY ||
      (storageEvent.storageArea !== null &&
        storage !== undefined &&
        storageEvent.storageArea !== storage)
    ) {
      return;
    }

    subscriber(decodeMemoryView(storageEvent.newValue));
  };

  eventTarget.addEventListener("storage", handleStorage);

  return () => eventTarget.removeEventListener("storage", handleStorage);
}

/** Reports whether the operator dismissed Get started; it reads as shown until dismissed. */
export function readGetStartedDismissed(storage = getBrowserStorage()): boolean {
  try {
    return decodeGetStartedDismissed(storage?.getItem(CONSOLE_PREFERENCES_STORAGE_KEY) ?? null);
  } catch {
    return false;
  }
}

/** Stores whether Get started is dismissed and notifies same-tab listeners. */
export function writeGetStartedDismissed(
  storage: StorageLike | undefined,
  dismissed: boolean,
): void {
  try {
    // Decode-modify-encode keeps the sibling fields, as the other writers do.
    const preferences =
      decodePreferences(storage?.getItem(CONSOLE_PREFERENCES_STORAGE_KEY) ?? null) ??
      create(ConsolePreferencesSchema);
    preferences.getStartedDismissed = dismissed;

    storage?.setItem(CONSOLE_PREFERENCES_STORAGE_KEY, encodePreferences(preferences));
  } catch {
    // Browser storage is an optional convenience; listeners still observe the selection below.
  }

  for (const listener of [...getStartedDismissedListeners]) {
    listener();
  }
}

/** Subscribes to the dismissed preference changed by this tab's writers or by another tab. */
export function subscribeToGetStartedDismissed(
  eventTarget: EventTarget | undefined,
  storage: StorageLike | undefined,
  subscriber: GetStartedDismissedSubscriber,
): () => void {
  const handleLocalWrite = () => subscriber(readGetStartedDismissed(storage));
  getStartedDismissedListeners.add(handleLocalWrite);

  const handleStorage = (event: Event) => {
    const storageEvent = event as StorageEvent;

    if (
      storageEvent.key !== CONSOLE_PREFERENCES_STORAGE_KEY ||
      (storageEvent.storageArea !== null &&
        storage !== undefined &&
        storageEvent.storageArea !== storage)
    ) {
      return;
    }

    subscriber(decodeGetStartedDismissed(storageEvent.newValue));
  };

  eventTarget?.addEventListener("storage", handleStorage);

  return () => {
    getStartedDismissedListeners.delete(handleLocalWrite);
    eventTarget?.removeEventListener("storage", handleStorage);
  };
}

/** Reports whether both bounds are whole milliseconds with 0 <= min <= max <= MAX_LATENCY_MS. */
export function isValidDeveloperSettings(settings: DeveloperSettings): boolean {
  const { latencyMinMs: min, latencyMaxMs: max } = settings;

  return (
    Number.isInteger(min) &&
    Number.isInteger(max) &&
    min >= 0 &&
    min <= max &&
    max <= MAX_LATENCY_MS
  );
}

/**
 * Reads the stored developer settings. Absent, unreadable or out-of-range values yield the defaults
 * for the whole struct, so a half-valid range never reaches a caller.
 */
export function readDeveloperSettings(storage = getBrowserStorage()): DeveloperSettings {
  try {
    return decodeDeveloperSettings(storage?.getItem(CONSOLE_PREFERENCES_STORAGE_KEY) ?? null);
  } catch {
    return { ...DEFAULT_DEVELOPER_SETTINGS };
  }
}

/** Stores the settings and notifies same-tab listeners. Returns false, writing nothing, when invalid. */
export function writeDeveloperSettings(
  storage: StorageLike | undefined,
  settings: DeveloperSettings,
): boolean {
  if (!isValidDeveloperSettings(settings)) {
    return false;
  }

  try {
    // Decode-modify-encode keeps the sibling fields, as the other writers do.
    const preferences =
      decodePreferences(storage?.getItem(CONSOLE_PREFERENCES_STORAGE_KEY) ?? null) ??
      create(ConsolePreferencesSchema);
    preferences.developerSettings = create(DeveloperSettingsSchema, settings);

    storage?.setItem(CONSOLE_PREFERENCES_STORAGE_KEY, encodePreferences(preferences));
  } catch {
    // Browser storage is an optional convenience; listeners still observe the selection below.
  }

  for (const listener of [...developerSettingsListeners]) {
    listener();
  }

  return true;
}

/** Subscribes to developer settings changed by this tab's writers or by another tab. */
export function subscribeToDeveloperSettings(
  eventTarget: EventTarget | undefined,
  storage: StorageLike | undefined,
  subscriber: DeveloperSettingsSubscriber,
): () => void {
  const handleLocalWrite = () => subscriber(readDeveloperSettings(storage));
  developerSettingsListeners.add(handleLocalWrite);

  const handleStorage = (event: Event) => {
    const storageEvent = event as StorageEvent;

    if (
      storageEvent.key !== CONSOLE_PREFERENCES_STORAGE_KEY ||
      (storageEvent.storageArea !== null &&
        storage !== undefined &&
        storageEvent.storageArea !== storage)
    ) {
      return;
    }

    subscriber(decodeDeveloperSettings(storageEvent.newValue));
  };

  eventTarget?.addEventListener("storage", handleStorage);

  return () => {
    developerSettingsListeners.delete(handleLocalWrite);
    eventTarget?.removeEventListener("storage", handleStorage);
  };
}

export function applyColorScheme(document: Document | undefined, colorScheme: ColorScheme): void {
  if (!document) {
    return;
  }

  const isDark = colorScheme === ColorScheme.DARK;
  document.documentElement.classList.toggle("dark", isDark);
  document.documentElement.style.colorScheme = isDark ? "dark" : "light";
}

export function initializeColorScheme(
  document: Document | undefined,
  storage = getBrowserStorage(),
): ColorScheme {
  const colorScheme = readColorScheme(storage);
  applyColorScheme(document, colorScheme);

  return colorScheme;
}

function decodeColorScheme(value: string | null): ColorScheme {
  const preferences = decodePreferences(value);

  return preferences?.colorScheme === ColorScheme.LIGHT ? ColorScheme.LIGHT : ColorScheme.DARK;
}

function decodeSidebarCollapsed(value: string | null): boolean {
  const preferences = decodePreferences(value);

  return preferences?.sidebarCollapsed ?? false;
}

function decodeMemoryView(value: string | null): MemoryView {
  const preferences = decodePreferences(value);

  return preferences?.memoryView === MemoryView.BY_MACHINE
    ? MemoryView.BY_MACHINE
    : MemoryView.ALL_MACHINES;
}

function decodeGetStartedDismissed(value: string | null): boolean {
  return decodePreferences(value)?.getStartedDismissed ?? false;
}

function decodeDeveloperSettings(value: string | null): DeveloperSettings {
  const stored = decodePreferences(value)?.developerSettings;

  if (!stored) {
    return { ...DEFAULT_DEVELOPER_SETTINGS };
  }

  const settings: DeveloperSettings = {
    latencyEnabled: stored.latencyEnabled,
    latencyMinMs: stored.latencyMinMs,
    latencyMaxMs: stored.latencyMaxMs,
  };

  return isValidDeveloperSettings(settings) ? settings : { ...DEFAULT_DEVELOPER_SETTINGS };
}

function decodePreferences(value: string | null): ConsolePreferences | undefined {
  if (!value) {
    return undefined;
  }

  try {
    return fromBinary(
      ConsolePreferencesSchema,
      Uint8Array.from(atob(value), (byte) => byte.codePointAt(0)!),
    );
  } catch {
    return undefined;
  }
}

function encodePreferences(preferences: ConsolePreferences): string {
  const bytes = toBinary(ConsolePreferencesSchema, preferences);

  let binary = "";

  for (let offset = 0; offset < bytes.length; offset += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
  }

  return btoa(binary);
}
