import { create } from "@bufbuild/protobuf";
import { act, screen, within } from "@testing-library/react";
import { createMemoryRouter, RouterProvider } from "react-router-dom";
import { describe, expect, it } from "vitest";

import { routes } from "@/app/routes";
import {
  ListConfiguredExecutionTargetsResponseSchema,
  UpdateConsoleSettingsRequestSchema,
} from "@/gen/dotcore/site/v1/service_pb";
import { FakeConsoleSettings } from "@/test/fake-console-settings";
import { renderWithClients } from "@/test/render-with-clients";
import { setupUser } from "@/test/user";
import { ThemeProvider } from "@/theme/theme-provider";

const hiddenRequest = (hidden: boolean) =>
  create(UpdateConsoleSettingsRequestSchema, {
    settings: { getStartedHidden: hidden },
    updateMask: { paths: ["get_started_hidden"] },
  });

function renderConsole(initialEntry: string, settings: FakeConsoleSettings) {
  const router = createMemoryRouter(routes, { initialEntries: [initialEntry] });
  const view = renderWithClients(
    <ThemeProvider
      document={document}
      eventTarget={new EventTarget()}
      storage={window.localStorage}
    >
      <RouterProvider router={router} />
    </ThemeProvider>,
    {
      resourceWatchClient: settings.resourceWatchClient(),
      siteClient: {
        listConfiguredExecutionTargets: () =>
          Promise.resolve(create(ListConfiguredExecutionTargetsResponseSchema)),
        updateConsoleSettings: settings.updateConsoleSettings,
      },
    },
  );

  return { router, ...view };
}

function sidebarLink(name: string) {
  return screen.queryByRole("link", { name });
}

async function openGetStartedFromProfileMenu(user: ReturnType<typeof setupUser>) {
  await user.click(screen.getByRole("button", { name: "Open profile menu" }));
  await user.click(within(screen.getByRole("menu")).getByRole("menuitem", { name: "Get started" }));
}

describe("Get started entry points", () => {
  it("shows the sidebar item first while the site shows it and opens the page", async () => {
    const user = setupUser();
    const settings = new FakeConsoleSettings(false);
    const { router } = renderConsole("/overview", settings);

    await screen.findByRole("link", { name: "Get started" });
    const links = screen.getAllByRole("link");
    expect(links[0]).toHaveTextContent("Get started");
    expect(links[0]).toHaveAttribute("href", "/get-started");

    await user.click(links[0]);

    expect(router.state.location.pathname).toBe("/get-started");
    expect(await screen.findByRole("heading", { name: "Get started" })).toBeVisible();
    expect(settings.updates).toEqual([]);
  });

  it("leaves the item out of a site that hides it, with no write", async () => {
    const settings = new FakeConsoleSettings(true);
    renderConsole("/overview", settings);

    expect(await screen.findByRole("link", { name: "Overview" })).toBeVisible();
    await act(() => settings.watch.tick());

    expect(sidebarLink("Get started")).not.toBeInTheDocument();
    expect(settings.updates).toEqual([]);
  });

  it("hides the item for the whole site on Dismiss and goes to the overview", async () => {
    const user = setupUser();
    const settings = new FakeConsoleSettings(false);
    const first = renderConsole("/get-started", settings);
    await screen.findByRole("link", { name: "Get started" });

    await user.click(await screen.findByRole("button", { name: "Dismiss" }));

    expect(settings.updates).toEqual([hiddenRequest(true)]);
    expect(first.router.state.location.pathname).toBe("/overview");
    expect(sidebarLink("Get started")).not.toBeInTheDocument();

    // The watch confirms it on its next tick, with no flicker back.
    await act(() => settings.watch.tick());
    expect(sidebarLink("Get started")).not.toBeInTheDocument();

    // Another browser tab loading the console agrees.
    first.unmount();
    renderConsole("/overview", settings);
    expect(await screen.findByRole("link", { name: "Overview" })).toBeVisible();
    expect(sidebarLink("Get started")).not.toBeInTheDocument();
  });

  it("clears the setting and opens the page from the profile menu", async () => {
    const user = setupUser();
    const settings = new FakeConsoleSettings(true);
    const { router } = renderConsole("/overview", settings);
    await screen.findByRole("link", { name: "Overview" });
    expect(sidebarLink("Get started")).not.toBeInTheDocument();

    await openGetStartedFromProfileMenu(user);

    expect(settings.updates).toEqual([hiddenRequest(false)]);
    expect(await screen.findByRole("heading", { name: "Get started" })).toBeVisible();
    expect(router.state.location.pathname).toBe("/get-started");
    expect(await screen.findByRole("link", { name: "Get started" })).toHaveAttribute(
      "href",
      "/get-started",
    );
  });

  it("stays on the page with the reason when Dismiss cannot be saved", async () => {
    const user = setupUser();
    const settings = new FakeConsoleSettings(false);
    const { router } = renderConsole("/get-started", settings);
    await screen.findByRole("link", { name: "Get started" });
    settings.failing = true;

    await user.click(await screen.findByRole("button", { name: "Dismiss" }));

    expect(
      await screen.findByText(/Saving the Get started setting outcome is uncertain/),
    ).toBeVisible();
    expect(router.state.location.pathname).toBe("/get-started");
    expect(sidebarLink("Get started")).toBeVisible();
    expect(settings.updates).toEqual([hiddenRequest(true)]);
  });

  it("opens the page with the reason when the profile menu cannot clear the setting", async () => {
    const user = setupUser();
    const settings = new FakeConsoleSettings(true);
    const { router } = renderConsole("/overview", settings);
    await screen.findByRole("link", { name: "Overview" });
    settings.failing = true;

    await openGetStartedFromProfileMenu(user);

    expect(
      await screen.findByText(/Saving the Get started setting outcome is uncertain/),
    ).toBeVisible();
    expect(router.state.location.pathname).toBe("/get-started");
    expect(sidebarLink("Get started")).not.toBeInTheDocument();
  });
});
