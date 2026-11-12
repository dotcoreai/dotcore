import { screen, within } from "@testing-library/react";
import { createMemoryRouter, RouterProvider } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { routes } from "@/app/routes";
import { renderWithClients } from "@/test/render-with-clients";
import { setupUser } from "@/test/user";
import { ThemeProvider } from "@/theme/theme-provider";

function renderConsole(initialEntry: string) {
  const router = createMemoryRouter(routes, { initialEntries: [initialEntry] });
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

function sidebarLink(name: string) {
  return screen.queryByRole("link", { name });
}

beforeEach(() => window.localStorage.clear());
afterEach(() => window.localStorage.clear());

describe("Get started entry points", () => {
  it("shows the sidebar item first by default and opens the page", async () => {
    const user = setupUser();
    const { router } = renderConsole("/overview");

    const links = screen.getAllByRole("link");
    expect(links[0]).toHaveTextContent("Get started");
    expect(links[0]).toHaveAttribute("href", "/get-started");

    await user.click(links[0]);

    expect(router.state.location.pathname).toBe("/get-started");
    expect(await screen.findByRole("heading", { name: "Get started" })).toBeVisible();
  });

  it("hides the item on Dismiss, goes to the overview and stays hidden after a reload", async () => {
    const user = setupUser();
    const first = renderConsole("/get-started");

    await user.click(await screen.findByRole("button", { name: "Dismiss" }));

    expect(first.router.state.location.pathname).toBe("/overview");
    expect(sidebarLink("Get started")).not.toBeInTheDocument();

    first.unmount();
    renderConsole("/overview");

    expect(await screen.findByRole("link", { name: "Overview" })).toBeVisible();
    expect(sidebarLink("Get started")).not.toBeInTheDocument();
  });

  it("shows the item again and opens the page from the profile menu", async () => {
    const user = setupUser();
    const { router } = renderConsole("/get-started");

    await user.click(await screen.findByRole("button", { name: "Dismiss" }));
    expect(sidebarLink("Get started")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Open profile menu" }));
    await user.click(
      within(screen.getByRole("menu")).getByRole("menuitem", { name: "Get started" }),
    );

    expect(router.state.location.pathname).toBe("/get-started");
    expect(await screen.findByRole("link", { name: "Get started" })).toHaveAttribute(
      "href",
      "/get-started",
    );
  });
});
