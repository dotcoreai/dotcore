import { describe, expect, it } from "vitest";

import {
  appendPanel,
  appendPanelHref,
  appendPanelLocation,
  canonicalizePanelLocation,
  closePanelLocation,
  createPanelHistoryState,
  deepensPanelStack,
  hasInvalidPanelEntries,
  isPanelHistoryState,
  type PanelEntry,
  PanelHistoryCoordinator,
  panelHostPath,
  panelPagePath,
  parsePanelEntry,
  parsePanelStack,
  planPanelClose,
  popPanelLocation,
  replaceActivePanel,
  selectPanel,
  selectPanelLocation,
  serializePanelEntry,
  serializePanelStack,
  sidebarParent,
} from "@/lib/panel-stack";

const deployment: PanelEntry = {
  page: "models",
  resourceId: "deployment-1",
  screen: "deployment",
};
const route: PanelEntry = { page: "routes", resourceId: "route-1", screen: "route" };
const connect: PanelEntry = { page: "routes", resourceId: "route-1", screen: "connect" };
const catalog: PanelEntry = { page: "model-catalog", resourceId: "catalog-1", screen: "catalog" };
const profile: PanelEntry = {
  page: "model-catalog",
  profileId: "fast-start",
  resourceId: "catalog-1",
  screen: "catalog-profile",
};
const modelLogs: PanelEntry = { page: "models", resourceId: "deployment-1", screen: "logs" };
const applicationLogs: PanelEntry = { page: "apps", resourceId: "deployment-1", screen: "logs" };
const modelEdit: PanelEntry = { page: "models", resourceId: "deployment-1", screen: "edit" };
const configuration: PanelEntry = {
  page: "models",
  resourceId: "deployment-1",
  screen: "configuration",
};
const otherDeployment: PanelEntry = {
  page: "models",
  resourceId: "deployment-2",
  screen: "deployment",
};
const applicationDeployment: PanelEntry = {
  page: "apps",
  resourceId: "application-deployment-1",
  screen: "deployment",
};
const applicationCatalog: PanelEntry = {
  page: "app-catalog",
  resourceId: "application-1",
  screen: "catalog",
};
const otherApplicationDeployment: PanelEntry = {
  page: "apps",
  resourceId: "application-deployment-2",
  screen: "deployment",
};
const machine: PanelEntry = { page: "machines", resourceId: "node-1", screen: "machine" };
const executionTarget: PanelEntry = {
  page: "machines",
  resourceId: "target-1",
  screen: "execution-target",
};
const applicationEdit = {
  page: "apps",
  resourceId: "application-deployment-1",
  screen: "edit",
} as unknown as PanelEntry;

describe("panel stack", () => {
  it("deepens the stack only when the next one holds more panels", () => {
    const deployment = "?panel=apps%2Fdeployment%3Ad";
    const catalogY = `${deployment}&panel=app-catalog%2Fcatalog%3Ay`;
    const catalogZ = `${deployment}&panel=app-catalog%2Fcatalog%3Az`;

    expect(deepensPanelStack("", "?panel=app-catalog%2Fcatalog%3Ay")).toBe(true);
    expect(deepensPanelStack(deployment, catalogY)).toBe(true);
    expect(deepensPanelStack(catalogY, catalogZ)).toBe(false);
    expect(deepensPanelStack(catalogY, deployment)).toBe(false);
  });

  it.each<[string, PanelEntry]>([
    ["routes/create", { page: "routes", screen: "create" }],
    ["routes/route:route-1", route],
    ["routes/edit:route-1", { ...route, screen: "edit" }],
    ["routes/connect:route-1", connect],
  ])("parses the Routes entry %s", (value, entry) => {
    expect(parsePanelEntry(value)).toEqual(entry);
    expect(serializePanelEntry(entry)).toBe(value);
  });

  it.each([
    "router/create",
    "router/route:route-1",
    "router/edit:route-1",
    "router/connect:route-1",
  ])("rejects the unsupported page identity %s without dropping a valid parent", (value) => {
    expect(parsePanelEntry(value)).toBeUndefined();
    const search = `?tab=traffic&panel=models%2Fdeployment%3Adeployment-1&panel=${encodeURIComponent(value)}`;
    expect(hasInvalidPanelEntries(search)).toBe(true);
    expect(canonicalizePanelLocation({ pathname: "/routes", search })).toEqual({
      pathname: "/models",
      recovered: true,
      search: "?panel=models%2Fdeployment%3Adeployment-1",
      stack: [deployment],
    });
  });

  it.each(["edit", "connect"] as const)(
    "restores the route detail parent for a direct Routes %s link",
    (screen) => {
      expect(
        canonicalizePanelLocation({
          pathname: "/routes",
          search: `?tab=traffic&panel=routes%2F${screen}%3Aroute-1`,
        }),
      ).toEqual({
        pathname: "/routes",
        recovered: true,
        search: `?tab=traffic&panel=routes%2Froute%3Aroute-1&panel=routes%2F${screen}%3Aroute-1`,
        stack: [route, { ...route, screen }],
      });
    },
  );

  it("round-trips typed entries and preserves unrelated query parameters", () => {
    expect(parsePanelEntry(serializePanelEntry(deployment))).toEqual(deployment);
    expect(serializePanelStack("filter=active&tab=details", [deployment, route])).toBe(
      "?filter=active&tab=details&panel=models%2Fdeployment%3Adeployment-1&panel=routes%2Froute%3Aroute-1",
    );
  });

  it("round-trips catalog profile details and rejects the removed profile-list destination", () => {
    expect(parsePanelEntry(serializePanelEntry(profile))).toEqual(profile);
    expect(parsePanelEntry("model-catalog/catalog-profile-list:catalog-1")).toBeUndefined();
    expect(appendPanel(appendPanel("", catalog), profile)).toBe(
      "?panel=model-catalog%2Fcatalog%3Acatalog-1&panel=model-catalog%2Fcatalog-profile%3Acatalog-1%3Afast-start",
    );
    expect(
      canonicalizePanelLocation({
        pathname: "/models/catalog",
        search: "?panel=model-catalog%2Fcatalog-profile%3Acatalog-1%3Afast-start",
      }),
    ).toEqual({
      pathname: "/models/catalog",
      recovered: true,
      search:
        "?panel=model-catalog%2Fcatalog%3Acatalog-1&panel=model-catalog%2Fcatalog-profile%3Acatalog-1%3Afast-start",
      stack: [catalog, profile],
    });
  });

  it.each<[string, PanelEntry]>([
    ["model-catalog/catalog:catalog-1", catalog],
    ["model-catalog/catalog-profile:catalog-1:fast-start", profile],
  ])("parses the Catalog page entry %s and maps it to /models/catalog", (value, entry) => {
    expect(parsePanelEntry(value)).toEqual(entry);
    expect(serializePanelEntry(entry)).toBe(value);
    expect(panelPagePath(entry.page)).toBe("/models/catalog");
  });

  it.each([
    "models/catalog:catalog-1",
    "models/catalog-profile:catalog-1:fast-start",
    "models/upstream-model-template:openai-compatible",
  ])("drops the stale Models page entry %s", (value) => {
    expect(parsePanelEntry(value)).toBeUndefined();
    expect(
      canonicalizePanelLocation({
        pathname: "/models",
        search: `?panel=${encodeURIComponent(value)}`,
      }),
    ).toEqual({ pathname: "/models", recovered: true, search: "", stack: [] });
  });

  it("does not imply a catalog parent for a profile on another page", () => {
    expect(parsePanelEntry("models/catalog-profile:catalog-1:fast-start")).toBeUndefined();
    expect(parsePanelStack(serializePanelStack("", [catalog, profile]))).toEqual([
      catalog,
      profile,
    ]);
    expect(
      parsePanelStack(serializePanelStack("", [{ ...catalog, resourceId: "catalog-2" }, profile])),
    ).toEqual([{ ...catalog, resourceId: "catalog-2" }]);
  });

  it("appends a catalog model above a deployment across pages", () => {
    const search = appendPanel(serializePanelStack("", [deployment]), catalog);
    expect(search).toBe(
      "?panel=models%2Fdeployment%3Adeployment-1&panel=model-catalog%2Fcatalog%3Acatalog-1",
    );
    expect(canonicalizePanelLocation({ pathname: "/models", search })).toEqual({
      pathname: "/models/catalog",
      recovered: true,
      search,
      stack: [deployment, catalog],
    });
    expect(popPanelLocation("/models/catalog", search)).toEqual({
      pathname: "/models",
      search: "?panel=models%2Fdeployment%3Adeployment-1",
    });
    expect(parsePanelStack(appendPanel(search, { ...profile }))).toEqual([
      deployment,
      catalog,
      profile,
    ]);
  });

  it("closes a catalog model opened from a deployment to the deployment's page", () => {
    const search = serializePanelStack("", [deployment, catalog, profile]);
    expect(closePanelLocation({ pathname: "/models/catalog", search })).toEqual({
      pathname: "/models",
      search: "",
    });
    expect(
      planPanelClose(
        { pathname: "/models/catalog", search },
        createPanelHistoryState("session-1", 3),
        3,
      ),
    ).toEqual({
      historyDelta: -3,
      location: { pathname: "/models", search: "" },
      replace: false,
    });
    expect(
      closePanelLocation({
        pathname: "/models/catalog",
        search: serializePanelStack("", [catalog]),
      }),
    ).toEqual({ pathname: "/models/catalog", search: "" });
  });

  it("removes a stale profile destination while retaining its available catalog parent", () => {
    expect(
      canonicalizePanelLocation(
        {
          pathname: "/models/catalog",
          search:
            "?panel=model-catalog%2Fcatalog%3Acatalog-1&panel=model-catalog%2Fcatalog-profile%3Acatalog-1%3Afast-start",
        },
        { isAvailable: (entry) => entry.screen !== "catalog-profile" },
      ),
    ).toEqual({
      pathname: "/models/catalog",
      recovered: true,
      search: "?panel=model-catalog%2Fcatalog%3Acatalog-1",
      stack: [catalog],
    });
  });

  it("round-trips an execution target above its machine and drops one without a machine", () => {
    const search = appendPanel(serializePanelStack("", [machine]), executionTarget);

    expect(parsePanelEntry("machines/execution-target:target-1")).toEqual(executionTarget);
    expect(serializePanelEntry(executionTarget)).toBe("machines/execution-target:target-1");
    expect(search).toBe(
      "?panel=machines%2Fmachine%3Anode-1&panel=machines%2Fexecution-target%3Atarget-1",
    );
    expect(parsePanelStack(search)).toEqual([machine, executionTarget]);
    expect(popPanelLocation("/machines", search)).toEqual({
      pathname: "/machines",
      search: "?panel=machines%2Fmachine%3Anode-1",
    });
    expect(appendPanel(serializePanelStack("", [deployment]), executionTarget)).toBe(
      "?panel=models%2Fdeployment%3Adeployment-1",
    );
    expect(
      canonicalizePanelLocation({
        pathname: "/machines",
        search: "?panel=machines%2Fexecution-target%3Atarget-1",
      }),
    ).toEqual({ pathname: "/machines", recovered: true, search: "", stack: [] });
  });

  it("removes an execution target its machine does not own, keeping the machine", () => {
    const owners = new Map([["target-1", "node-1"]]);
    const registry = {
      isAvailable: (entry: PanelEntry, parent: PanelEntry | undefined) =>
        entry.screen !== "execution-target" || owners.get(entry.resourceId!) === parent?.resourceId,
    };
    const location = (targetId: string, machineId = "node-1") => ({
      pathname: "/machines",
      search: serializePanelStack("", [
        { ...machine, resourceId: machineId },
        { ...executionTarget, resourceId: targetId },
      ]),
    });

    expect(canonicalizePanelLocation(location("target-1"), registry)).toEqual({
      ...location("target-1"),
      recovered: false,
      stack: [machine, executionTarget],
    });
    for (const [targetId, machineId] of [
      ["unknown", "node-1"],
      ["target-1", "node-2"],
    ]) {
      expect(canonicalizePanelLocation(location(targetId, machineId), registry)).toEqual({
        pathname: "/machines",
        recovered: true,
        search: `?panel=machines%2Fmachine%3A${machineId}`,
        stack: [{ ...machine, resourceId: machineId }],
      });
    }
  });

  it("does not recognize superseded selection URLs", () => {
    expect(parsePanelStack("sel=models%2Fdeployment%3Adeployment-1")).toEqual([]);
  });

  it("drops malformed, impossible, duplicate, cyclic, and over-depth suffixes", () => {
    expect(
      parsePanelStack(
        "panel=models%2Fdeployment%3Adeployment-1&panel=routes%2Froute%3Aroute-1&panel=routes%2Fconnect%3Aroute-1&panel=routes%2Fconnect%3Aroute-1",
      ),
    ).toEqual([deployment, route, connect]);
    expect(
      parsePanelStack("panel=models%2Fdeployment%3Adeployment-1&panel=routes%2Fconnect%3Aroute-1"),
    ).toEqual([deployment]);
    expect(
      parsePanelStack(
        Array.from({ length: 9 }, (_, index) => `panel=models/deployment:${index}`).join("&"),
      ),
    ).toHaveLength(1);
    expect(hasInvalidPanelEntries("panel=models%2Fdeployment%3Adeployment-1&panel=oops")).toBe(
      true,
    );
  });

  it("pushes children, replaces lateral selections, and pops to the parent pathname", () => {
    const initial = "?tab=traffic&panel=models%2Fdeployment%3Adeployment-1";
    expect(appendPanel(initial, route)).toBe(
      "?tab=traffic&panel=models%2Fdeployment%3Adeployment-1&panel=routes%2Froute%3Aroute-1",
    );
    expect(selectPanel(initial, { ...deployment, resourceId: "deployment-2" })).toBe(
      "?tab=traffic&panel=models%2Fdeployment%3Adeployment-2",
    );
    expect(replaceActivePanel(initial, { ...deployment, resourceId: "deployment-2" })).toBe(
      "?tab=traffic&panel=models%2Fdeployment%3Adeployment-2",
    );
    expect(popPanelLocation("/routes", appendPanel(initial, route))).toEqual({
      pathname: "/models",
      search: "?panel=models%2Fdeployment%3Adeployment-1",
    });
  });

  it("links a pushed panel at its own page path, keeping page parameters only on the same page", () => {
    const machine: PanelEntry = { page: "machines", resourceId: "node-1", screen: "machine" };
    const applicationLocation = serializePanelStack("", [applicationDeployment]);
    const filteredApps = { pathname: "/apps", search: `?q=web&${applicationLocation.slice(1)}` };

    expect(appendPanelHref(filteredApps, machine)).toBe(
      "/machines?panel=apps%2Fdeployment%3Aapplication-deployment-1&panel=machines%2Fmachine%3Anode-1",
    );
    expect(appendPanelHref(filteredApps, route)).toBe(
      "/routes?panel=apps%2Fdeployment%3Aapplication-deployment-1&panel=routes%2Froute%3Aroute-1",
    );
    expect(appendPanelHref(filteredApps, applicationCatalog)).toBe(
      "/apps/catalog?panel=apps%2Fdeployment%3Aapplication-deployment-1&panel=app-catalog%2Fcatalog%3Aapplication-1",
    );
    expect(appendPanelHref(filteredApps, applicationEdit)).toBe(
      "/apps?q=web&panel=apps%2Fdeployment%3Aapplication-deployment-1&panel=apps%2Fedit%3Aapplication-deployment-1",
    );
    expect(popPanelLocation("/machines", appendPanel(applicationLocation, machine))).toEqual({
      pathname: "/apps",
      search: applicationLocation,
    });
  });

  it("round-trips model and application deployment Edit entries", () => {
    expect(parsePanelEntry(serializePanelEntry(modelEdit))).toEqual(modelEdit);
    expect(parsePanelEntry(serializePanelEntry(applicationEdit))).toEqual(applicationEdit);
  });

  it("synthesizes immutable-ID deployment parents for direct model and application Edit links", () => {
    expect(
      canonicalizePanelLocation({
        pathname: "/models",
        search: "?panel=models%2Fedit%3Adeployment-1",
      }),
    ).toEqual({
      pathname: "/models",
      recovered: true,
      search: "?panel=models%2Fdeployment%3Adeployment-1&panel=models%2Fedit%3Adeployment-1",
      stack: [deployment, modelEdit],
    });
    expect(
      canonicalizePanelLocation({
        pathname: "/apps",
        search: "?panel=apps%2Fedit%3Aapplication-deployment-1",
      }),
    ).toEqual({
      pathname: "/apps",
      recovered: true,
      search:
        "?panel=apps%2Fdeployment%3Aapplication-deployment-1&panel=apps%2Fedit%3Aapplication-deployment-1",
      stack: [applicationDeployment, applicationEdit],
    });
  });

  it("rejects deployment Edit entries without immutable IDs", () => {
    expect(parsePanelEntry("models/edit")).toBeUndefined();
    expect(parsePanelEntry("apps/edit")).toBeUndefined();
    expect(hasInvalidPanelEntries("panel=models%2Fedit&panel=apps%2Fedit")).toBe(true);
  });

  it("pushes an Edit child, pops Back to its parent, and clears it on Close", () => {
    const modelLocation = "?panel=models%2Fdeployment%3Adeployment-1";
    const applicationLocation = "?panel=apps%2Fdeployment%3Aapplication-deployment-1";
    expect(appendPanel(modelLocation, modelEdit)).toBe(
      "?panel=models%2Fdeployment%3Adeployment-1&panel=models%2Fedit%3Adeployment-1",
    );
    expect(popPanelLocation("/models", appendPanel(modelLocation, modelEdit))).toEqual({
      pathname: "/models",
      search: modelLocation,
    });
    expect(
      closePanelLocation({
        pathname: "/apps",
        search: appendPanel(applicationLocation, applicationEdit),
      }),
    ).toEqual({
      pathname: "/apps",
      search: "",
    });
  });

  it("restores model and application deployment parents for direct log URLs", () => {
    expect(
      canonicalizePanelLocation({
        pathname: "/models",
        search: "?panel=models%2Flogs%3Adeployment-1",
      }),
    ).toEqual({
      pathname: "/models",
      recovered: true,
      search: "?panel=models%2Fdeployment%3Adeployment-1&panel=models%2Flogs%3Adeployment-1",
      stack: [deployment, modelLogs],
    });
    expect(
      canonicalizePanelLocation({
        pathname: "/apps",
        search: "?panel=apps%2Flogs%3Adeployment-1",
      }),
    ).toEqual({
      pathname: "/apps",
      recovered: true,
      search: "?panel=apps%2Fdeployment%3Adeployment-1&panel=apps%2Flogs%3Adeployment-1",
      stack: [{ page: "apps", resourceId: "deployment-1", screen: "deployment" }, applicationLogs],
    });
  });

  it("keeps the sidebar's parent beneath a log window drawn over it", () => {
    const machine: PanelEntry = { page: "machines", resourceId: "machine-1", screen: "machine" };

    expect(sidebarParent(serializePanelStack("", [deployment, modelLogs]))).toBeUndefined();
    expect(sidebarParent(serializePanelStack("", [machine, deployment, modelLogs]))).toEqual(
      machine,
    );
    expect(sidebarParent(serializePanelStack("", [machine, deployment]))).toEqual(machine);
    expect(sidebarParent(serializePanelStack("", [deployment, configuration]))).toEqual(deployment);
    expect(sidebarParent("")).toBeUndefined();
  });

  it("rejects log panels without their matching deployment parent", () => {
    expect(
      parsePanelStack("panel=models%2Fdeployment%3Adeployment-1&panel=apps%2Flogs%3Adeployment-1"),
    ).toEqual([deployment]);
  });

  it("pushes the deployment Configuration child and rebuilds its parent from a direct link", () => {
    const search = serializePanelStack("", [deployment]);
    expect(parsePanelStack(appendPanel(search, configuration))).toEqual([
      deployment,
      configuration,
    ]);

    const direct = canonicalizePanelLocation({
      pathname: "/models",
      search: serializePanelStack("", [configuration]),
    });
    expect(direct.stack).toEqual([deployment, configuration]);
    expect(direct.recovered).toBe(true);
  });

  it("rejects the removed Details screen and drops a deep link that names it", () => {
    // `models` no longer lists `details`, so the entry fails `isPanelScreen` at parse time and
    // never reaches stack normalization.
    expect(parsePanelEntry("models/details:deployment-1")).toBeUndefined();
    expect(parsePanelStack("?panel=models%2Fdetails%3Adeployment-1")).toEqual([]);
    expect(hasInvalidPanelEntries("?panel=models%2Fdetails%3Adeployment-1")).toBe(true);

    // Parsing stops at the unknown child, so a valid parent ahead of it survives.
    const search = serializePanelStack("", [deployment]);
    expect(parsePanelStack(`${search}&panel=models%2Fdetails%3Adeployment-1`)).toEqual([
      deployment,
    ]);

    // A link naming only the removed screen empties the stack and closes the panel.
    expect(
      canonicalizePanelLocation({
        pathname: "/models",
        search: "?panel=models%2Fdetails%3Adeployment-1",
      }).stack,
    ).toEqual([]);
  });

  it("parses the Application Catalog page entry and maps it to /apps/catalog", () => {
    expect(parsePanelEntry("app-catalog/catalog:application-1")).toEqual(applicationCatalog);
    expect(serializePanelEntry(applicationCatalog)).toBe("app-catalog/catalog:application-1");
    expect(panelPagePath("app-catalog")).toBe("/apps/catalog");
  });

  it("drops the stale Apps page Catalog entry instead of rewriting it", () => {
    expect(parsePanelEntry("apps/catalog:application-1")).toBeUndefined();
    expect(
      canonicalizePanelLocation({
        pathname: "/apps",
        search: "?panel=apps%2Fcatalog%3Aapplication-1",
      }),
    ).toEqual({ pathname: "/apps", recovered: true, search: "", stack: [] });
  });

  it("stacks a catalog application above a deployment as a cross-page push", () => {
    const search = appendPanel(
      serializePanelStack("", [applicationDeployment]),
      applicationCatalog,
    );

    expect(parsePanelStack(search)).toEqual([applicationDeployment, applicationCatalog]);
    expect(canonicalizePanelLocation({ pathname: "/apps", search }).pathname).toBe("/apps/catalog");
    expect(closePanelLocation({ pathname: "/apps/catalog", search })).toEqual({
      pathname: "/apps",
      search: "",
    });
  });

  it("cuts a hand-written same-page deployment to Catalog URL short at the Catalog entry", () => {
    expect(
      parsePanelStack(
        "?panel=apps%2Fdeployment%3Aapplication-deployment-1&panel=apps%2Fcatalog%3Aapplication-1",
      ),
    ).toEqual([applicationDeployment]);
    expect(
      hasInvalidPanelEntries(
        "?panel=apps%2Fdeployment%3Aapplication-deployment-1&panel=apps%2Fcatalog%3Aapplication-1",
      ),
    ).toBe(true);
  });

  it("rejects pushing a root onto another root of the same page", () => {
    expect(
      parsePanelStack(
        appendPanel(serializePanelStack("", [applicationDeployment]), otherApplicationDeployment),
      ),
    ).toEqual([applicationDeployment]);
    expect(
      parsePanelStack(
        appendPanel(serializePanelStack("", [applicationCatalog]), applicationDeployment),
      ),
    ).toEqual([applicationCatalog, applicationDeployment]);
  });

  it("replaces a same-page root when another card is selected", () => {
    expect(
      parsePanelStack(
        selectPanel(serializePanelStack("", [applicationDeployment]), otherApplicationDeployment),
      ),
    ).toEqual([otherApplicationDeployment]);

    expect(
      parsePanelStack(selectPanel(serializePanelStack("", [deployment]), otherDeployment)),
    ).toEqual([otherDeployment]);

    expect(
      parsePanelStack(selectPanel(serializePanelStack("", [route, deployment]), otherDeployment)),
    ).toEqual([route, otherDeployment]);
  });

  it("canonicalizes pathname mismatches and stale descendants without retaining them", () => {
    expect(
      canonicalizePanelLocation({
        pathname: "/apps",
        search:
          "?tab=details&panel=models%2Fdeployment%3Adeployment-1&panel=routes%2Froute%3Aroute-1",
      }),
    ).toEqual({
      pathname: "/routes",
      recovered: true,
      search: "?panel=models%2Fdeployment%3Adeployment-1&panel=routes%2Froute%3Aroute-1",
      stack: [deployment, route],
    });
    expect(
      canonicalizePanelLocation(
        {
          pathname: "/routes",
          search:
            "?panel=models%2Fdeployment%3Adeployment-1&panel=routes%2Froute%3Aroute-1&panel=routes%2Fconnect%3Aroute-1",
        },
        { isAvailable: (entry) => entry.screen !== "route" },
      ),
    ).toEqual({
      pathname: "/models",
      recovered: true,
      search: "?panel=models%2Fdeployment%3Adeployment-1",
      stack: [deployment],
    });
  });

  describe("view parameters", () => {
    const view = "q=qwen&engine=vllm&engine=llama-cpp&sort=-memory";
    const catalogPath = "/models/catalog";

    it("keeps non-panel parameters through push, replace, pop, and canonicalization on one page", () => {
      const search = `?${view}`;

      const pushed = selectPanel(search, catalog);
      expect(pushed).toBe(`?${view}&panel=model-catalog%2Fcatalog%3Acatalog-1`);

      const child = appendPanel(pushed, profile);
      expect(child).toBe(
        `?${view}&panel=model-catalog%2Fcatalog%3Acatalog-1&panel=model-catalog%2Fcatalog-profile%3Acatalog-1%3Afast-start`,
      );

      expect(replaceActivePanel(pushed, { ...catalog, resourceId: "catalog-2" })).toBe(
        `?${view}&panel=model-catalog%2Fcatalog%3Acatalog-2`,
      );
      expect(popPanelLocation(catalogPath, child)).toEqual({
        pathname: catalogPath,
        search: pushed,
      });
      expect(canonicalizePanelLocation({ pathname: catalogPath, search: child })).toEqual({
        pathname: catalogPath,
        recovered: false,
        search: child,
        stack: [catalog, profile],
      });
      expect(closePanelLocation({ pathname: catalogPath, search: child })).toEqual({
        pathname: catalogPath,
        search: `?${view}`,
      });
    });

    it("strips non-panel parameters when popping to another page", () => {
      const search = serializePanelStack(`?${view}`, [deployment, catalog]);

      expect(popPanelLocation(catalogPath, search)).toEqual({
        pathname: "/models",
        search: "?panel=models%2Fdeployment%3Adeployment-1",
      });
    });

    it("strips non-panel parameters when closing to another page", () => {
      const search = serializePanelStack(`?${view}`, [deployment, catalog, profile]);

      expect(closePanelLocation({ pathname: catalogPath, search })).toEqual({
        pathname: "/models",
        search: "",
      });
      expect(
        planPanelClose(
          { pathname: catalogPath, search },
          createPanelHistoryState("session-1", 3),
          3,
        ).location,
      ).toEqual({ pathname: "/models", search: "" });
    });

    it("strips non-panel parameters when canonicalization changes the pathname", () => {
      const search = serializePanelStack(`?${view}`, [deployment, catalog]);

      expect(canonicalizePanelLocation({ pathname: "/models", search })).toEqual({
        pathname: catalogPath,
        recovered: true,
        search: serializePanelStack("", [deployment, catalog]),
        stack: [deployment, catalog],
      });
    });

    it("walks deployment, Catalog model, and back without leaking the Catalog view", () => {
      const onDeployment = serializePanelStack("", [deployment]);
      const opened = canonicalizePanelLocation({
        pathname: "/models",
        search: appendPanel(onDeployment, catalog),
      });
      expect(opened.pathname).toBe(catalogPath);

      const viewed = `?${view}&${opened.search.slice(1)}`;
      expect(canonicalizePanelLocation({ pathname: catalogPath, search: viewed })).toMatchObject({
        pathname: catalogPath,
        recovered: false,
        search: viewed,
      });

      expect(popPanelLocation(catalogPath, viewed)).toEqual({
        pathname: "/models",
        search: onDeployment,
      });
    });
  });

  it("renders clean browser Back and Forward traversals directly", () => {
    const coordinator = new PanelHistoryCoordinator();
    expect(
      coordinator.traverse(
        {
          delta: -1,
          location: { pathname: "/models", search: "?panel=models%2Fdeployment%3Adeployment-1" },
        },
        false,
      ),
    ).toEqual({ type: "render" });
    expect(
      coordinator.traverse(
        {
          delta: 1,
          location: { pathname: "/routes", search: "?panel=routes%2Froute%3Aroute-1" },
        },
        false,
      ),
    ).toEqual({ type: "render" });
  });

  it("restores, confirms, cancels, and replays a dirty traversal exactly once", () => {
    const coordinator = new PanelHistoryCoordinator();
    const traversal = {
      delta: -1,
      location: { pathname: "/models", search: "?panel=models%2Fdeployment%3Adeployment-1" },
    };
    const restore = coordinator.traverse(traversal, true);
    expect(restore.type).toBe("restore");
    if (restore.type !== "restore") throw new Error("Expected a guarded restore.");
    expect(restore.delta).toBe(1);
    expect(coordinator.restored(restore.state)).toEqual({ type: "confirm-discard" });
    expect(coordinator.resolveDiscard(false)).toEqual({ type: "stay" });
    expect(coordinator.restored(restore.state)).toEqual({ type: "stay" });

    const replayRestore = coordinator.traverse(traversal, true);
    if (replayRestore.type !== "restore") throw new Error("Expected a guarded restore.");
    expect(coordinator.restored(replayRestore.state)).toEqual({ type: "confirm-discard" });
    expect(coordinator.resolveDiscard(true)).toEqual({ delta: -1, type: "replay" });
    expect(coordinator.traverse(traversal, true)).toEqual({ type: "render" });
    expect(coordinator.traverse(traversal, true).type).toBe("restore");
  });

  it("closes only a known contiguous session and otherwise replaces the current base URL", () => {
    const location = {
      pathname: "/routes",
      search: "?panel=models%2Fdeployment%3Adeployment-1&panel=routes%2Froute%3Aroute-1",
    };
    const sessionState = createPanelHistoryState("session-1", 2);
    expect(planPanelClose(location, sessionState, 2)).toEqual({
      historyDelta: -2,
      location: { pathname: "/routes", search: "" },
      replace: false,
    });
    expect(planPanelClose(location, sessionState, 1)).toEqual({
      location: { pathname: "/routes", search: "" },
      replace: true,
    });
    expect(
      closePanelLocation({
        pathname: "/models",
        search: "?panel=models%2Fdeployment%3Adeployment-1",
      }),
    ).toEqual({
      pathname: "/models",
      search: "",
    });
  });

  describe("hosting page", () => {
    const overview = "/overview";
    const pageView = "range=1h";

    it.each([
      ["deployment", deployment, modelLogs],
      ["route", route, connect],
      ["machine", machine, executionTarget],
    ])(
      "keeps the overview path for a %s stack through open, push, pop and close",
      (_kind, root, child) => {
        const opened = selectPanelLocation({ pathname: overview, search: `?${pageView}` }, root);
        expect(opened).toEqual({
          pathname: overview,
          search: serializePanelStack(`?${pageView}`, [root]),
        });

        const pushed = appendPanelLocation(opened, child);
        expect(pushed).toEqual({
          pathname: overview,
          search: serializePanelStack(`?${pageView}`, [root, child]),
        });
        expect(appendPanelHref(opened, child)).toBe(`${pushed.pathname}${pushed.search}`);
        expect(canonicalizePanelLocation(pushed)).toEqual({
          ...pushed,
          recovered: false,
          stack: [root, child],
        });

        expect(popPanelLocation(overview, pushed.search)).toEqual(opened);
        expect(closePanelLocation(pushed)).toEqual({ pathname: overview, search: `?${pageView}` });
      },
    );

    it("keeps the overview path when a deployment pushes a route or machine mention", () => {
      const opened = serializePanelStack("", [deployment]);

      expect(appendPanelHref({ pathname: overview, search: opened }, route)).toBe(
        `${overview}${serializePanelStack("", [deployment, route])}`,
      );
      expect(appendPanelHref({ pathname: overview, search: opened }, machine)).toBe(
        `${overview}${serializePanelStack("", [deployment, machine])}`,
      );
      expect(popPanelLocation(overview, serializePanelStack("", [deployment, route]))).toEqual({
        pathname: overview,
        search: opened,
      });
    });

    it("leaves the overview for a panel kind it does not host", () => {
      const opened = serializePanelStack(`?${pageView}`, [deployment]);

      expect(appendPanelLocation({ pathname: overview, search: opened }, catalog)).toEqual({
        pathname: "/models/catalog",
        search: serializePanelStack("", [deployment, catalog]),
      });
      expect(
        selectPanelLocation({ pathname: overview, search: "" }, applicationDeployment),
      ).toEqual({
        pathname: "/apps",
        search: serializePanelStack("", [applicationDeployment]),
      });
    });

    it("keeps owning pages rendering only their own panel kinds", () => {
      const opened = serializePanelStack("", [deployment]);

      expect(appendPanelLocation({ pathname: "/models", search: opened }, route)).toEqual({
        pathname: "/routes",
        search: serializePanelStack("", [deployment, route]),
      });
      expect(selectPanelLocation({ pathname: "/routes", search: "" }, deployment)).toEqual({
        pathname: "/models",
        search: opened,
      });
      expect(panelHostPath("/models", "routes")).toBe("/routes");
      expect(panelHostPath(overview, "routes")).toBe(overview);
      expect(panelHostPath(overview, "apps")).toBe("/apps");
    });
  });

  it("marks panel-created history entries with a session and step", () => {
    const state = createPanelHistoryState("session-1", 2, { scroll: "top" });
    expect(state).toEqual({ __dotcorePanel: { sessionId: "session-1", step: 2 }, scroll: "top" });
    expect(isPanelHistoryState(state)).toBe(true);
    expect(isPanelHistoryState({ __dotcorePanel: { sessionId: "session-1", step: -1 } })).toBe(
      false,
    );
    expect(panelPagePath("routes")).toBe("/routes");
  });
});
