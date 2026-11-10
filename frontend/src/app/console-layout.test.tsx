import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { type ReactNode, useRef, useState } from "react";
import { createMemoryRouter, RouterProvider } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { ConsoleLayout } from "@/app/console-layout";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { CONSOLE_PREFERENCES_STORAGE_KEY, writeSidebarCollapsed } from "@/lib/console-preferences";
import { productionTiming, setTiming, timing } from "@/lib/timing";
import { PanelLink } from "@/panels/panel-navigation";
import { usePanelPendingNavigation } from "@/panels/panel-navigation-pending";
import { renderWithClients } from "@/test/render-with-clients";
import { setupUser } from "@/test/user";
import { ThemeProvider } from "@/theme/theme-provider";

// Isolates each test's sidebar preference from real jsdom localStorage (and from each other),
// the same way ThemeProvider is given an explicit storage below.
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

// Matches the pattern in inspector-shell.test.tsx: useIsMobile reads window.innerWidth directly on
// mount, so setting it before render is enough to put the sidebar in its mobile (drawer) branch.
function setViewportWidth(width: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, value: width });
  window.dispatchEvent(new Event("resize"));
}

afterEach(() => {
  setViewportWidth(1024);
  vi.useRealTimers();
  vi.restoreAllMocks();
});

// jsdom does not reliably match :focus-visible on anchors. Keep the browser's retained keyboard
// focus indication on the actual link under test, including pointer presses on that link.
function focusSidebarLink(name: string) {
  const link = screen.getByRole("link", { name });
  vi.spyOn(link, "matches").mockImplementation((selector) =>
    selector === ":focus-visible"
      ? link === document.activeElement
      : Element.prototype.matches.call(link, selector),
  );
  act(() => link.focus());
  return { link };
}

function PendingRouteContent() {
  const [pending, setPending] = useState(true);
  usePanelPendingNavigation(pending, "Saving changes. Inspector navigation is unavailable.");
  return (
    <button type="button" onClick={() => setPending(false)}>
      Finish route operation
    </button>
  );
}

function FeaturePanel({ title, destination }: { title: string; destination: string }) {
  const openerRef = useRef<HTMLButtonElement>(null);

  return (
    <>
      <button ref={openerRef} type="button">
        {title} opener
      </button>
      <InspectorShell
        body={<PanelLink to={destination}>Continue inspector navigation</PanelLink>}
        id="feature-panel"
        onOpenChange={() => undefined}
        open
        openerRef={openerRef}
        title={title}
      />
    </>
  );
}

function ModelsFeaturePanel() {
  return (
    <FeaturePanel destination="/routes?panel=routes%2Froute%3Aroute-a" title="Model deployment" />
  );
}

function RoutesFeaturePanel() {
  return (
    <FeaturePanel
      destination="/routes?panel=routes%2Froute%3Aroute-a&panel=routes%2Fedit%3Aroute-a"
      title="Route details"
    />
  );
}

function renderFeaturePanelLayout(initialEntries: string[]) {
  const router = createMemoryRouter(
    [
      {
        element: <ConsoleLayout eventTarget={new EventTarget()} storage={new FakeStorage()} />,
        children: [
          { path: "/models", element: <ModelsFeaturePanel /> },
          { path: "/routes", element: <RoutesFeaturePanel /> },
          { path: "/machines", element: <p>Machines base content</p> },
        ],
      },
    ],
    { initialEntries },
  );
  const view = renderWithClients(
    <ThemeProvider
      document={document}
      eventTarget={new EventTarget()}
      storage={window.localStorage}
    >
      <RouterProvider router={router} />
    </ThemeProvider>,
  );
  return { router, ...view };
}

function renderLayout(
  initialEntry = "/models",
  storage: FakeStorage = new FakeStorage(),
  routeContent: ReactNode = <p>Route content</p>,
) {
  const router = createMemoryRouter(
    [
      {
        element: <ConsoleLayout eventTarget={new EventTarget()} storage={storage} />,
        children: [
          { path: "/", element: routeContent },
          { path: "/models", element: routeContent },
          { path: "/routes", element: routeContent },
          { path: "/models-archive", element: routeContent },
          { path: "/machines", element: routeContent },
        ],
      },
    ],
    { initialEntries: [initialEntry] },
  );

  const view = renderWithClients(
    <ThemeProvider
      document={document}
      eventTarget={new EventTarget()}
      storage={window.localStorage}
    >
      <RouterProvider router={router} />
    </ThemeProvider>,
  );

  return { router, storage, ...view };
}

describe("ConsoleLayout visual inspector session", () => {
  it("shows a cross-page destination immediately at rest without restoring the outgoing opener", async () => {
    const user = setupUser();
    const { router } = renderFeaturePanelLayout([
      "/models?panel=models%2Fdeployment%3Adeployment-a",
    ]);
    const outgoingPanel = screen.getByRole("complementary", { name: "Model deployment" });
    const outgoingOpener = screen.getByRole("button", { name: "Model deployment opener" });

    await user.click(screen.getByRole("link", { name: "Continue inspector navigation" }));

    const destination = await screen.findByRole("complementary", { name: "Route details" });
    expect(destination).not.toBe(outgoingPanel);
    expect(destination).toHaveTextContent("Continue inspector navigation");
    expect(destination.className).not.toMatch(/animate-(in|out)|fade|zoom|slide|translate/);
    expect(outgoingOpener).not.toHaveFocus();
    expect(router.state.location.pathname).toBe("/routes");
  });

  it("clears a pathname-changing panel navigation immediately without retaining an exiting inspector", async () => {
    const user = setupUser();
    const { router } = renderFeaturePanelLayout([
      "/models?panel=models%2Fdeployment%3Adeployment-a",
    ]);

    await user.click(screen.getByRole("link", { name: "Machines" }));

    expect(router.state.location.pathname).toBe("/machines");
    expect(screen.getByText("Machines base content")).toBeInTheDocument();
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
  });
});

describe("ConsoleLayout", () => {
  it.each([
    ["Models", "/models"],
    ["Routes", "/routes"],
    ["Machines", "/machines"],
  ])("locks %s navigation while a panel operation is pending", async (label, destination) => {
    const user = setupUser();
    const initialEntry = "/models?panel=models%2Fdeployment%3Adeployment-a";
    const { router } = renderLayout(initialEntry, new FakeStorage(), <PendingRouteContent />);

    await user.click(screen.getByRole("link", { name: label }));
    expect(router.state.location.pathname + router.state.location.search).toBe(initialEntry);

    await user.click(screen.getByRole("button", { name: "Finish route operation" }));
    await user.click(screen.getByRole("link", { name: label }));
    expect(router.state.location.pathname + router.state.location.search).toBe(destination);
  });

  it("renders the header and sidebar around the routed content", () => {
    renderLayout();

    const banner = screen.getByRole("banner");
    expect(banner).toHaveTextContent("dotcore");
    expect(within(banner).getByRole("button", { name: "Toggle Sidebar" })).toHaveClass("md:hidden");
    expect(within(banner).getByRole("button", { name: "Open profile menu" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Collapse" })).toBeInTheDocument();

    expect(screen.getByRole("link", { name: "Machines" })).toHaveAttribute("href", "/machines");
    expect(screen.getByRole("button", { name: "Apps" })).toHaveAttribute("aria-expanded", "false");

    expect(screen.getByRole("button", { name: "Models" })).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByRole("link", { name: "Models" })).toHaveAttribute("href", "/models");
    expect(screen.getByRole("link", { name: "Catalog" })).toHaveAttribute(
      "href",
      "/models/catalog",
    );
    expect(screen.getByRole("link", { name: "Routes" })).toHaveAttribute("href", "/routes");
    expect(screen.getByRole("link", { name: "Overview" })).toHaveAttribute("href", "/overview");

    const settings = screen.getByRole("button", { name: "Settings" });
    expect(settings).toHaveAttribute("aria-disabled", "true");
    expect(settings).toHaveAttribute("tabindex", "-1");

    expect(screen.getAllByRole("main")).toHaveLength(1);
    expect(screen.getByRole("main")).toHaveTextContent("Route content");
  });

  it("renders the logo mark to the left of the dotcore wordmark", () => {
    renderLayout();

    const banner = screen.getByRole("banner");
    const mark = banner.querySelector('[data-slot="console-mark"]');
    expect(mark).not.toBeNull();

    const wordmark = within(banner).getByText("dotcore");
    // DOCUMENT_POSITION_FOLLOWING (4): the wordmark comes after the mark in document order, which
    // renders it to the mark's right in this left-to-right layout.
    expect(mark!.compareDocumentPosition(wordmark)).toBe(Node.DOCUMENT_POSITION_FOLLOWING);
  });

  it.each([false, true])(
    "keeps primary links in order and disabled sections out of the tab order (collapsed: %s)",
    async (collapsed) => {
      const user = setupUser();
      const storage = new FakeStorage();
      writeSidebarCollapsed(storage, collapsed);
      renderLayout("/models", storage);

      await user.tab();
      expect(document.activeElement).toHaveAccessibleName("Toggle Sidebar");
      await user.tab();
      expect(document.activeElement).toHaveAccessibleName("Open profile menu");

      for (const path of ["/overview", "/machines"]) {
        await user.tab();
        expect(document.activeElement).toHaveAttribute("href", path);
      }
      await user.tab();
      expect(document.activeElement).toHaveAccessibleName("Models");
      expect(document.activeElement).toHaveAttribute("aria-expanded", "true");

      // Keyboard focus peeks the collapsed rail open, so the open group's children stay reachable.
      for (const path of ["/models", "/models/catalog", "/routes"]) {
        await user.tab();
        expect(document.activeElement).toHaveAttribute("href", path);
        expect(document.activeElement).not.toHaveAttribute("aria-expanded");
      }
      await user.tab();
      expect(document.activeElement).toHaveAccessibleName("Apps");
      expect(document.activeElement).toHaveAttribute("aria-expanded", "false");

      await user.tab();
      expect(document.activeElement).toHaveAccessibleName("Chat");

      await user.tab();
      expect(document.activeElement).toHaveAccessibleName(collapsed ? "Expand" : "Collapse");
    },
  );

  it("survives navigation instead of remounting the sidebar provider", async () => {
    const user = setupUser();
    const router = createMemoryRouter(
      [
        {
          element: <ConsoleLayout eventTarget={new EventTarget()} storage={new FakeStorage()} />,
          children: [
            { path: "/models", element: <p>Models content</p> },
            { path: "/machines", element: <p>Machines content</p> },
          ],
        },
      ],
      { initialEntries: ["/models"] },
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

    const footerToggle = screen.getByRole("button", { name: "Collapse" });
    await user.click(footerToggle);
    const collapsedSidebar = document.querySelector(
      '[data-slot="sidebar"][data-state="collapsed"]',
    );
    expect(collapsedSidebar).not.toBeNull();

    await user.click(screen.getByRole("link", { name: "Machines" }));

    expect(await screen.findByText("Machines content")).toBeInTheDocument();
    expect(document.querySelector('[data-slot="sidebar"][data-state="collapsed"]')).not.toBeNull();
  });

  it("drives the sidebar from the stored preference and writes back through the inversion", async () => {
    const user = setupUser();
    const storage = new FakeStorage();
    const router = createMemoryRouter(
      [
        {
          element: <ConsoleLayout eventTarget={new EventTarget()} storage={storage} />,
          children: [{ path: "/", element: <p>Route content</p> }],
        },
      ],
      { initialEntries: ["/"] },
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

    // Nothing stored yet: open (expanded), the collapsed field's proto3 zero value.
    expect(document.querySelector('[data-slot="sidebar"][data-state="expanded"]')).not.toBeNull();

    const footerToggle = screen.getByRole("button", { name: "Collapse" });
    await user.click(footerToggle);

    expect(document.querySelector('[data-slot="sidebar"][data-state="collapsed"]')).not.toBeNull();
    expect(storage.values.size).toBe(1);
  });

  it("flips the footer toggle's label and icon with the sidebar state on desktop", async () => {
    setViewportWidth(1024);
    const user = setupUser();
    renderLayout();

    const collapseToggle = screen.getByRole("button", { name: "Collapse" });
    expect(collapseToggle.querySelector("svg")).toHaveClass("lucide-panel-left-close");

    await user.click(collapseToggle);

    const expandToggle = screen.getByRole("button", { name: "Expand" });
    expect(expandToggle.querySelector("svg")).toHaveClass("lucide-panel-left-open");
  });

  it("labels the footer toggle as a close affordance inside the mobile drawer", async () => {
    setViewportWidth(390);
    const user = setupUser();
    renderLayout();

    await user.click(screen.getByRole("button", { name: "Toggle Sidebar" }));

    // Scoped to the footer, not `screen`: the drawer also renders the Sheet's own dismiss button,
    // sr-only labeled "Close" too. It's hidden by CSS in a real browser ([&>button]:hidden), but
    // jsdom doesn't compute that, so an unscoped query would match both.
    const footer = await waitFor(() => {
      const element = document.querySelector('[data-slot="sidebar-footer"]');
      expect(element).not.toBeNull();
      return element as HTMLElement;
    });
    const closeToggle = within(footer).getByRole("button", { name: "Close" });
    expect(closeToggle.querySelector("svg")).toHaveClass("lucide-panel-left-close");

    await user.click(closeToggle);

    await waitFor(() => {
      expect(document.querySelector('[data-slot="sidebar-footer"]')).toBeNull();
    });
  });
});

describe("ConsoleLayout app shell (R8.A)", () => {
  // jsdom implements no layout, so `scrollHeight` and `clientHeight` are always 0 regardless of
  // markup — asserting they're equal here would pass on any change and prove nothing. This checks
  // only the classes the shell's scroll containment depends on (the content pane scrolls, the
  // document doesn't); real scroll behavior is verified in the browser, not jsdom.
  it("marks the shell root and content pane with the classes the scroll containment depends on", () => {
    renderLayout();

    const shellRoot = document.querySelector('[data-slot="sidebar-wrapper"]');
    expect(shellRoot).toHaveClass("h-full");

    // The row holding the sidebar and the content pane: a direct child of the shell root, and the
    // only one that's a <div> (the header is a <header>).
    const row = shellRoot!.querySelector(":scope > div");
    expect(row).toHaveClass("min-h-0");

    const inset = screen.getByRole("main");
    expect(inset).toHaveAttribute("data-slot", "sidebar-inset");
    expect(inset).toHaveClass("min-h-0", "overflow-y-auto");
  });
});

// jsdom implements no layout or CSS engine, so none of these tests can prove the peek visually
// widens the rail or that `sidebar-inset` holds still — that's the browser task's job. What's
// testable here, and what actually breaks first if the peek is wired through the wrong state, is
// the state and attribute contract: which element gets `data-peek`, whether the stored preference
// ever sees a write, and whether the classes the reveal depends on are present on the DOM.
describe("ConsoleLayout hover peek (R5)", () => {
  it("marks the rail data-peek after a sustained hover and clears it after leaving, touching neither data-collapsible nor the stored preference (R5.A, R5.B, R5.D)", async () => {
    setTiming(productionTiming);
    vi.useFakeTimers();
    const { storage } = renderLayout("/machines");

    fireEvent.click(screen.getByRole("button", { name: "Collapse" }));
    const sidebar = document.querySelector('[data-slot="sidebar"]')!;
    expect(sidebar).toHaveAttribute("data-collapsible", "icon");
    expect(sidebar).not.toHaveAttribute("data-peek");

    const storedAfterCollapse = storage.values.get(CONSOLE_PREFERENCES_STORAGE_KEY);
    expect(storage.values.size).toBe(1);

    const container = document.querySelector('[data-slot="sidebar-container"]')!;
    fireEvent.pointerEnter(container);

    // Still within the enter delay: no peek yet.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(productionTiming.sidebarPeekEnterMs - 1);
    });
    expect(sidebar).not.toHaveAttribute("data-peek");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(sidebar).toHaveAttribute("data-peek", "true");
    // The gap-driving attribute is untouched by the peek — this is the state-level half of R5.D;
    // the browser task confirms the geometry this is supposed to guarantee.
    expect(sidebar).toHaveAttribute("data-collapsible", "icon");
    expect(storage.values.get(CONSOLE_PREFERENCES_STORAGE_KEY)).toBe(storedAfterCollapse);
    expect(storage.values.size).toBe(1);

    fireEvent.pointerLeave(container);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(productionTiming.sidebarPeekLeaveMs - 1);
    });
    expect(sidebar).toHaveAttribute("data-peek", "true");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(sidebar).not.toHaveAttribute("data-peek");
    expect(storage.values.get(CONSOLE_PREFERENCES_STORAGE_KEY)).toBe(storedAfterCollapse);
    expect(storage.values.size).toBe(1);
  });

  it("does not peek on a brief graze that leaves before the enter delay elapses", async () => {
    setTiming(productionTiming);
    vi.useFakeTimers();
    renderLayout("/machines");
    fireEvent.click(screen.getByRole("button", { name: "Collapse" }));
    const sidebar = document.querySelector('[data-slot="sidebar"]')!;
    const container = document.querySelector('[data-slot="sidebar-container"]')!;

    fireEvent.pointerEnter(container);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(80);
    });
    fireEvent.pointerLeave(container);

    // The leave handler must cancel the pending enter timer, not just race it: advancing well past
    // the enter delay after leaving must never flip data-peek on.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(
        productionTiming.sidebarPeekEnterMs + productionTiming.sidebarPeekLeaveMs,
      );
    });
    expect(sidebar).not.toHaveAttribute("data-peek");
  });

  it("stays inert while the sidebar is expanded by preference, even under a sustained hover", async () => {
    vi.useFakeTimers();
    renderLayout("/machines");
    const sidebar = document.querySelector('[data-slot="sidebar"]')!;
    expect(sidebar).toHaveAttribute("data-state", "expanded");

    const container = document.querySelector('[data-slot="sidebar-container"]')!;
    fireEvent.pointerEnter(container);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(timing().sidebarPeekEnterMs);
    });

    expect(sidebar).not.toHaveAttribute("data-peek");
  });

  it("widens the collapsed rail immediately when keyboard focus enters it without changing the preference", () => {
    const storage = new FakeStorage();
    writeSidebarCollapsed(storage, true);
    renderLayout("/machines", storage);
    const storedAfterCollapse = storage.values.get(CONSOLE_PREFERENCES_STORAGE_KEY);

    focusSidebarLink("Routes");

    const sidebar = document.querySelector('[data-slot="sidebar"]')!;
    expect(sidebar).toHaveAttribute("data-peek", "true");
    expect(sidebar).toHaveAttribute("data-collapsible", "icon");
    expect(storage.values.get(CONSOLE_PREFERENCES_STORAGE_KEY)).toBe(storedAfterCollapse);
    expect(storage.values.size).toBe(1);
  });

  it("collapses immediately when focus leaves the sidebar entirely", () => {
    const storage = new FakeStorage();
    writeSidebarCollapsed(storage, true);
    renderLayout("/machines", storage);
    focusSidebarLink("Routes");
    const sidebar = document.querySelector('[data-slot="sidebar"]')!;
    expect(sidebar).toHaveAttribute("data-peek", "true");

    act(() => screen.getByRole("button", { name: "Open profile menu" }).focus());

    expect(sidebar).not.toHaveAttribute("data-peek");
  });

  it("keeps the peek open when keyboard focus moves between peer links", () => {
    const storage = new FakeStorage();
    writeSidebarCollapsed(storage, true);
    renderLayout("/models", storage);
    const sidebar = document.querySelector('[data-slot="sidebar"]')!;

    focusSidebarLink("Models");
    expect(sidebar).toHaveAttribute("data-peek", "true");
    focusSidebarLink("Routes");

    expect(document.activeElement).toBe(screen.getByRole("link", { name: "Routes" }));
    expect(sidebar).toHaveAttribute("data-peek", "true");
  });

  it("does not leave the peek stuck open after a click: clicking a row focuses it without triggering :focus-visible, so the peek closes once the pointer leaves (regression)", async () => {
    const user = setupUser();
    const storage = new FakeStorage();
    writeSidebarCollapsed(storage, true);
    renderLayout("/models", storage);

    const sidebar = document.querySelector('[data-slot="sidebar"]')!;
    const container = document.querySelector('[data-slot="sidebar-container"]')!;
    fireEvent.pointerEnter(container);
    await waitFor(() => expect(sidebar).toHaveAttribute("data-peek", "true"));

    // A click focuses its target exactly as Tab does, but that focus never blurs on its own once
    // the pointer leaves — this is the bug: without the `:focus-visible` gate, focusWithin latches
    // true forever and the peek never closes again.
    const routesLink = screen.getByRole("link", { name: "Routes" });
    await user.click(routesLink);
    expect(document.activeElement).toBe(routesLink);
    expect(routesLink.matches(":focus-visible")).toBe(false);

    fireEvent.pointerLeave(container);
    await waitFor(() => expect(sidebar).not.toHaveAttribute("data-peek"));
  });

  it("releases keyboard peek on focused-link click despite retained focus-visible, then reopens on Tab", async () => {
    vi.useFakeTimers();
    const storage = new FakeStorage();
    writeSidebarCollapsed(storage, true);
    const { router } = renderLayout("/models", storage);
    const sidebar = document.querySelector('[data-slot="sidebar"]')!;
    const container = document.querySelector('[data-slot="sidebar-container"]')!;
    const storedAfterCollapse = storage.values.get(CONSOLE_PREFERENCES_STORAGE_KEY);
    const { link } = focusSidebarLink("Routes");
    expect(sidebar).toHaveAttribute("data-peek", "true");

    fireEvent.pointerEnter(container);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(timing().sidebarPeekEnterMs);
    });
    fireEvent.pointerDown(link);
    fireEvent.click(link);
    expect(link.matches(":focus-visible")).toBe(true);
    expect(router.state.location.pathname).toBe("/routes");
    expect(document.activeElement).toBe(link);
    expect(sidebar).toHaveAttribute("data-peek", "true");

    fireEvent.pointerLeave(container);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(timing().sidebarPeekLeaveMs);
    });
    expect(sidebar).not.toHaveAttribute("data-peek");
    expect(document.activeElement).toBe(link);
    expect(sidebar).toHaveAttribute("data-collapsible", "icon");
    expect(storage.values.get(CONSOLE_PREFERENCES_STORAGE_KEY)).toBe(storedAfterCollapse);

    fireEvent.keyDown(link, { key: "Tab" });
    const { link: machines } = focusSidebarLink("Machines");
    expect(document.activeElement).toBe(machines);
    expect(sidebar).toHaveAttribute("data-peek", "true");
  });

  it("does not reacquire keyboard peek from pointer-generated focus in the same gesture", () => {
    const storage = new FakeStorage();
    writeSidebarCollapsed(storage, true);
    renderLayout("/models", storage);
    const { link } = focusSidebarLink("Routes");
    act(() => screen.getByRole("button", { name: "Open profile menu" }).focus());
    const sidebar = document.querySelector('[data-slot="sidebar"]')!;
    fireEvent.pointerDown(link);
    act(() => link.focus());
    fireEvent.click(link);
    expect(link.matches(":focus-visible")).toBe(true);
    expect(document.activeElement).toBe(link);
    expect(sidebar).not.toHaveAttribute("data-peek");
  });
});
