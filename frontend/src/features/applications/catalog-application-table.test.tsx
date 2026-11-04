import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import {
  ApplicationLifecycleState,
  ApplicationReadinessState,
} from "@/gen/dotcore/application/v1/state_pb";
import {
  type Application,
  ApplicationSchema,
  type ApplicationService,
  ApplicationServiceSchema,
} from "@/gen/dotcore/catalog/v1/application_pb";

import {
  clientFor,
  currentLocation,
  deployment,
  renderAppsSection,
} from "./applications-section-test-support";

const routeBinding = {
  bindingName: "route",
  input: { kind: { case: "inferenceRoute" as const, value: {} } },
};
const storageBinding = { bindingName: "data", mountPath: "/data" };

// Hermes and Open WebUI take a route; Hermes and Notebook use storage. Notebook's services share
// only linux/amd64. Only Hermes has a description.
const hermesDescription = "Autonomous agent with a web dashboard.";
const hermes = create(ApplicationSchema, {
  ...application("hermes-app", "Hermes", "0.1.0", [
    service(["amd64", "arm64"], {
      configurationBindings: [routeBinding],
      storageBindings: [storageBinding],
    }),
  ]),
  description: hermesDescription,
});
const openWebUi = application("open-webui", "Open WebUI", "0.6.5", [
  service(["amd64", "arm64"], { configurationBindings: [routeBinding] }),
]);
const notebook = application("notebook", "Notebook", "2.0", [
  service(["amd64", "arm64"], { storageBindings: [storageBinding] }),
  service(["amd64"]),
]);

function application(
  applicationId: string,
  displayName: string,
  version: string,
  services: ApplicationService[],
): Application {
  return create(ApplicationSchema, { applicationId, displayName, graph: { services }, version });
}

function service(
  architectures: string[],
  bindings: Pick<
    MessageInitShape<typeof ApplicationServiceSchema>,
    "configurationBindings" | "storageBindings"
  > = {},
): ApplicationService {
  return create(ApplicationServiceSchema, {
    artifact: {
      supportedPlatforms: architectures.map((architecture) => ({
        architecture,
        operatingSystem: "linux",
      })),
    },
    serviceName: "web",
    ...bindings,
  });
}

function renderCatalog(search = "", deployments: ReturnType<typeof deployment>[] = []) {
  const user = userEvent.setup();
  const client = clientFor([hermes, openWebUi, notebook], deployments);
  const rendered = renderAppsSection({ client, initialEntry: `/apps/catalog${search}` });
  return { ...rendered, client, user };
}

function table() {
  return screen.getByRole("table", { name: "Application catalog" });
}

function headers(): string[] {
  return within(table())
    .getAllByRole("columnheader")
    .map((header) => header.textContent);
}

function bodyRows(): string[] {
  return within(table())
    .getAllByRole("row")
    .slice(1)
    .map((row) => row.getAttribute("aria-label") ?? row.textContent);
}

function cells(name: string): string[] {
  return within(screen.getByRole("row", { name }))
    .getAllByRole("cell")
    .map((cell) => cell.textContent);
}

function footer() {
  return screen.getByText(/ of \d+ applications?$/);
}

function menuValues(menu: HTMLElement): string[] {
  return within(menu)
    .getAllByRole("menuitemcheckbox")
    .map((item) => item.textContent);
}

async function loaded() {
  await screen.findByRole("table", { name: "Application catalog" });
}

describe("AppCatalogPage table", () => {
  it("shows the default view: Application and Version by name, without Reset", async () => {
    const { router } = renderCatalog("", [
      deployment(hermes, ApplicationLifecycleState.READY, ApplicationReadinessState.READY),
    ]);
    await loaded();

    expect(headers()).toEqual(["Application", "Version"]);
    expect(
      screen.getByRole("button", { name: "Application column options" }).closest("th"),
    ).toHaveAttribute("aria-sort", "ascending");
    expect(bodyRows()).toEqual(["Hermes", "Notebook", "Open WebUI"]);
    expect(cells("Hermes")).toEqual([`Hermes1 deployed${hermesDescription}`, "0.1.0"]);
    expect(cells("Notebook")).toEqual(["Notebook", "2.0"]);
    expect(cells("Open WebUI")).toEqual(["Open WebUI", "0.6.5"]);
    expect(footer()).toHaveTextContent("3 of 3 applications");
    expect(screen.queryByRole("button", { name: /^Reset/ })).not.toBeInTheDocument();
    expect(currentLocation(router)).toBe("/apps/catalog");
  });

  it("searches application names and ids", async () => {
    const { router, user } = renderCatalog();
    await loaded();

    await user.type(screen.getByRole("searchbox", { name: "Filter applications" }), "WEBUI");
    expect(bodyRows()).toEqual(["Open WebUI"]);
    expect(footer()).toHaveTextContent("1 of 3 applications");
    expect(currentLocation(router)).toBe("/apps/catalog?q=WEBUI");

    await user.clear(screen.getByRole("searchbox", { name: "Filter applications" }));
    await user.type(screen.getByRole("searchbox", { name: "Filter applications" }), "hermes-app");
    expect(bodyRows()).toEqual(["Hermes"]);
    expect(screen.getByRole("button", { name: /^Reset/ })).toBeVisible();
  });

  it("filters on uses that must all hold, with counts", async () => {
    const { router, user } = renderCatalog();
    await loaded();

    await user.click(screen.getByRole("button", { name: "Filter" }));
    await user.click(await screen.findByRole("menuitem", { name: "Uses" }));
    const menu = await screen.findByRole("menu");
    expect(menuValues(menu)).toEqual(["Inference route2", "Persistent storage2"]);
    expect(currentLocation(router)).toBe("/apps/catalog?f=uses");

    await user.click(within(menu).getByRole("menuitemcheckbox", { name: /Inference route/ }));
    expect(currentLocation(router)).toBe("/apps/catalog?f=uses&uses=inference-route");
    expect(bodyRows()).toEqual(["Hermes", "Open WebUI"]);
    // Uses add up, so storage now counts only the applications that also take a route.
    expect(menuValues(menu)).toEqual(["Inference route2", "Persistent storage1"]);

    await user.click(within(menu).getByRole("menuitemcheckbox", { name: /Persistent storage/ }));
    expect(bodyRows()).toEqual(["Hermes"]);
    await user.keyboard("{Escape}");
    expect(
      screen.getByRole("button", {
        name: "Uses filter: Inference route, Persistent storage",
      }),
    ).toBeVisible();
    expect(footer()).toHaveTextContent("1 of 3 applications");
  });

  it("filters on platforms that are alternatives, with counts", async () => {
    const { router, user } = renderCatalog();
    await loaded();

    await user.click(screen.getByRole("button", { name: "Filter" }));
    await user.click(await screen.findByRole("menuitem", { name: "Platform" }));
    const menu = await screen.findByRole("menu");
    expect(menuValues(menu)).toEqual(["linux/amd643", "linux/arm642"]);

    await user.click(within(menu).getByRole("menuitemcheckbox", { name: /linux\/arm64/ }));
    expect(currentLocation(router)).toBe("/apps/catalog?f=platform&platform=linux%2Farm64");
    expect(bodyRows()).toEqual(["Hermes", "Open WebUI"]);
    expect(menuValues(menu)).toEqual(["linux/amd643", "linux/arm642"]);

    await user.click(within(menu).getByRole("menuitemcheckbox", { name: /linux\/amd64/ }));
    expect(bodyRows()).toEqual(["Hermes", "Notebook", "Open WebUI"]);
  });

  it("sorts on Services both ways from its header menu", async () => {
    const { router, user } = renderCatalog("?cols=application,services");
    await loaded();

    expect(headers()).toEqual(["Application", "Services"]);
    await user.click(screen.getByRole("button", { name: "Services column options" }));
    expect(
      within(await screen.findByRole("menu"))
        .getAllByRole("menuitem")
        .map((item) => item.textContent),
    ).toEqual(["Low → high", "High → low", "Hide column"]);
    await user.click(screen.getByRole("menuitem", { name: "Low → high" }));
    expect(currentLocation(router)).toBe("/apps/catalog?sort=services&cols=application%2Cservices");
    // Ties keep name order.
    expect(bodyRows()).toEqual(["Hermes", "Open WebUI", "Notebook"]);

    await user.click(screen.getByRole("button", { name: "Services column options" }));
    await user.click(await screen.findByRole("menuitem", { name: "High → low" }));
    expect(currentLocation(router)).toBe(
      "/apps/catalog?sort=-services&cols=application%2Cservices",
    );
    expect(
      screen.getByRole("button", { name: "Services column options" }).closest("th"),
    ).toHaveAttribute("aria-sort", "descending");
    expect(bodyRows()).toEqual(["Notebook", "Hermes", "Open WebUI"]);
  });

  it("shows Uses, Platforms, and Services from the View menu", async () => {
    const { router, user } = renderCatalog();
    await loaded();

    await user.click(screen.getByRole("button", { name: "View" }));
    const menu = await screen.findByRole("menu");
    expect(
      within(menu)
        .getAllByRole("menuitemcheckbox")
        .map((item) => [item.textContent, item.getAttribute("aria-checked")]),
    ).toEqual([
      ["Version", "true"],
      ["Uses", "false"],
      ["Platforms", "false"],
      ["Services", "false"],
    ]);
    await user.click(within(menu).getByRole("menuitemcheckbox", { name: "Uses" }));
    await user.click(within(menu).getByRole("menuitemcheckbox", { name: "Platforms" }));
    await user.click(within(menu).getByRole("menuitemcheckbox", { name: "Services" }));

    expect(headers()).toEqual(["Application", "Version", "Uses", "Platforms", "Services"]);
    expect(cells("Hermes").slice(2)).toEqual([
      "Inference routePersistent storage",
      "linux/amd64linux/arm64",
      "1",
    ]);
    expect(cells("Notebook").slice(2)).toEqual(["Persistent storage", "linux/amd64", "2"]);
    expect(currentLocation(router)).toBe(
      "/apps/catalog?cols=application%2Cversion%2Cuses%2Cplatforms%2Cservices",
    );
  });

  it("shows the no-match state and resets the filters from it", async () => {
    const { router, user } = renderCatalog(
      "?q=hermes&f=uses&uses=inference-route&sort=-name&platform=linux%2Fs390x",
    );
    await loaded();

    expect(within(table()).getByText("No applications match.")).toBeVisible();
    expect(footer()).toHaveTextContent("0 of 3 applications");
    await user.click(screen.getByRole("button", { name: "Reset filters" }));

    expect(currentLocation(router)).toBe("/apps/catalog?sort=-name");
    expect(bodyRows()).toEqual(["Open WebUI", "Notebook", "Hermes"]);
  });

  it("opens the catalog application inspector from a row click and from Enter", async () => {
    const { router, user } = renderCatalog();
    await loaded();

    screen.getByRole("row", { name: "Hermes" }).focus();
    await user.keyboard("{Enter}");
    expect(currentLocation(router)).toBe("/apps/catalog?panel=app-catalog%2Fcatalog%3Ahermes-app");
    const hermesPanel = await screen.findByRole("complementary", { name: "Hermes" });
    expect(within(hermesPanel).getByText(hermesDescription)).toBeVisible();

    await user.click(screen.getByRole("row", { name: "Notebook" }));
    expect(currentLocation(router)).toBe("/apps/catalog?panel=app-catalog%2Fcatalog%3Anotebook");
    const panel = await screen.findByRole("complementary", { name: "Notebook" });
    expect(within(panel).getByRole("button", { name: "Deploy" })).toBeVisible();
    expect(within(panel).queryByText(hermesDescription)).not.toBeInTheDocument();
    expect(screen.getByRole("row", { name: "Notebook" })).toHaveAttribute(
      "aria-controls",
      panel.id,
    );
    expect(screen.getByRole("row", { name: "Notebook" })).toHaveAttribute("data-state", "selected");
  });

  it("restores the view from a URL", async () => {
    renderCatalog("?q=e&f=platform&platform=linux%2Farm64&sort=-name&cols=application,platforms");
    await loaded();

    expect(screen.getByRole("searchbox", { name: "Filter applications" })).toHaveValue("e");
    expect(screen.getByRole("button", { name: "Platform filter: linux/arm64" })).toBeVisible();
    expect(headers()).toEqual(["Application", "Platforms"]);
    expect(
      screen.getByRole("button", { name: "Application column options" }).closest("th"),
    ).toHaveAttribute("aria-sort", "descending");
    expect(bodyRows()).toEqual(["Open WebUI", "Hermes"]);
    expect(screen.getByRole("button", { name: /^Reset/ })).toBeVisible();
  });

  it("keeps a filter changed while a panel is open after closing the panel", async () => {
    const { router, user } = renderCatalog();
    await loaded();

    await user.click(screen.getByRole("row", { name: "Hermes" }));
    await user.click(screen.getByRole("row", { name: "Notebook" }));
    await screen.findByRole("complementary", { name: "Notebook" });
    await user.type(screen.getByRole("searchbox", { name: "Filter applications" }), "o");
    expect(currentLocation(router)).toBe(
      "/apps/catalog?q=o&panel=app-catalog%2Fcatalog%3Anotebook",
    );

    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    await waitFor(() => expect(currentLocation(router)).toBe("/apps/catalog?q=o"));
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
    expect(screen.getByRole("searchbox", { name: "Filter applications" })).toHaveValue("o");
    expect(bodyRows()).toEqual(["Notebook", "Open WebUI"]);
  });

  it("leaves the view behind when Back returns from the Catalog panel to its deployment", async () => {
    const installed = deployment(
      hermes,
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
      "deployment-1",
    );
    const { router, user } = renderCatalog("", [installed]);
    await act(() => router.navigate("/apps?panel=apps%2Fdeployment%3Adeployment-1"));

    const section = await screen.findByRole("region", { name: "Application" });
    await user.click(within(section).getByRole("link", { name: "Hermes" }));
    await screen.findByRole("region", { name: "Catalog application" });
    await user.type(screen.getByRole("searchbox", { name: "Filter applications" }), "her");
    expect(currentLocation(router)).toBe(
      "/apps/catalog?q=her&panel=apps%2Fdeployment%3Adeployment-1&panel=app-catalog%2Fcatalog%3Ahermes-app",
    );

    await user.click(screen.getByRole("button", { name: "Back to Hermes" }));
    await waitFor(() =>
      expect(currentLocation(router)).toBe("/apps?panel=apps%2Fdeployment%3Adeployment-1"),
    );
  });

  it("keeps row order and an open chip menu through a watch update returning equal data", async () => {
    const { client, user, watch } = renderCatalog("?f=platform", [
      deployment(hermes, ApplicationLifecycleState.STARTING, ApplicationReadinessState.NOT_READY),
    ]);
    await loaded();

    await user.click(screen.getByRole("button", { name: "Platform filter: any" }));
    const menu = await screen.findByRole("menu");
    const reads = vi.mocked(client.application.listApplicationDeployments).mock.calls.length;

    await act(() => watch.tick());
    expect(
      vi.mocked(client.application.listApplicationDeployments).mock.calls.length,
    ).toBeGreaterThan(reads);
    await act(async () => {});
    expect(screen.getByRole("menu")).toBe(menu);
    expect(menu).toBeVisible();
    expect(menuValues(menu)).toEqual(["linux/amd643", "linux/arm642"]);
    expect(bodyRows()).toEqual(["Hermes", "Notebook", "Open WebUI"]);
  });
});
