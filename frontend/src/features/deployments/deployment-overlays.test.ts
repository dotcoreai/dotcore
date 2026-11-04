import { describe, expect, it } from "vitest";

import {
  type DeploymentFields,
  type DeploymentOverlay,
  mergeDeploymentOverlays,
  withoutSettled,
  withOverlay,
} from "./deployment-overlays";

interface Deployment {
  desiredState: "paused" | "present" | "teardown";
  id: string;
  name: string;
}

const fields: DeploymentFields<Deployment> = {
  desiredState: (deployment) => ["present", "paused", "teardown"].indexOf(deployment.desiredState),
  id: (deployment) => deployment.id,
  tearingDown: (deployment) => deployment.desiredState === "teardown",
};

describe("mergeDeploymentOverlays", () => {
  it("settles each deployment's overlay on its own streamed content", () => {
    const overlays = new Map<string, DeploymentOverlay<Deployment>>([
      ["a", lifecycle(deployment("a", "paused"))],
      ["b", lifecycle(deployment("b", "present"))],
      ["c", rename("renamed")],
    ]);

    const merged = mergeDeploymentOverlays(
      [deployment("a", "present"), deployment("b", "present"), deployment("c", "present")],
      overlays,
      fields,
    );

    expect(merged.deployments).toEqual([
      deployment("a", "paused"),
      deployment("b", "present"),
      { ...deployment("c", "present"), name: "renamed" },
    ]);
    expect(merged.settled).toEqual(["b"]);
  });

  it("adds a created deployment the stream has not listed yet, then settles once listed", () => {
    const overlays = new Map([["new", lifecycle(deployment("new", "present"), true)]]);

    expect(mergeDeploymentOverlays([], overlays, fields)).toEqual({
      deployments: [deployment("new", "present")],
      settled: [],
    });
    expect(
      mergeDeploymentOverlays([deployment("new", "paused")], overlays, fields).settled,
    ).toEqual(["new"]);
  });

  it("settles any overlay of a deployment the stream shows tearing down or no longer lists", () => {
    const overlays = new Map([
      ["a", lifecycle(deployment("a", "paused"))],
      ["b", lifecycle(deployment("b", "teardown"))],
    ]);

    const merged = mergeDeploymentOverlays([deployment("a", "teardown")], overlays, fields);

    expect(merged.deployments).toEqual([deployment("a", "teardown")]);
    expect(merged.settled).toEqual(["a", "b"]);
  });
});

describe("withOverlay", () => {
  it("joins content overlays of one deployment until the stream shows both", () => {
    const overlays = withOverlay(new Map([["c", rename("renamed")]]), "c", flag("c"));
    const listed = (name: string, desiredState: Deployment["desiredState"]) => [
      { ...deployment("c", desiredState), name },
    ];

    expect(mergeDeploymentOverlays(listed("c", "present"), overlays, fields)).toEqual({
      deployments: [{ ...deployment("c", "paused"), name: "renamed" }],
      settled: [],
    });
    expect(mergeDeploymentOverlays(listed("renamed", "present"), overlays, fields).settled).toEqual(
      [],
    );
    expect(mergeDeploymentOverlays(listed("renamed", "paused"), overlays, fields).settled).toEqual([
      "c",
    ]);
  });
});

describe("withoutSettled", () => {
  it("keeps an overlay set for the same deployment after the merge read the settled one", () => {
    const settled = lifecycle(deployment("a", "paused"));
    const newer = lifecycle(deployment("a", "present"));
    const merged = new Map([["a", settled]]);

    expect(withoutSettled(new Map([["a", settled]]), merged, ["a"]).size).toBe(0);
    expect(withoutSettled(new Map([["a", newer]]), merged, ["a"]).get("a")).toBe(newer);
  });
});

function deployment(id: string, desiredState: Deployment["desiredState"]): Deployment {
  return { desiredState, id, name: id };
}

function lifecycle(accepted: Deployment, created = false): DeploymentOverlay<Deployment> {
  return { deployment: accepted, fence: { created, kind: "lifecycle" } };
}

/** A content change standing for an endpoint edit: it shows the deployment paused. */
function flag(id: string): DeploymentOverlay<Deployment> {
  return {
    deployment: deployment(id, "paused"),
    fence: {
      apply: (streamed) => ({ ...streamed, desiredState: "paused" }),
      kind: "content",
      shown: (streamed) => streamed.desiredState === "paused",
    },
  };
}

function rename(name: string): DeploymentOverlay<Deployment> {
  return {
    deployment: { ...deployment("c", "present"), name },
    fence: {
      apply: (streamed) => ({ ...streamed, name }),
      kind: "content",
      shown: (streamed) => streamed.name === name,
    },
  };
}
