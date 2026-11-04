import { act, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createMemoryRouter, RouterProvider } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  MachinesSectionIcon,
  ModelsSectionIcon,
  RoutesSectionIcon,
} from "@/app/console-section-icons";
import { ConsoleSidebar, PEEK_ENTER_DELAY_MS } from "@/app/console-sidebar";
import { useSidebarPreference } from "@/app/sidebar-preference-context";
import { SidebarPreferenceProvider } from "@/app/sidebar-preference-provider";
import { SidebarProvider } from "@/components/ui/sidebar";
import { CONSOLE_PREFERENCES_STORAGE_KEY, writeSidebarCollapsed } from "@/lib/console-preferences";

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("ConsoleSidebar optional groups", () => {
  it("marks the models child active on /models, with the parent plain because the branch is open", () => {
    renderGroups("/models");

    const trigger = screen.getByRole("button", { name: "Workspace" });
    expect(trigger).not.toHaveAttribute("href");
    expect(trigger).toHaveAttribute("aria-expanded", "true");
    // The open branch lets the active child carry the highlight alone; the parent renders plain so
    // exactly one row in the branch is highlighted at a time.
    expect(trigger).not.toHaveAttribute("data-active");

    const modelsLink = screen.getByRole("link", { name: "Models" });
    expect(modelsLink).toHaveAttribute("href", "/models");
    expect(modelsLink).toHaveAttribute("data-active");

    const routesLink = screen.getByRole("link", { name: "Routes" });
    expect(routesLink).toHaveAttribute("href", "/routes");
    expect(routesLink).not.toHaveAttribute("data-active");
  });

  it("marks the Routes child active on /routes, with the parent plain because the branch is open", () => {
    renderGroups("/routes");

    const trigger = screen.getByRole("button", { name: "Workspace" });
    expect(trigger).toHaveAttribute("aria-expanded", "true");
    expect(trigger).not.toHaveAttribute("data-active");

    const routesLink = screen.getByRole("link", { name: "Routes" });
    expect(routesLink).toHaveAttribute("href", "/routes");
    expect(routesLink).toHaveAttribute("data-active");

    const modelsLink = screen.getByRole("link", { name: "Models" });
    expect(modelsLink).toHaveAttribute("href", "/models");
    expect(modelsLink).not.toHaveAttribute("data-active");
  });

  it("highlights the parent once its open branch is closed by hand, on both branch locations", async () => {
    const user = userEvent.setup();

    for (const location of ["/models", "/routes"]) {
      const { unmount } = renderGroups(location);

      const trigger = screen.getByRole("button", { name: "Workspace" });
      expect(trigger).toHaveAttribute("aria-expanded", "true");
      expect(trigger).not.toHaveAttribute("data-active");

      await user.click(trigger);

      expect(trigger).toHaveAttribute("aria-expanded", "false");
      expect(trigger).toHaveAttribute("data-active");

      unmount();
    }
  });

  it("highlights the parent in the collapsed icon rail even though its branch is open", async () => {
    const user = userEvent.setup();
    renderGroups("/models");

    const trigger = screen.getByRole("button", { name: "Workspace" });
    expect(trigger).toHaveAttribute("aria-expanded", "true");
    expect(trigger).not.toHaveAttribute("data-active");

    // Collapsing the whole sidebar to the icon rail hides children regardless of the branch's own
    // open state, so the parent must pick up the highlight there or the rail shows no indication of
    // which section is active.
    await user.click(screen.getByRole("button", { name: "Collapse" }));

    expect(trigger).toHaveAttribute("data-active");
  });

  it("collapses the navigation group on a fresh load at /machines, unlike arriving from inside it", () => {
    renderGroups("/machines");

    const trigger = screen.getByRole("button", { name: "Workspace" });
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(trigger).not.toHaveAttribute("data-active");
    expect(screen.queryByRole("link", { name: "Routes" })).not.toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "Models" })).not.toBeInTheDocument();
  });

  it("leaves the navigation group expanded when navigating to /machines from inside it, unlike a fresh load there", async () => {
    const user = userEvent.setup();
    renderGroups("/models");

    expect(screen.getByRole("button", { name: "Workspace" })).toHaveAttribute(
      "aria-expanded",
      "true",
    );

    await user.click(screen.getByRole("link", { name: "Machines" }));

    // A branch is opened by entering it and by the user, and closed only by the user — never by
    // leaving it. So the same "/machines" location renders collapsed on a fresh load (previous
    // test) but expanded when reached this way. That asymmetry is intended, not a bug.
    expect(screen.getByRole("button", { name: "Workspace" })).toHaveAttribute(
      "aria-expanded",
      "true",
    );
    expect(screen.getByRole("link", { name: "Routes" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Models" })).toBeInTheDocument();
  });

  it("does not read /models-archive as inside the navigation group", () => {
    // Segment-aware matching (R3.A): a naive startsWith("/models") would wrongly treat this as
    // inside the branch, since "/models-archive" shares that string prefix without a "/" boundary.
    renderGroups("/models-archive");

    const trigger = screen.getByRole("button", { name: "Workspace" });
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(trigger).not.toHaveAttribute("data-active");
    expect(screen.queryByRole("link", { name: "Routes" })).not.toBeInTheDocument();
  });

  it("toggles the navigation group by click without navigating", async () => {
    const user = userEvent.setup();
    const { router } = renderGroups("/machines");

    const trigger = screen.getByRole("button", { name: "Workspace" });
    expect(trigger).toHaveAttribute("aria-expanded", "false");

    await user.click(trigger);
    expect(trigger).toHaveAttribute("aria-expanded", "true");
    // /machines is outside the navigation group, so the parent stays plain whether the branch the user
    // just opened by hand is open or closed.
    expect(trigger).not.toHaveAttribute("data-active");
    expect(screen.getByRole("link", { name: "Routes" })).toBeInTheDocument();
    expect(router.state.location.pathname).toBe("/machines");

    await user.click(trigger);
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(trigger).not.toHaveAttribute("data-active");
    expect(screen.queryByRole("link", { name: "Routes" })).not.toBeInTheDocument();
    expect(router.state.location.pathname).toBe("/machines");
  });

  it("toggles the navigation group with both Enter and Space", async () => {
    const user = userEvent.setup();
    renderGroups("/machines");

    const trigger = screen.getByRole("button", { name: "Workspace" });
    trigger.focus();
    expect(trigger).toHaveAttribute("aria-expanded", "false");

    await user.keyboard("{Enter}");
    expect(trigger).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByRole("link", { name: "Routes" })).toBeInTheDocument();

    await user.keyboard(" ");
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("link", { name: "Routes" })).not.toBeInTheDocument();
  });

  it("reopens a manually closed group when navigation enters a child", async () => {
    const user = userEvent.setup();
    const { router } = renderGroups("/models");
    const trigger = screen.getByRole("button", { name: "Workspace" });

    await user.click(trigger);
    expect(trigger).toHaveAttribute("aria-expanded", "false");

    await act(() => router.navigate("/routes"));

    expect(trigger).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByRole("link", { name: "Routes" })).toHaveAttribute("data-active");
  });

  it("keeps a manual collapse of the navigation group across an unrelated re-render", async () => {
    const user = userEvent.setup();
    renderGroups("/models");

    const trigger = screen.getByRole("button", { name: "Workspace" });
    expect(trigger).toHaveAttribute("aria-expanded", "true");

    await user.click(trigger);
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("link", { name: "Routes" })).not.toBeInTheDocument();

    // Collapsing the whole sidebar to icon mode re-renders every nav row without navigating; the
    // manual collapse must survive it.
    await user.click(screen.getByRole("button", { name: "Collapse" }));

    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("link", { name: "Routes" })).not.toBeInTheDocument();
  });

  it("wires the chevron's rotation to the trigger's own named group, not a same-element attribute", async () => {
    const user = userEvent.setup();
    renderGroups("/machines");

    const trigger = screen.getByRole("button", { name: "Workspace" });
    const chevron = trigger.querySelector("svg.lucide-chevron-right");
    expect(chevron).not.toBeNull();

    // `data-panel-open` lands on the trigger button itself, not on this descendant svg, so the
    // rotation class must target the button's own named group rather than repeat the attribute on
    // the chevron, where it can never match.
    expect(chevron).toHaveClass("group-data-panel-open/menu-button:rotate-90");
    expect(trigger).toHaveClass("group/menu-button");
    expect(trigger).not.toHaveAttribute("data-panel-open");

    await user.click(trigger);

    expect(trigger).toHaveAttribute("data-panel-open");
  });

  it("keeps the group row a direct <li> child of the nav menu, not wrapped in an extra element", () => {
    renderGroups("/machines");

    const trigger = screen.getByRole("button", { name: "Workspace" });
    const menuItem = trigger.closest("li");
    expect(menuItem?.tagName).toBe("LI");

    // `Collapsible.Root` renders a `<div>` by default; without `render={<SidebarMenuItem />}` that
    // div sits between the `<ul>` and this row's `<li>`, breaking the list content model and
    // pulling the row out of its siblings' `<li>` sequence.
    const menu = menuItem?.parentElement;
    expect(menu?.tagName).toBe("UL");
    expect(Array.from(menu?.children ?? []).every((child) => child.tagName === "LI")).toBe(true);
  });
});

describe("ConsoleSidebar optional group peek", () => {
  it("keeps an open branch's children hidden on the collapsed rail until peeked (R5.E) — an open branch alone must not paint past the 48px rail", () => {
    renderGroups("/models");
    fireEvent.click(screen.getByRole("button", { name: "Collapse" }));

    const sidebar = document.querySelector('[data-slot="sidebar"]')!;
    expect(sidebar).toHaveAttribute("data-collapsible", "icon");
    // Never peeked, so the reveal is proven to depend on peek, not merely on the branch being open.
    expect(sidebar).not.toHaveAttribute("data-peek");

    const trigger = screen.getByRole("button", { name: "Workspace" });
    expect(trigger).toHaveAttribute("aria-expanded", "true");

    const subList = document.querySelector('[data-slot="sidebar-menu-sub"]')!;
    // Mounted children remain hidden in icon mode; their CSS reveal depends on the sidebar's peek.
    // The panel's own open state must not reveal content beyond the rail.
    expect(subList.closest("[data-open]")).not.toBeNull();
    expect(subList).toHaveClass("group-data-peek:flex!");
    expect(subList.className).not.toContain("in-data-open");

    const routesLink = screen.getByRole("link", { name: "Routes" });
    expect(routesLink).toBeInTheDocument();
    expect(routesLink).toHaveClass("group-data-peek:flex!");
    expect(routesLink.className).not.toContain("in-data-open");
  });

  it("reveals an open branch's children once the rail is actually peeked, by hover (R5.C, R5.E)", async () => {
    vi.useFakeTimers();
    const { storage } = renderGroups("/models");
    fireEvent.click(screen.getByRole("button", { name: "Collapse" }));
    const storedAfterCollapse = storage.values.get(CONSOLE_PREFERENCES_STORAGE_KEY);

    const sidebar = document.querySelector('[data-slot="sidebar"]')!;
    const container = document.querySelector('[data-slot="sidebar-container"]')!;
    fireEvent.pointerEnter(container);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(PEEK_ENTER_DELAY_MS);
    });

    expect(sidebar).toHaveAttribute("data-peek", "true");
    expect(screen.getByRole("link", { name: "Routes" })).toBeInTheDocument();
    // Peeking still never touches the stored preference, even with a branch open underneath it.
    expect(storage.values.get(CONSOLE_PREFERENCES_STORAGE_KEY)).toBe(storedAfterCollapse);
  });

  it("reveals an open branch's children once the rail is actually peeked, by keyboard focus (R5.C, R5.E)", () => {
    // Pre-seeds the collapsed preference rather than clicking the footer's Collapse toggle: a real
    // click establishes pointer modality for the whole document, which would make jsdom's
    // `:focus-visible` report false for every focus in this test, not just the click's own target.
    const storage = new FakeStorage();
    writeSidebarCollapsed(storage, true);
    renderGroups("/models", storage);
    resetKeyboardModality();

    const sidebar = document.querySelector('[data-slot="sidebar"]')!;
    act(() => {
      screen.getByRole("button", { name: "Workspace" }).focus();
    });

    expect(sidebar).toHaveAttribute("data-peek", "true");
    expect(screen.getByRole("link", { name: "Routes" })).toBeInTheDocument();
  });
});

function renderGroups(initialEntry = "/models", storage = new FakeStorage()) {
  const router = createMemoryRouter(
    [
      {
        path: "*",
        element: (
          <SidebarPreferenceProvider eventTarget={new EventTarget()} storage={storage}>
            <GroupSidebar />
          </SidebarPreferenceProvider>
        ),
      },
    ],
    { initialEntries: [initialEntry] },
  );
  const view = render(<RouterProvider router={router} />);
  return { router, storage, ...view };
}

function GroupSidebar() {
  const { collapsed, setCollapsed } = useSidebarPreference();
  return (
    <SidebarProvider open={!collapsed} onOpenChange={(open) => setCollapsed(!open)}>
      <ConsoleSidebar
        items={[
          { icon: MachinesSectionIcon, label: "Machines", path: "/machines" },
          {
            icon: ModelsSectionIcon,
            label: "Workspace",
            children: [
              { icon: RoutesSectionIcon, label: "Routes", path: "/routes" },
              { icon: ModelsSectionIcon, label: "Models", path: "/models" },
            ],
          },
        ]}
      />
    </SidebarProvider>
  );
}

function resetKeyboardModality() {
  fireEvent.keyDown(document.body, { key: "Tab" });
}

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
