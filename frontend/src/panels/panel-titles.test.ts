import { describe, expect, it, vi } from "vitest";

import type { PanelEntry } from "./panel-stack";
import { panelKindLabel, panelTitleKey, PanelTitleStore } from "./panel-titles";

describe("panelTitleKey", () => {
  it.each<[PanelEntry, string | undefined]>([
    [{ page: "models", screen: "deployment", resourceId: "d1" }, "models:d1"],
    [{ page: "models", screen: "logs", resourceId: "d1" }, "models:d1"],
    [{ page: "models", screen: "edit", resourceId: "d1" }, "models:d1"],
    [{ page: "routes", screen: "connect", resourceId: "r1" }, "routes:r1"],
    [{ page: "routes", screen: "create" }, undefined],
    [{ page: "model-catalog", screen: "catalog", resourceId: "m1" }, "model-catalog:m1"],
    [
      { page: "model-catalog", screen: "catalog-profile", resourceId: "m1", profileId: "p" },
      "model-catalog:m1",
    ],
    [{ page: "machines", screen: "machine", resourceId: "n1" }, "machines:n1"],
    [
      { page: "machines", screen: "execution-target", resourceId: "t1" },
      "machines/execution-target:t1",
    ],
    [{ page: "apps", screen: "deployment", resourceId: "a1" }, "apps:a1"],
    [{ page: "app-catalog", screen: "catalog", resourceId: "c1" }, "app-catalog:c1"],
  ])("keys %j as %s", (entry, key) => {
    expect(panelTitleKey(entry)).toBe(key);
  });
});

describe("panelKindLabel", () => {
  it.each<[PanelEntry, string]>([
    [{ page: "models", screen: "deployment" }, "model deployment"],
    [{ page: "routes", screen: "route" }, "route"],
    [{ page: "apps", screen: "deployment" }, "application deployment"],
    [{ page: "app-catalog", screen: "catalog" }, "application"],
    [{ page: "model-catalog", screen: "catalog" }, "model"],
    [{ page: "machines", screen: "machine" }, "machine"],
    [{ page: "machines", screen: "execution-target" }, "execution target"],
  ])("labels %j as %s", (entry, label) => {
    expect(panelKindLabel(entry)).toBe(label);
  });
});

describe("PanelTitleStore", () => {
  const route: PanelEntry = { page: "routes", screen: "route", resourceId: "r1" };

  it("stores a published name and serves it for every screen of the resource", () => {
    const store = new PanelTitleStore();

    expect(store.publish([{ entry: route, name: "chat" }])).toBe(true);

    expect(store.get("routes:r1")).toBe("chat");
    expect(store.get(panelTitleKey({ ...route, screen: "edit" })!)).toBe("chat");
  });

  it("notifies once for a change and not for an unchanged republish", () => {
    const store = new PanelTitleStore();
    const listener = vi.fn();
    store.subscribe(listener);

    store.publish([{ entry: route, name: "chat" }]);
    expect(store.publish([{ entry: route, name: "chat" }])).toBe(false);
    store.publish([{ entry: route, name: "chat-2" }]);

    expect(listener).toHaveBeenCalledTimes(2);
    expect(store.get("routes:r1")).toBe("chat-2");
  });

  it("ignores empty names and entries that name no resource", () => {
    const store = new PanelTitleStore();

    expect(
      store.publish([
        { entry: route, name: "" },
        { entry: { page: "routes", screen: "create" }, name: "new" },
      ]),
    ).toBe(false);
    expect(store.get("routes:r1")).toBeUndefined();
  });

  it("stops notifying an unsubscribed listener", () => {
    const store = new PanelTitleStore();
    const listener = vi.fn();
    store.subscribe(listener)();

    store.publish([{ entry: route, name: "chat" }]);

    expect(listener).not.toHaveBeenCalled();
  });
});
