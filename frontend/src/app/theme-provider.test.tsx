import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { useTheme } from "@/app/theme-context";
import { ThemeProvider } from "@/app/theme-provider";
import { ColorScheme } from "@/gen/dotcore/console/v1/preferences_pb";
import { writeColorScheme } from "@/lib/console-preferences";

class FakeStorage {
  values = new Map<string, string>();

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

function ThemeControl() {
  const { colorScheme, setColorScheme } = useTheme();

  return (
    <button onClick={() => setColorScheme(ColorScheme.DARK)} type="button">
      {colorScheme === ColorScheme.DARK ? "Dark" : "Light"}
    </button>
  );
}

describe("ThemeProvider", () => {
  it("applies runtime changes to document state and persistence", async () => {
    const user = userEvent.setup();
    const storage = new FakeStorage();
    writeColorScheme(storage, ColorScheme.LIGHT);
    const documentElement = document.documentElement;
    documentElement.classList.remove("dark");
    documentElement.style.colorScheme = "";

    render(
      <ThemeProvider document={document} eventTarget={new EventTarget()} storage={storage}>
        <ThemeControl />
      </ThemeProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Light" }));

    expect(screen.getByRole("button", { name: "Dark" })).toBeVisible();
    expect(documentElement).toHaveClass("dark");
    expect(documentElement.style.colorScheme).toBe("dark");
    expect(storage.values.size).toBe(1);
  });

  it("defaults to Dark when storage has no preference", () => {
    const storage = new FakeStorage();
    const documentElement = document.documentElement;
    documentElement.classList.remove("dark");
    documentElement.style.colorScheme = "";

    render(
      <ThemeProvider document={document} eventTarget={new EventTarget()} storage={storage}>
        <ThemeControl />
      </ThemeProvider>,
    );

    expect(screen.getByRole("button", { name: "Dark" })).toBeVisible();
    expect(documentElement).toHaveClass("dark");
    expect(documentElement.style.colorScheme).toBe("dark");
  });

  it("updates from same-origin storage events and removes the listener on teardown", () => {
    const storage = new FakeStorage();
    const events = new EventTarget();
    const removeEventListener = vi.spyOn(events, "removeEventListener");
    const { unmount } = render(
      <ThemeProvider document={document} eventTarget={events} storage={storage}>
        <ThemeControl />
      </ThemeProvider>,
    );

    act(() => {
      events.dispatchEvent(
        new StorageEvent("storage", { key: "dotcore.console.preferences.v1", newValue: "CAI=" }),
      );
    });

    expect(screen.getByRole("button", { name: "Dark" })).toBeVisible();

    unmount();
    expect(removeEventListener).toHaveBeenCalledWith("storage", expect.any(Function));
  });

  it("retains the selected theme when persistence fails", async () => {
    const user = userEvent.setup();
    const storage = new FakeStorage();
    writeColorScheme(storage, ColorScheme.LIGHT);
    vi.spyOn(storage, "setItem").mockImplementation(() => {
      throw new Error("Storage disabled");
    });

    render(
      <ThemeProvider document={document} eventTarget={new EventTarget()} storage={storage}>
        <ThemeControl />
      </ThemeProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Light" }));

    expect(screen.getByRole("button", { name: "Dark" })).toBeVisible();
    expect(document.documentElement).toHaveClass("dark");
  });

  it("fails descriptively outside its provider", () => {
    expect(() => render(<ThemeControl />)).toThrow("useTheme must be used within a ThemeProvider");
  });
});
