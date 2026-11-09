import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { type ReactNode, useState } from "react";
import { createMemoryRouter, RouterProvider, useLocation, useNavigate } from "react-router-dom";
import { describe, expect, it } from "vitest";

import { InspectorOpenerCard } from "@/components/ui/inspector-opener-card";
import { PanelNavigationProvider } from "@/components/ui/panel-navigation-provider";
import { PanelLink, usePanelDirtyGuard, usePanelNavigation } from "@/panels/panel-navigation";
import { usePanelPendingNavigation } from "@/panels/panel-navigation-pending";

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

describe("PanelNavigationProvider inspector presentation", () => {
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
});
