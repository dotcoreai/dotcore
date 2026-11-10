import { act, render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { useSidebarPreference } from "@/app/sidebar-preference-context";
import { SidebarPreferenceProvider } from "@/app/sidebar-preference-provider";
import { setupUser } from "@/test/user";

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

function SidebarControl() {
  const { collapsed, setCollapsed } = useSidebarPreference();

  return (
    <button onClick={() => setCollapsed(!collapsed)} type="button">
      {collapsed ? "Collapsed" : "Expanded"}
    </button>
  );
}

describe("SidebarPreferenceProvider", () => {
  it("renders expanded on a first visit with no stored preference", () => {
    const storage = new FakeStorage();

    render(
      <SidebarPreferenceProvider eventTarget={new EventTarget()} storage={storage}>
        <SidebarControl />
      </SidebarPreferenceProvider>,
    );

    expect(screen.getByRole("button", { name: "Expanded" })).toBeVisible();
  });

  it("restores a collapsed preference across a fresh mount, simulating a reload", async () => {
    const user = setupUser();
    const storage = new FakeStorage();

    const { unmount } = render(
      <SidebarPreferenceProvider eventTarget={new EventTarget()} storage={storage}>
        <SidebarControl />
      </SidebarPreferenceProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Expanded" }));
    expect(screen.getByRole("button", { name: "Collapsed" })).toBeVisible();
    unmount();

    // A fresh provider mount against the same storage, standing in for a page reload.
    render(
      <SidebarPreferenceProvider eventTarget={new EventTarget()} storage={storage}>
        <SidebarControl />
      </SidebarPreferenceProvider>,
    );

    expect(screen.getByRole("button", { name: "Collapsed" })).toBeVisible();
  });

  it("updates from a storage event dispatched by another tab", () => {
    const storage = new FakeStorage();
    const events = new EventTarget();

    render(
      <SidebarPreferenceProvider eventTarget={events} storage={storage}>
        <SidebarControl />
      </SidebarPreferenceProvider>,
    );

    expect(screen.getByRole("button", { name: "Expanded" })).toBeVisible();

    act(() => {
      events.dispatchEvent(
        new StorageEvent("storage", { key: "dotcore.console.preferences.v1", newValue: "EAE=" }),
      );
    });

    expect(screen.getByRole("button", { name: "Collapsed" })).toBeVisible();
  });

  it("fails descriptively outside its provider", () => {
    expect(() => render(<SidebarControl />)).toThrow(
      "useSidebarPreference must be used within a SidebarPreferenceProvider",
    );
  });
});
