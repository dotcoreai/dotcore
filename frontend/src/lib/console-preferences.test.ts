import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  ColorScheme,
  ConsolePreferencesSchema,
  MemoryView,
} from "@/gen/dotcore/console/v1/preferences_pb";
import {
  CONSOLE_PREFERENCES_STORAGE_KEY,
  DEFAULT_DEVELOPER_SETTINGS,
  type DeveloperSettings,
  initializeColorScheme,
  MAX_LATENCY_MS,
  readColorScheme,
  readDeveloperSettings,
  readGetStartedDismissed,
  readMemoryView,
  readSidebarCollapsed,
  subscribeToColorScheme,
  subscribeToDeveloperSettings,
  subscribeToGetStartedDismissed,
  subscribeToMemoryView,
  subscribeToSidebarCollapsed,
  writeColorScheme,
  writeDeveloperSettings,
  writeGetStartedDismissed,
  writeMemoryView,
  writeSidebarCollapsed,
} from "@/lib/console-preferences";

class FakeStorage {
  readonly getItem = vi.fn<(key: string) => string | null>();
  readonly removeItem = vi.fn<(key: string) => void>();
  readonly setItem = vi.fn<(key: string, value: string) => void>();
}

function encodeColorScheme(colorScheme: ColorScheme): string {
  const bytes = toBinary(
    ConsolePreferencesSchema,
    create(ConsolePreferencesSchema, { colorScheme }),
  );

  return encodeBytes(bytes);
}

function encodeSidebarCollapsed(collapsed: boolean): string {
  const bytes = toBinary(
    ConsolePreferencesSchema,
    create(ConsolePreferencesSchema, { sidebarCollapsed: collapsed }),
  );

  return encodeBytes(bytes);
}

function encodeMemoryView(memoryView: MemoryView): string {
  return encodeBytes(
    toBinary(ConsolePreferencesSchema, create(ConsolePreferencesSchema, { memoryView })),
  );
}

function encodeBytes(bytes: Uint8Array): string {
  let binary = "";

  for (let offset = 0; offset < bytes.length; offset += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
  }

  return btoa(binary);
}

function encodeVarint(value: number): Uint8Array {
  const bytes: number[] = [];
  let remaining = value;

  while (remaining > 0x7f) {
    bytes.push((remaining & 0x7f) | 0x80);
    remaining >>>= 7;
  }
  bytes.push(remaining);

  return Uint8Array.from(bytes);
}

describe("Console preferences", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it.each([ColorScheme.LIGHT, ColorScheme.DARK])(
    "round-trips %s as base64 protobuf binary",
    (colorScheme) => {
      const storage = new FakeStorage();

      writeColorScheme(storage, colorScheme);

      const encoded = storage.setItem.mock.calls[0]?.[1];
      if (!encoded) {
        throw new Error("Expected preference write");
      }

      expect(encoded).toMatch(/^[A-Za-z0-9+/]+={0,2}$/);
      expect(
        fromBinary(
          ConsolePreferencesSchema,
          Uint8Array.from(atob(encoded), (byte) => byte.codePointAt(0)!),
        ),
      ).toMatchObject({ colorScheme });
      storage.getItem.mockReturnValue(encoded);
      expect(readColorScheme(storage)).toBe(colorScheme);
    },
  );

  it("preserves compatible unknown protobuf fields while updating the scheme", () => {
    const storage = new FakeStorage();
    storage.getItem.mockReturnValue("CAF4Bw==");

    writeColorScheme(storage, ColorScheme.DARK);

    expect(storage.setItem).toHaveBeenCalledWith(CONSOLE_PREFERENCES_STORAGE_KEY, "CAJ4Bw==");
  });

  it("writes a selection while preserving a large valid unknown-field payload", () => {
    const storage = new FakeStorage();
    const unknownPayload = new Uint8Array(256 * 1024).fill(7);
    const length = encodeVarint(unknownPayload.length);
    const preferences = new Uint8Array(3 + length.length + unknownPayload.length);
    preferences.set([0x08, 0x01, 0x7a]);
    preferences.set(length, 3);
    preferences.set(unknownPayload, 3 + length.length);
    storage.getItem.mockReturnValue(encodeBytes(preferences));

    writeColorScheme(storage, ColorScheme.DARK);

    const written = storage.setItem.mock.calls[0]?.[1];
    if (!written) {
      throw new Error("Expected preference write");
    }

    const persisted = new FakeStorage();
    persisted.getItem.mockReturnValue(written);
    expect(readColorScheme(persisted)).toBe(ColorScheme.DARK);
    expect(atob(written).length).toBe(preferences.length);
  });

  it.each([
    ["missing state", null],
    ["corrupt base64", "not base64"],
    ["malformed protobuf", "CA=="],
    ["unspecified enum", encodeColorScheme(ColorScheme.UNSPECIFIED)],
    ["unknown enum", "CGM="],
  ])("returns Dark for %s", (_description, value) => {
    const storage = new FakeStorage();
    storage.getItem.mockReturnValue(value);

    expect(readColorScheme(storage)).toBe(ColorScheme.DARK);
  });

  it("returns Dark when storage cannot be read", () => {
    const storage = new FakeStorage();
    storage.getItem.mockImplementation(() => {
      throw new Error("Storage disabled");
    });

    expect(readColorScheme(storage)).toBe(ColorScheme.DARK);
  });

  it.each([ColorScheme.LIGHT, ColorScheme.DARK])(
    "applies the shared stored %s theme before React mounts",
    (colorScheme) => {
      const storage = new FakeStorage();
      storage.getItem.mockReturnValue(encodeColorScheme(colorScheme));
      document.documentElement.classList.toggle("dark", colorScheme === ColorScheme.DARK);
      document.documentElement.style.colorScheme = "";

      expect(initializeColorScheme(document, storage)).toBe(colorScheme);
      expect(document.documentElement.classList.contains("dark")).toBe(
        colorScheme === ColorScheme.DARK,
      );
      expect(document.documentElement.style.colorScheme).toBe(
        colorScheme === ColorScheme.DARK ? "dark" : "light",
      );
    },
  );

  it("keeps unrelated storage untouched when writing fails", () => {
    const storage = new FakeStorage();
    storage.setItem.mockImplementation(() => {
      throw new Error("Storage full");
    });

    expect(() => writeColorScheme(storage, ColorScheme.DARK)).not.toThrow();
    expect(storage.setItem).toHaveBeenCalledWith(
      CONSOLE_PREFERENCES_STORAGE_KEY,
      expect.any(String),
    );
    expect(storage.removeItem).not.toHaveBeenCalled();
  });

  it("notifies only for the owned key", () => {
    const events = new EventTarget();
    const storage = new FakeStorage();
    const subscriber = vi.fn();
    const unsubscribe = subscribeToColorScheme(events, storage, subscriber);

    events.dispatchEvent(new StorageEvent("storage", { key: "unrelated", newValue: "ignored" }));
    events.dispatchEvent(
      new StorageEvent("storage", {
        key: CONSOLE_PREFERENCES_STORAGE_KEY,
        newValue: encodeColorScheme(ColorScheme.DARK),
      }),
    );

    expect(subscriber).toHaveBeenCalledTimes(1);
    expect(subscriber).toHaveBeenCalledWith(ColorScheme.DARK);

    unsubscribe();
    events.dispatchEvent(
      new StorageEvent("storage", {
        key: CONSOLE_PREFERENCES_STORAGE_KEY,
        newValue: encodeColorScheme(ColorScheme.LIGHT),
      }),
    );
    expect(subscriber).toHaveBeenCalledTimes(1);
  });

  describe("sidebar_collapsed", () => {
    it.each([true, false])("round-trips %s as base64 protobuf binary", (collapsed) => {
      const storage = new FakeStorage();

      writeSidebarCollapsed(storage, collapsed);

      // `collapsed: false` alongside a default UNSPECIFIED color scheme is an all-zero-value
      // proto3 message, which serializes to zero bytes and base64-encodes to "" — a legitimate
      // written value, not a missing one — so this checks the call happened, not truthiness.
      expect(storage.setItem).toHaveBeenCalledTimes(1);
      const encoded = storage.setItem.mock.calls[0]?.[1] ?? "";

      expect(
        fromBinary(
          ConsolePreferencesSchema,
          Uint8Array.from(atob(encoded), (byte) => byte.codePointAt(0)!),
        ),
      ).toMatchObject({ sidebarCollapsed: collapsed });
      storage.getItem.mockReturnValue(encoded);
      expect(readSidebarCollapsed(storage)).toBe(collapsed);
    });

    it("preserves the color scheme when writing the collapsed state", () => {
      const storage = new FakeStorage();
      storage.getItem.mockReturnValue(encodeColorScheme(ColorScheme.DARK));

      writeSidebarCollapsed(storage, true);

      const written = storage.setItem.mock.calls[0]?.[1];
      if (!written) {
        throw new Error("Expected preference write");
      }

      const persisted = new FakeStorage();
      persisted.getItem.mockReturnValue(written);
      expect(readColorScheme(persisted)).toBe(ColorScheme.DARK);
      expect(readSidebarCollapsed(persisted)).toBe(true);
    });

    it("preserves the collapsed state when writing the color scheme", () => {
      const storage = new FakeStorage();
      storage.getItem.mockReturnValue(encodeSidebarCollapsed(true));

      writeColorScheme(storage, ColorScheme.DARK);

      const written = storage.setItem.mock.calls[0]?.[1];
      if (!written) {
        throw new Error("Expected preference write");
      }

      const persisted = new FakeStorage();
      persisted.getItem.mockReturnValue(written);
      expect(readSidebarCollapsed(persisted)).toBe(true);
      expect(readColorScheme(persisted)).toBe(ColorScheme.DARK);
    });

    it.each([
      ["missing state", null],
      ["corrupt base64", "not base64"],
      ["malformed protobuf", "CA=="],
    ])("returns expanded for %s", (_description, value) => {
      const storage = new FakeStorage();
      storage.getItem.mockReturnValue(value);

      expect(readSidebarCollapsed(storage)).toBe(false);
    });

    it("returns expanded when storage cannot be read", () => {
      const storage = new FakeStorage();
      storage.getItem.mockImplementation(() => {
        throw new Error("Storage disabled");
      });

      expect(readSidebarCollapsed(storage)).toBe(false);
    });

    it("notifies only for the owned key", () => {
      const events = new EventTarget();
      const storage = new FakeStorage();
      const subscriber = vi.fn();
      const unsubscribe = subscribeToSidebarCollapsed(events, storage, subscriber);

      events.dispatchEvent(new StorageEvent("storage", { key: "unrelated", newValue: "ignored" }));
      events.dispatchEvent(
        new StorageEvent("storage", {
          key: CONSOLE_PREFERENCES_STORAGE_KEY,
          newValue: encodeSidebarCollapsed(true),
        }),
      );

      expect(subscriber).toHaveBeenCalledTimes(1);
      expect(subscriber).toHaveBeenCalledWith(true);

      unsubscribe();
      events.dispatchEvent(
        new StorageEvent("storage", {
          key: CONSOLE_PREFERENCES_STORAGE_KEY,
          newValue: encodeSidebarCollapsed(false),
        }),
      );
      expect(subscriber).toHaveBeenCalledTimes(1);
    });
  });
  describe("memory_view", () => {
    it.each([MemoryView.ALL_MACHINES, MemoryView.BY_MACHINE])("round-trips view %s", (view) => {
      const storage = new FakeStorage();

      writeMemoryView(storage, view);

      const encoded = storage.setItem.mock.calls[0]?.[1] ?? "";
      storage.getItem.mockReturnValue(encoded);
      expect(readMemoryView(storage)).toBe(view);
    });

    it("preserves the color scheme and collapsed state when writing the view", () => {
      const storage = new FakeStorage();
      storage.getItem.mockReturnValue(
        encodeBytes(
          toBinary(
            ConsolePreferencesSchema,
            create(ConsolePreferencesSchema, {
              colorScheme: ColorScheme.LIGHT,
              sidebarCollapsed: true,
            }),
          ),
        ),
      );

      writeMemoryView(storage, MemoryView.BY_MACHINE);

      const persisted = new FakeStorage();
      persisted.getItem.mockReturnValue(storage.setItem.mock.calls[0]?.[1] ?? null);
      expect(readColorScheme(persisted)).toBe(ColorScheme.LIGHT);
      expect(readSidebarCollapsed(persisted)).toBe(true);
      expect(readMemoryView(persisted)).toBe(MemoryView.BY_MACHINE);
    });

    it("preserves the view when writing the collapsed state", () => {
      const storage = new FakeStorage();
      storage.getItem.mockReturnValue(encodeMemoryView(MemoryView.BY_MACHINE));

      writeSidebarCollapsed(storage, true);

      const persisted = new FakeStorage();
      persisted.getItem.mockReturnValue(storage.setItem.mock.calls[0]?.[1] ?? null);
      expect(readMemoryView(persisted)).toBe(MemoryView.BY_MACHINE);
    });

    it.each([
      ["missing state", null],
      ["corrupt base64", "not base64"],
      ["an unspecified view", encodeMemoryView(MemoryView.UNSPECIFIED)],
    ])("reads all machines for %s", (_description, value) => {
      const storage = new FakeStorage();
      storage.getItem.mockReturnValue(value);

      expect(readMemoryView(storage)).toBe(MemoryView.ALL_MACHINES);
    });

    it("reads all machines when storage cannot be read", () => {
      const storage = new FakeStorage();
      storage.getItem.mockImplementation(() => {
        throw new Error("Storage disabled");
      });

      expect(readMemoryView(storage)).toBe(MemoryView.ALL_MACHINES);
    });

    it("notifies another tab's view change for the owned key only", () => {
      const events = new EventTarget();
      const subscriber = vi.fn();
      const unsubscribe = subscribeToMemoryView(events, new FakeStorage(), subscriber);

      events.dispatchEvent(new StorageEvent("storage", { key: "unrelated", newValue: "ignored" }));
      events.dispatchEvent(
        new StorageEvent("storage", {
          key: CONSOLE_PREFERENCES_STORAGE_KEY,
          newValue: encodeMemoryView(MemoryView.BY_MACHINE),
        }),
      );

      expect(subscriber).toHaveBeenCalledTimes(1);
      expect(subscriber).toHaveBeenCalledWith(MemoryView.BY_MACHINE);

      unsubscribe();
      events.dispatchEvent(
        new StorageEvent("storage", {
          key: CONSOLE_PREFERENCES_STORAGE_KEY,
          newValue: encodeMemoryView(MemoryView.ALL_MACHINES),
        }),
      );
      expect(subscriber).toHaveBeenCalledTimes(1);
    });
  });
});

describe("Get started dismissed preference", () => {
  it("reads as shown by default and as dismissed once written, keeping sibling fields", () => {
    const storage = new MapStorage();
    expect(readGetStartedDismissed(storage)).toBe(false);

    writeSidebarCollapsed(storage, true);
    writeGetStartedDismissed(storage, true);
    expect(readGetStartedDismissed(storage)).toBe(true);
    expect(readSidebarCollapsed(storage)).toBe(true);

    writeGetStartedDismissed(storage, false);
    expect(readGetStartedDismissed(storage)).toBe(false);
    expect(readSidebarCollapsed(storage)).toBe(true);
  });

  it("notifies same-tab writes and other-tab storage events, and stops after unsubscribe", () => {
    const storage = new MapStorage();
    const eventTarget = new EventTarget();
    const subscriber = vi.fn();
    const unsubscribe = subscribeToGetStartedDismissed(eventTarget, storage, subscriber);

    writeGetStartedDismissed(storage, true);
    expect(subscriber).toHaveBeenLastCalledWith(true);

    eventTarget.dispatchEvent(
      new StorageEvent("storage", {
        key: CONSOLE_PREFERENCES_STORAGE_KEY,
        newValue: encodeBytes(toBinary(ConsolePreferencesSchema, create(ConsolePreferencesSchema))),
      }),
    );
    expect(subscriber).toHaveBeenLastCalledWith(false);
    expect(subscriber).toHaveBeenCalledTimes(2);

    unsubscribe();
    writeGetStartedDismissed(storage, true);
    expect(subscriber).toHaveBeenCalledTimes(2);
  });
});

class MapStorage {
  readonly values = new Map<string, string>();

  getItem(key: string) {
    return this.values.get(key) ?? null;
  }

  removeItem(key: string) {
    this.values.delete(key);
  }

  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}

describe("Developer settings preferences", () => {
  const enabled: DeveloperSettings = {
    latencyEnabled: true,
    latencyMinMs: 250,
    latencyMaxMs: 1_500,
  };

  it("reads the disabled defaults from empty storage", () => {
    expect(readDeveloperSettings(new MapStorage())).toEqual(DEFAULT_DEVELOPER_SETTINGS);
    expect(DEFAULT_DEVELOPER_SETTINGS).toEqual({
      latencyEnabled: false,
      latencyMinMs: 0,
      latencyMaxMs: 0,
    });
  });

  it("round-trips written settings", () => {
    const storage = new MapStorage();

    expect(writeDeveloperSettings(storage, enabled)).toBe(true);

    expect(readDeveloperSettings(storage)).toEqual(enabled);
  });

  it("keeps sibling preferences when writing, and the settings when siblings change", () => {
    const storage = new MapStorage();
    writeColorScheme(storage, ColorScheme.LIGHT);
    writeSidebarCollapsed(storage, true);
    writeMemoryView(storage, MemoryView.BY_MACHINE);

    writeDeveloperSettings(storage, enabled);

    expect(readColorScheme(storage)).toBe(ColorScheme.LIGHT);
    expect(readSidebarCollapsed(storage)).toBe(true);
    expect(readMemoryView(storage)).toBe(MemoryView.BY_MACHINE);
    expect(readDeveloperSettings(storage)).toEqual(enabled);

    writeColorScheme(storage, ColorScheme.DARK);
    expect(readDeveloperSettings(storage)).toEqual(enabled);
  });

  it.each([
    ["negative minimum", { ...enabled, latencyMinMs: -1 }],
    ["minimum above maximum", { ...enabled, latencyMinMs: 2_000, latencyMaxMs: 1_000 }],
    ["maximum above the cap", { ...enabled, latencyMaxMs: MAX_LATENCY_MS + 1 }],
    ["fractional minimum", { ...enabled, latencyMinMs: 1.5 }],
  ])("rejects a %s without writing", (_label, settings) => {
    const storage = new MapStorage();
    writeDeveloperSettings(storage, enabled);
    const before = storage.getItem(CONSOLE_PREFERENCES_STORAGE_KEY);

    expect(writeDeveloperSettings(storage, settings)).toBe(false);

    expect(storage.getItem(CONSOLE_PREFERENCES_STORAGE_KEY)).toBe(before);
    expect(readDeveloperSettings(storage)).toEqual(enabled);
  });

  it("accepts the cap and equal bounds", () => {
    const storage = new MapStorage();
    const atCap = {
      latencyEnabled: true,
      latencyMinMs: MAX_LATENCY_MS,
      latencyMaxMs: MAX_LATENCY_MS,
    };

    expect(writeDeveloperSettings(storage, atCap)).toBe(true);
    expect(readDeveloperSettings(storage)).toEqual(atCap);
  });

  it("falls back to the defaults for a stored out-of-range struct", () => {
    const storage = new MapStorage();
    storage.setItem(
      CONSOLE_PREFERENCES_STORAGE_KEY,
      encodeBytes(
        toBinary(
          ConsolePreferencesSchema,
          create(ConsolePreferencesSchema, {
            developerSettings: { latencyEnabled: true, latencyMinMs: 5_000, latencyMaxMs: 100 },
          }),
        ),
      ),
    );

    expect(readDeveloperSettings(storage)).toEqual(DEFAULT_DEVELOPER_SETTINGS);
  });

  it("notifies same-tab writes and other-tab storage events, and stops after unsubscribe", () => {
    const storage = new MapStorage();
    const eventTarget = new EventTarget();
    const subscriber = vi.fn();
    const unsubscribe = subscribeToDeveloperSettings(eventTarget, storage, subscriber);

    writeDeveloperSettings(storage, enabled);
    expect(subscriber).toHaveBeenLastCalledWith(enabled);

    const otherTab = new StorageEvent("storage", {
      key: CONSOLE_PREFERENCES_STORAGE_KEY,
      newValue: encodeBytes(
        toBinary(
          ConsolePreferencesSchema,
          create(ConsolePreferencesSchema, {
            developerSettings: { latencyEnabled: true, latencyMinMs: 10, latencyMaxMs: 20 },
          }),
        ),
      ),
    });
    eventTarget.dispatchEvent(otherTab);
    expect(subscriber).toHaveBeenLastCalledWith({
      latencyEnabled: true,
      latencyMinMs: 10,
      latencyMaxMs: 20,
    });
    expect(subscriber).toHaveBeenCalledTimes(2);

    unsubscribe();
    writeDeveloperSettings(storage, { ...enabled, latencyEnabled: false });
    expect(subscriber).toHaveBeenCalledTimes(2);
  });
});
