import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { createMemoryRouter, RouterProvider } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ConsoleLayout } from "@/app/console-layout";
import { productionTiming, setTiming } from "@/lib/timing";
import { usePanelPendingNavigation } from "@/panels/panel-navigation-pending";
import { useFakeTimersWithWaits } from "@/test/fake-timers";
import { renderWithClients } from "@/test/render-with-clients";
import { setupUser } from "@/test/user";
import { ThemeProvider } from "@/theme/theme-provider";

class TestStorage {
  private readonly values = new Map<string, string>();

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

function setViewportWidth(width: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, value: width });
  window.dispatchEvent(new Event("resize"));
}

function renderNavigation(initialEntry = "/models", pending = false) {
  const router = createMemoryRouter(
    [
      {
        element: <ConsoleLayout eventTarget={new EventTarget()} storage={new TestStorage()} />,
        children: [
          { path: "/models", element: <NavigationPage title="Models page" pending={pending} /> },
          {
            path: "/models/catalog",
            element: <NavigationPage title="Catalog page" pending={pending} />,
          },
          {
            path: "/routes",
            element: <NavigationPage title="Routes page" pending={pending} />,
          },
          { path: "/apps", element: <NavigationPage title="Apps page" pending={pending} /> },
          {
            path: "/apps/catalog",
            element: <NavigationPage title="Apps catalog page" pending={pending} />,
          },
          { path: "/machines", element: <p>Machines page</p> },
        ],
      },
    ],
    { initialEntries: [initialEntry] },
  );

  renderWithClients(
    <ThemeProvider
      document={document}
      eventTarget={new EventTarget()}
      storage={window.localStorage}
    >
      <RouterProvider router={router} />
    </ThemeProvider>,
  );

  return router;
}

function NavigationPage({ title, pending }: { title: string; pending: boolean }) {
  usePanelPendingNavigation(pending, "Saving changes. Navigation is unavailable.");
  return <p>{title}</p>;
}

function isCollapsed() {
  return document.querySelector('[data-slot="sidebar"][data-state="collapsed"]') !== null;
}

function isPeeking() {
  return document.querySelector('[data-peek="true"]') !== null;
}

function advancePastPeekEnterDelay() {
  return act(() => vi.advanceTimersByTimeAsync(productionTiming.sidebarPeekEnterMs));
}

async function openMobileNavigation(user: UserEvent) {
  await user.click(screen.getByRole("button", { name: "Toggle Sidebar" }));
  return waitFor(() => {
    const sidebar = document.querySelector('[data-mobile="true"]');
    expect(sidebar).not.toBeNull();
    return sidebar as HTMLElement;
  });
}

describe("ConsoleSidebar primary destinations", () => {
  it("groups the Models and Catalog links under Models, followed by Routes", () => {
    setViewportWidth(1280);
    renderNavigation();

    const group = screen.getByRole("button", { name: "Models" });
    const groupRow = group.closest('[data-slot="sidebar-menu-item"]')!;
    const routesRow = screen
      .getByRole("link", { name: "Routes" })
      .closest('[data-slot="sidebar-menu-item"]')!;

    expect(group).toHaveAttribute("aria-expanded", "true");
    expect(groupRow.nextElementSibling).toBe(routesRow);
    expect(
      within(groupRow as HTMLElement)
        .getAllByRole("link")
        .map((link) => [link.textContent, link.getAttribute("href")]),
    ).toEqual([
      ["Models", "/models"],
      ["Catalog", "/models/catalog"],
    ]);
    expect(
      within(groupRow.parentElement as HTMLElement)
        .getAllByRole("link")
        .map((link) => link.getAttribute("href")),
    ).toEqual(["/get-started", "/overview", "/machines", "/models", "/models/catalog", "/routes"]);
  });

  it("groups the Apps and Catalog links under Apps, open on the Apps pages", () => {
    setViewportWidth(1280);
    renderNavigation("/apps/catalog");

    const group = screen.getByRole("button", { name: "Apps" });
    const groupRow = group.closest('[data-slot="sidebar-menu-item"]')!;

    expect(group).toHaveAttribute("aria-expanded", "true");
    expect(
      within(groupRow as HTMLElement)
        .getAllByRole("link")
        .map((link) => [link.textContent, link.getAttribute("href")]),
    ).toEqual([
      ["Apps", "/apps"],
      ["Catalog", "/apps/catalog"],
    ]);
  });

  it.each([
    ["/models", "Models"],
    ["/models?panel=models%2Fdeployment%3Adeployment-a", "Models"],
    ["/models/catalog", "Catalog"],
    ["/models/catalog?panel=model-catalog%2Fcatalog%3Agemma", "Catalog"],
    ["/routes", "Routes"],
    ["/routes?panel=routes%2Fcreate", "Routes"],
    ["/apps", "Apps"],
    ["/apps?panel=apps%2Fdeployment%3Adeployment-a", "Apps"],
    ["/apps/catalog", "Catalog"],
    ["/apps/catalog?panel=app-catalog%2Fcatalog%3Aopen-webui", "Catalog"],
  ])("selects only %s as the current destination", (location, active) => {
    setViewportWidth(1280);
    renderNavigation(location);

    const current = screen
      .getAllByRole("link")
      .filter((link) => link.getAttribute("aria-current") === "page");
    expect(current.map((link) => link.textContent)).toEqual([active]);
    expect(current[0]).toHaveAttribute("data-active");
    expect(
      screen
        .getAllByRole("link")
        .filter((link) => link.hasAttribute("data-active"))
        .map((link) => link.textContent),
    ).toEqual([active]);
  });
});

describe("ConsoleSidebar navigation icons", () => {
  it("shows each destination with its own icon before its label", () => {
    setViewportWidth(1280);
    renderNavigation("/models");

    for (const [label, icon] of [
      ["Routes", "lucide-route"],
      ["Models", "lucide-boxes"],
      ["Catalog", "lucide-library"],
    ]) {
      const link = screen.getByRole("link", { name: label });
      expect(link.querySelector("svg")).toHaveClass(icon);
      expect(link.firstElementChild).toBe(link.querySelector("svg"));
    }
  });
});

describe("ConsoleSidebar mobile navigation", () => {
  it.each([
    ["/models", "Routes", "/routes", "Routes page"],
    ["/models/catalog", "Models", "/models", "Models page"],
    ["/models", "Catalog", "/models/catalog", "Catalog page"],
  ])("closes after navigation from %s to %s", async (initialEntry, label, destination, page) => {
    setViewportWidth(390);
    const user = setupUser();
    const router = renderNavigation(initialEntry);
    const sidebar = await openMobileNavigation(user);

    await user.click(within(sidebar).getByRole("link", { name: label }));

    expect(await screen.findByText(page)).toBeInTheDocument();
    await waitFor(() => {
      expect(document.querySelector('[data-slot="sidebar-footer"]')).toBeNull();
    });
    expect(router.state.location.pathname).toBe(destination);
  });

  it.each(["Models", "Routes"])(
    "keeps the drawer open when %s navigation is pending",
    async (label) => {
      setViewportWidth(390);
      const user = setupUser();
      const initialEntry = "/models?panel=models%2Fdeployment%3Adeployment-a";
      const router = renderNavigation(initialEntry, true);
      const sidebar = await openMobileNavigation(user);

      await user.click(within(sidebar).getByRole("link", { name: label }));

      expect(router.state.location.pathname + router.state.location.search).toBe(initialEntry);
      expect(document.querySelector('[data-mobile="true"]')).toBe(sidebar);
    },
  );

  it.each(["Models", "Routes"])("keeps the drawer open for a modified %s click", async (label) => {
    setViewportWidth(390);
    const user = setupUser();
    const router = renderNavigation();
    const sidebar = await openMobileNavigation(user);

    fireEvent.click(within(sidebar).getByRole("link", { name: label }), { ctrlKey: true });

    expect(router.state.location.pathname).toBe("/models");
    expect(document.querySelector('[data-mobile="true"]')).toBe(sidebar);
  });

  it("closes after cross-branch navigation and does not reopen on browser Back", async () => {
    setViewportWidth(390);
    const user = setupUser();
    const router = renderNavigation();
    const sidebar = await openMobileNavigation(user);

    await user.click(within(sidebar).getByRole("link", { name: "Machines" }));

    expect(await screen.findByText("Machines page")).toBeInTheDocument();
    await waitFor(() => {
      expect(document.querySelector('[data-slot="sidebar-footer"]')).toBeNull();
    });

    await router.navigate(-1);

    expect(await screen.findByText("Models page")).toBeInTheDocument();
    expect(document.querySelector('[data-slot="sidebar-footer"]')).toBeNull();
  });

  it("keeps desktop navigation behavior unchanged", async () => {
    setViewportWidth(1024);
    const user = setupUser();
    const router = renderNavigation();

    expect(document.querySelector('[data-mobile="true"]')).toBeNull();
    await user.click(screen.getByRole("link", { name: "Machines" }));

    expect(await screen.findByText("Machines page")).toBeInTheDocument();
    expect(router.state.location.pathname).toBe("/machines");
    expect(document.querySelector('[data-slot="sidebar"][data-state="expanded"]')).not.toBeNull();
  });
});

describe("ConsoleSidebar collapse peek", () => {
  beforeEach(() => {
    setTiming(productionTiming);
    useFakeTimersWithWaits();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("does not peek after Collapse while the pointer stays over the sidebar", async () => {
    setViewportWidth(1280);
    const user = setupUser();
    renderNavigation();

    await user.hover(screen.getByRole("link", { name: "Machines" }));
    await advancePastPeekEnterDelay();
    await user.click(screen.getByRole("button", { name: "Collapse" }));
    await advancePastPeekEnterDelay();

    expect(isCollapsed()).toBe(true);
    expect(isPeeking()).toBe(false);
  });

  it("peeks again when the pointer leaves and re-enters the collapsed rail", async () => {
    setViewportWidth(1280);
    const user = setupUser();
    renderNavigation();

    await user.hover(screen.getByRole("link", { name: "Machines" }));
    await advancePastPeekEnterDelay();
    await user.click(screen.getByRole("button", { name: "Collapse" }));
    await user.unhover(screen.getByRole("button", { name: "Expand" }));
    await user.hover(screen.getByRole("link", { name: "Machines" }));
    await advancePastPeekEnterDelay();

    expect(isPeeking()).toBe(true);
    expect(isCollapsed()).toBe(true);
  });

  it("does not peek after keyboard Collapse until focus leaves and re-enters the rail", async () => {
    // jsdom has no input modality, so this test treats the focused element as keyboard-focused.
    vi.spyOn(HTMLElement.prototype, "matches").mockImplementation(function (
      this: HTMLElement,
      selector: string,
    ) {
      return selector === ":focus-visible"
        ? this === document.activeElement
        : Element.prototype.matches.call(this, selector);
    });
    setViewportWidth(1280);
    const user = setupUser();
    renderNavigation();

    act(() => screen.getByRole("button", { name: "Collapse" }).focus());
    await user.keyboard("{Enter}");
    await advancePastPeekEnterDelay();

    expect(isCollapsed()).toBe(true);
    expect(isPeeking()).toBe(false);

    act(() => screen.getByRole("button", { name: "Expand" }).blur());
    act(() => screen.getByRole("link", { name: "Machines" }).focus());

    expect(isPeeking()).toBe(true);
  });

  it("does not peek after the keyboard shortcut collapses a hovered sidebar", async () => {
    setViewportWidth(1280);
    const user = setupUser();
    renderNavigation();

    await user.hover(screen.getByRole("link", { name: "Machines" }));
    await advancePastPeekEnterDelay();
    await user.keyboard("{Control>}b{/Control}");
    await advancePastPeekEnterDelay();

    expect(isCollapsed()).toBe(true);
    expect(isPeeking()).toBe(false);
  });
});

describe("ConsoleSidebar chat item", () => {
  const panel = () => document.querySelector<HTMLElement>('section[aria-label="Chat"]');

  it("sits above Settings as a pressed-state button rather than a link", async () => {
    setViewportWidth(1280);
    const user = setupUser();
    renderNavigation();

    const footer = document.querySelector('[data-slot="sidebar-footer"]') as HTMLElement;
    const labels = within(footer)
      .getAllByRole("button")
      .map((button) => button.textContent);
    expect(labels.indexOf("Chat")).toBeGreaterThanOrEqual(0);
    expect(labels.indexOf("Chat")).toBe(labels.indexOf("Settings") - 1);
    expect(within(footer).queryByRole("link", { name: "Chat" })).toBeNull();

    const item = within(footer).getByRole("button", { name: "Chat" });
    expect(item).toHaveAttribute("aria-pressed", "false");
    await user.click(item);
    expect(item).toHaveAttribute("aria-pressed", "true");
    expect(panel()).toBeVisible();
    await user.click(item);
    expect(item).toHaveAttribute("aria-pressed", "false");
    expect(panel()).not.toBeVisible();
  });

  it("opens the panel and closes the drawer on mobile", async () => {
    setViewportWidth(390);
    const user = setupUser();
    renderNavigation();
    const sidebar = await openMobileNavigation(user);

    await user.click(within(sidebar).getByRole("button", { name: "Chat" }));

    expect(panel()).toBeVisible();
    await waitFor(() => {
      expect(document.querySelector('[data-mobile="true"]')).toBeNull();
    });
    setViewportWidth(1024);
  });
});
