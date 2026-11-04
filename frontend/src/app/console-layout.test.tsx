import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { type ReactNode, useRef, useState } from "react";
import { createMemoryRouter, RouterProvider, useLocation, useNavigate } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  ConsoleLayout,
  PanelLink,
  PanelNavigationProvider,
  usePanelDirtyGuard,
  usePanelNavigation,
} from "@/app/console-layout";
import { PEEK_ENTER_DELAY_MS, PEEK_LEAVE_DELAY_MS } from "@/app/console-sidebar";
import { usePanelPendingNavigation } from "@/app/panel-navigation-pending";
import { ThemeProvider } from "@/app/theme-provider";
import { InspectorOpenerCard } from "@/components/ui/inspector-opener-card";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { CONSOLE_PREFERENCES_STORAGE_KEY, writeSidebarCollapsed } from "@/lib/console-preferences";

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

function NavigationHarness() {
  const navigation = usePanelNavigation();
  return (
    <button
      onClick={() =>
        navigation.navigatePanel({
          pathname: "/models",
          search: "?panel=models%2Fdeployment%3Adeployment-a",
        })
      }
      type="button"
    >
      Open panel
    </button>
  );
}

type Presentation = "continue" | "enter" | "idle" | "restore";

function PresentationHarness() {
  // `presentation` is the narrow behavior seam route-owned inspector shells will consume. It is
  // intentionally cast while the provider has not implemented that contract, so this red test
  // remains type-checkable without adding production-only test machinery.
  const navigation = usePanelNavigation() as ReturnType<typeof usePanelNavigation> & {
    presentation?: Presentation;
  };
  const rootA = { pathname: "/models", search: "?panel=models%2Fdeployment%3Adeployment-a" };
  const rootB = { pathname: "/models", search: "?panel=models%2Fdeployment%3Adeployment-b" };
  const nested = {
    pathname: "/models",
    search: "?panel=models%2Fdeployment%3Adeployment-a&panel=models%2Fedit%3Adeployment-a",
  };
  const routesLocation = { pathname: "/routes", search: "?panel=routes%2Froute%3Aroute-a" };

  return (
    <>
      <output aria-label="Inspector presentation">
        {navigation.presentation ?? "unclassified"}
      </output>
      <button type="button" onClick={() => navigation.navigatePanel(rootA)}>
        Enter root
      </button>
      <button type="button" onClick={() => navigation.navigatePanel(rootB, { replace: true })}>
        Replace root
      </button>
      <button type="button" onClick={() => navigation.navigatePanel(nested)}>
        Push nested
      </button>
      <button type="button" onClick={() => navigation.navigatePanel(routesLocation)}>
        Cross page
      </button>
      <button type="button" onClick={navigation.back}>
        Back
      </button>
      <button type="button" onClick={() => navigation.close()}>
        Close
      </button>
    </>
  );
}

function PendingNavigationHarness() {
  const navigation = usePanelNavigation();
  const [pending, setPending] = useState(true);
  usePanelPendingNavigation(pending, "Saving changes. Inspector navigation is unavailable.");

  const destination = {
    pathname: "/routes",
    search: "?panel=routes%2Froute%3Aroute-b",
  };

  return (
    <>
      <button type="button" onClick={navigation.back}>
        Back
      </button>
      <button type="button" onClick={() => navigation.close()}>
        Close
      </button>
      <button type="button" onClick={() => navigation.navigatePanel(destination)}>
        Navigate panel
      </button>
      <PanelLink to={`${destination.pathname}${destination.search}`}>Panel link</PanelLink>
      <InspectorOpenerCard
        label="Lateral opener"
        onSelect={() => navigation.navigatePanel(destination)}
        panelId="pending-panel"
        selected={false}
      >
        Lateral
      </InspectorOpenerCard>
      <button type="button" onClick={() => setPending(false)}>
        Finish operation
      </button>
    </>
  );
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

function DirtyHistoryHarness() {
  const navigation = usePanelNavigation();
  const location = useLocation();
  const navigate = useNavigate();
  const [draft, setDraft] = useState("Saved name");
  usePanelDirtyGuard(draft !== "Saved name", () => setDraft("Saved name"));

  return (
    <>
      <button
        type="button"
        onClick={() =>
          navigation.navigatePanel({
            pathname: "/routes",
            search: "?tab=traffic&panel=routes%2Froute%3Aroute-a",
          })
        }
      >
        Open detail
      </button>
      <button
        type="button"
        onClick={() =>
          navigation.navigatePanel({
            pathname: "/routes",
            search: "?tab=traffic&panel=routes%2Froute%3Aroute-a&panel=routes%2Fedit%3Aroute-a",
          })
        }
      >
        Edit route
      </button>
      <label>
        Name
        <input value={draft} onChange={(event) => setDraft(event.target.value)} />
      </label>
      <button
        type="button"
        onClick={() =>
          void navigate(
            { pathname: location.pathname, search: location.search.replace("traffic", "latency") },
            { replace: true, state: location.state as unknown },
          )
        }
      >
        Change view
      </button>
      <button type="button" onClick={navigation.back}>
        Back
      </button>
      <button type="button" onClick={() => navigation.close()}>
        Close
      </button>
    </>
  );
}

function renderPanelNavigationProvider(
  initialEntry = "/models",
  pending = false,
  content: ReactNode = <NavigationHarness />,
) {
  const router = createMemoryRouter(
    [
      {
        path: "*",
        element: (
          <PanelNavigationProvider>
            {pending ? <PendingNavigationHarness /> : content}
          </PanelNavigationProvider>
        ),
      },
    ],
    { initialEntries: [initialEntry] },
  );
  const view = render(<RouterProvider router={router} />);
  return { router, ...view };
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
  const view = render(
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

  const view = render(
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

describe("PanelNavigationProvider", () => {
  it("lets a view-only change through while a panel is dirty", async () => {
    const user = userEvent.setup();
    const { router } = renderPanelNavigationProvider(
      "/routes?tab=traffic",
      false,
      <DirtyHistoryHarness />,
    );
    await user.click(screen.getByRole("button", { name: "Open detail" }));
    await user.type(screen.getByRole("textbox", { name: "Name" }), " changed");

    await user.click(screen.getByRole("button", { name: "Change view" }));

    expect(screen.queryByRole("dialog", { name: "Discard changes?" })).not.toBeInTheDocument();
    expect(router.state.location.pathname + router.state.location.search).toBe(
      "/routes?tab=latency&panel=routes%2Froute%3Aroute-a",
    );
    expect(router.state.location.state).toMatchObject({ __dotcorePanel: { step: 1 } });
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("Saved name changed");

    // A panel change still asks first.
    await user.click(screen.getByRole("button", { name: "Edit route" }));
    expect(await screen.findByRole("dialog", { name: "Discard changes?" })).toBeVisible();
  });

  it("keeps the current view parameters through a panel Back", async () => {
    const user = userEvent.setup();
    const { router } = renderPanelNavigationProvider(
      "/routes?tab=traffic",
      false,
      <DirtyHistoryHarness />,
    );
    await user.click(screen.getByRole("button", { name: "Open detail" }));
    await user.click(screen.getByRole("button", { name: "Edit route" }));
    await user.click(screen.getByRole("button", { name: "Change view" }));

    await user.click(screen.getByRole("button", { name: "Back" }));

    await waitFor(() =>
      expect(router.state.location.pathname + router.state.location.search).toBe(
        "/routes?tab=latency&panel=routes%2Froute%3Aroute-a",
      ),
    );
    expect(router.state.location.state).toMatchObject({ __dotcorePanel: { step: 1 } });
  });

  it.each(["Keep editing", "Escape"] as const)(
    "abandons cancelled session Close via %s without replacing later Back/Forward entries",
    async (cancellation) => {
      const user = userEvent.setup();
      const { router } = renderPanelNavigationProvider(
        "/routes?tab=traffic",
        false,
        <DirtyHistoryHarness />,
      );
      await user.click(screen.getByRole("button", { name: "Open detail" }));
      const detail = router.state.location;
      await user.click(screen.getByRole("button", { name: "Edit route" }));
      const edit = router.state.location;
      expect(detail.state).toMatchObject({ __dotcorePanel: { step: 1 } });
      expect(edit.state).toMatchObject({ __dotcorePanel: { step: 2 } });
      await user.clear(screen.getByRole("textbox", { name: "Name" }));
      await user.type(screen.getByRole("textbox", { name: "Name" }), "Unsaved route name");

      await user.click(screen.getByRole("button", { name: "Close" }));
      await screen.findByRole("dialog", { name: "Discard changes?" });
      if (cancellation === "Escape") await user.keyboard("{Escape}");
      else await user.click(screen.getByRole("button", { name: cancellation }));
      expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("Unsaved route name");
      expect(router.state.location).toEqual(edit);

      await user.click(screen.getByRole("button", { name: "Back" }));
      await user.click(await screen.findByRole("button", { name: "Discard changes" }));
      await waitFor(() => expect(router.state.location).toEqual(detail));
      expect(router.state.location.pathname + router.state.location.search).toBe(
        "/routes?tab=traffic&panel=routes%2Froute%3Aroute-a",
      );
      await act(async () => {
        await router.navigate(1);
      });
      expect(router.state.location).toEqual(edit);
      expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("Saved name");
    },
  );

  it("keeps confirmed session Close after repeated cancellation and isolates an unmounted blocker", async () => {
    const user = userEvent.setup();
    const first = renderPanelNavigationProvider(
      "/routes?tab=traffic",
      false,
      <DirtyHistoryHarness />,
    );
    await user.click(screen.getByRole("button", { name: "Open detail" }));
    await user.click(screen.getByRole("button", { name: "Edit route" }));
    await user.type(screen.getByRole("textbox", { name: "Name" }), " changed");
    for (const cancellation of ["Keep editing", "Escape"] as const) {
      await user.click(screen.getByRole("button", { name: "Close" }));
      await screen.findByRole("dialog", { name: "Discard changes?" });
      if (cancellation === "Escape") await user.keyboard("{Escape}");
      else await user.click(screen.getByRole("button", { name: cancellation }));
      expect(screen.getByRole("textbox", { name: "Name" })).toHaveValue("Saved name changed");
    }
    await user.click(screen.getByRole("button", { name: "Close" }));
    await user.click(await screen.findByRole("button", { name: "Discard changes" }));
    await waitFor(() =>
      expect(first.router.state.location.pathname + first.router.state.location.search).toBe(
        "/routes?tab=traffic",
      ),
    );
    await user.click(screen.getByRole("button", { name: "Open detail" }));
    await user.type(screen.getByRole("textbox", { name: "Name" }), " changed");
    await user.click(screen.getByRole("button", { name: "Close" }));
    await screen.findByRole("dialog", { name: "Discard changes?" });
    first.unmount();

    const second = renderPanelNavigationProvider(
      "/routes?tab=traffic",
      false,
      <DirtyHistoryHarness />,
    );
    await user.click(screen.getByRole("button", { name: "Open detail" }));
    expect(second.router.state.location.pathname + second.router.state.location.search).toBe(
      "/routes?tab=traffic&panel=routes%2Froute%3Aroute-a",
    );
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("locks Back, Close, panel navigation, PanelLink, and lateral openers while pending", async () => {
    const user = userEvent.setup();
    const initialEntry =
      "/models?panel=models%2Fdeployment%3Adeployment-a&panel=models%2Fedit%3Adeployment-a";
    const { router } = renderPanelNavigationProvider(initialEntry, true);

    for (const name of ["Back", "Close", "Navigate panel", "Panel link", "Lateral opener"]) {
      await user.click(screen.getByRole(name === "Panel link" ? "link" : "button", { name }));
      expect(router.state.location.pathname + router.state.location.search).toBe(initialEntry);
    }

    await user.click(screen.getByRole("button", { name: "Finish operation" }));
    await user.click(screen.getByRole("button", { name: "Navigate panel" }));
    expect(router.state.location.pathname + router.state.location.search).toBe(
      "/routes?panel=routes%2Froute%3Aroute-b",
    );
  });

  it("creates isolated panel history when crypto.randomUUID is unavailable", async () => {
    const user = userEvent.setup();
    const cryptoDescriptor = Object.getOwnPropertyDescriptor(globalThis, "crypto");
    const cryptoWithoutRandomUuid = {
      getRandomValues: globalThis.crypto.getRandomValues.bind(globalThis.crypto),
    };
    Object.defineProperty(globalThis, "crypto", {
      configurable: true,
      value: cryptoWithoutRandomUuid,
    });

    try {
      const first = renderPanelNavigationProvider();
      await user.click(screen.getByRole("button", { name: "Open panel" }));
      const firstSession = (
        first.router.state.location.state as {
          __dotcorePanel: { sessionId: string; step: number };
        }
      ).__dotcorePanel;
      expect(firstSession.step).toBe(1);
      expect(firstSession.sessionId).toMatch(/^panel-[0-9a-f]{32}$/);
      first.unmount();

      const second = renderPanelNavigationProvider();
      await user.click(screen.getByRole("button", { name: "Open panel" }));
      const secondSession = (
        second.router.state.location.state as {
          __dotcorePanel: { sessionId: string; step: number };
        }
      ).__dotcorePanel;
      expect(secondSession.step).toBe(1);
      expect(secondSession.sessionId).toMatch(/^panel-[0-9a-f]{32}$/);
      expect(secondSession.sessionId).not.toBe(firstSession.sessionId);
    } finally {
      if (cryptoDescriptor) Object.defineProperty(globalThis, "crypto", cryptoDescriptor);
    }
  });
});

describe("ConsoleLayout visual inspector session", () => {
  it("classifies an initial non-empty deep link as restore", () => {
    renderPanelNavigationProvider(
      "/models?panel=models%2Fdeployment%3Adeployment-a",
      false,
      <PresentationHarness />,
    );

    expect(screen.getByRole("status", { name: "Inspector presentation" })).toHaveTextContent(
      "restore",
    );
  });

  it("classifies base open, root replacement, nested push/pop, and first-entry Back", async () => {
    const user = userEvent.setup();
    renderPanelNavigationProvider("/models", false, <PresentationHarness />);
    const presentation = screen.getByRole("status", { name: "Inspector presentation" });

    expect(presentation).toHaveTextContent("idle");
    await user.click(screen.getByRole("button", { name: "Enter root" }));
    expect(presentation).toHaveTextContent("enter");
    await user.click(screen.getByRole("button", { name: "Replace root" }));
    expect(presentation).toHaveTextContent("continue");
    await user.click(screen.getByRole("button", { name: "Push nested" }));
    expect(presentation).toHaveTextContent("continue");
    await user.click(screen.getByRole("button", { name: "Back" }));
    expect(presentation).toHaveTextContent("continue");
    await user.click(screen.getByRole("button", { name: "Back" }));
    expect(presentation).toHaveTextContent("idle");
  });

  it("classifies an explicit same-page Close from a root panel as idle", async () => {
    const user = userEvent.setup();
    renderPanelNavigationProvider(
      "/models?panel=models%2Fdeployment%3Adeployment-a",
      false,
      <PresentationHarness />,
    );

    await user.click(screen.getByRole("button", { name: "Close" }));

    expect(screen.getByRole("status", { name: "Inspector presentation" })).toHaveTextContent(
      "idle",
    );
  });

  it("classifies cross-page and non-empty browser history navigation as continue", async () => {
    const user = userEvent.setup();
    const { router } = renderPanelNavigationProvider(
      "/models?panel=models%2Fdeployment%3Adeployment-a",
      false,
      <PresentationHarness />,
    );
    const presentation = screen.getByRole("status", { name: "Inspector presentation" });

    await user.click(screen.getByRole("button", { name: "Cross page" }));
    expect(presentation).toHaveTextContent("continue");
    await act(async () => {
      await router.navigate(-1);
    });
    expect(presentation).toHaveTextContent("continue");
    await act(async () => {
      await router.navigate(1);
    });
    expect(presentation).toHaveTextContent("continue");
  });

  it("shows a cross-page destination immediately at rest without restoring the outgoing opener", async () => {
    const user = userEvent.setup();
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
    const user = userEvent.setup();
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
    const user = userEvent.setup();
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
      const user = userEvent.setup();
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
    const user = userEvent.setup();
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

    render(
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
    const user = userEvent.setup();
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

    render(
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
    const user = userEvent.setup();
    renderLayout();

    const collapseToggle = screen.getByRole("button", { name: "Collapse" });
    expect(collapseToggle.querySelector("svg")).toHaveClass("lucide-panel-left-close");

    await user.click(collapseToggle);

    const expandToggle = screen.getByRole("button", { name: "Expand" });
    expect(expandToggle.querySelector("svg")).toHaveClass("lucide-panel-left-open");
  });

  it("labels the footer toggle as a close affordance inside the mobile drawer", async () => {
    setViewportWidth(390);
    const user = userEvent.setup();
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
      await vi.advanceTimersByTimeAsync(PEEK_ENTER_DELAY_MS - 1);
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
      await vi.advanceTimersByTimeAsync(PEEK_LEAVE_DELAY_MS - 1);
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
      await vi.advanceTimersByTimeAsync(PEEK_ENTER_DELAY_MS + PEEK_LEAVE_DELAY_MS);
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
      await vi.advanceTimersByTimeAsync(PEEK_ENTER_DELAY_MS);
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
    const user = userEvent.setup();
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
      await vi.advanceTimersByTimeAsync(PEEK_ENTER_DELAY_MS);
    });
    fireEvent.pointerDown(link);
    fireEvent.click(link);
    expect(link.matches(":focus-visible")).toBe(true);
    expect(router.state.location.pathname).toBe("/routes");
    expect(document.activeElement).toBe(link);
    expect(sidebar).toHaveAttribute("data-peek", "true");

    fireEvent.pointerLeave(container);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(PEEK_LEAVE_DELAY_MS);
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
