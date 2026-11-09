import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import {
  ModelDeploymentDesiredState,
  ModelDeploymentSchema,
  ModelDeploymentSnapshotSchema,
  ResolvedLocalModelDeploymentSnapshotSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStateSchema, ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";

import {
  isModelDesiredStateVisible,
  isTransitioningModelStatus,
  localModelSnapshot,
  modelDeploymentDesiredStateLabel,
  modelStatusLabel,
  modelStatusMessage,
  projectModelDeploymentStatus,
  projectModelSnapshot,
} from "./model-projection";

const deployment = create(ModelDeploymentSchema, {
  modelDeploymentId: "deployment-1",
  snapshot: create(ModelDeploymentSnapshotSchema, {
    source: {
      case: "local",
      value: create(ResolvedLocalModelDeploymentSnapshotSchema, {
        modelId: "model-1",
        modelDisplayName: "Model",
        modelVersion: "1",
        configurationId: "config",
        profileId: "profile",
      }),
    },
  }),
});

describe("model projection", () => {
  it("names each deployment target without borrowing a lifecycle word", () => {
    expect(modelDeploymentDesiredStateLabel(ModelDeploymentDesiredState.PRESENT)).toBe("Present");
    expect(modelDeploymentDesiredStateLabel(ModelDeploymentDesiredState.PAUSED)).toBe("Paused");
    expect(modelDeploymentDesiredStateLabel(ModelDeploymentDesiredState.TEARDOWN)).toBe("Teardown");
    expect(modelDeploymentDesiredStateLabel(ModelDeploymentDesiredState.UNSPECIFIED)).toBe(
      "Unspecified",
    );
  });

  it("projects local resolved snapshots instead of mutable catalog models", () => {
    expect(localModelSnapshot(deployment.snapshot)).toMatchObject({
      modelId: "model-1",
      profileId: "profile",
      configurationId: "config",
    });
    expect(projectModelSnapshot([], [deployment]).runningModels).toEqual([deployment]);
  });

  it("labels downloading from typed status as transitioning", () => {
    expect(modelStatusLabel(ModelDeploymentStatus.DOWNLOADING)).toBe("Downloading");
    expect(isTransitioningModelStatus(ModelDeploymentStatus.DOWNLOADING)).toBe(true);
  });
});

describe("model deployment status projection", () => {
  function deploymentInState(
    status: ModelDeploymentStatus,
    state: Partial<{ failure: { code: string; message: string }; statusMessage: string }> = {},
    desiredState = ModelDeploymentDesiredState.PRESENT,
  ) {
    return create(ModelDeploymentSchema, {
      desiredState,
      modelDeploymentId: "deployment-1",
      state: create(ModelDeploymentStateSchema, { status, ...state }),
    });
  }

  it.each([
    [ModelDeploymentStatus.QUEUED, "Queued", "info"],
    [ModelDeploymentStatus.PREPARING, "Preparing", "info"],
    [ModelDeploymentStatus.DOWNLOADING, "Downloading", "info"],
    [ModelDeploymentStatus.LOADING, "Loading", "info"],
    [ModelDeploymentStatus.WARMING_UP, "Warming up", "info"],
    [ModelDeploymentStatus.READY, "Ready", "success"],
    [ModelDeploymentStatus.UNLOADING, "Tearing down", "info"],
    [ModelDeploymentStatus.PAUSED, "Paused", "neutral"],
    [ModelDeploymentStatus.FAILED, "Failed", "destructive"],
    [ModelDeploymentStatus.UNAVAILABLE, "Unavailable", "destructive"],
    [ModelDeploymentStatus.UNSPECIFIED, "Unknown", "neutral"],
  ])("projects %s as %s with %s tone", (status, label, tone) => {
    expect(projectModelDeploymentStatus(deploymentInState(status))).toEqual({
      label,
      tone,
    });
  });

  it("keeps download progress off the card; the inspector is the only progress surface", () => {
    expect(
      projectModelDeploymentStatus(deploymentInState(ModelDeploymentStatus.DOWNLOADING)).attention,
    ).toBeUndefined();
  });

  it("reports the backend failure message as the attention label", () => {
    expect(
      projectModelDeploymentStatus(
        deploymentInState(ModelDeploymentStatus.FAILED, {
          failure: { code: "EXIT", message: "Engine exited with status 1" },
        }),
      ),
    ).toEqual({
      attention: "Engine exited with status 1",
      label: "Failed",
      tone: "destructive",
    });
  });

  it("reports a teardown target as Tearing down whatever the reported status is", () => {
    expect(
      projectModelDeploymentStatus(
        deploymentInState(ModelDeploymentStatus.READY, {}, ModelDeploymentDesiredState.TEARDOWN),
      ).label,
    ).toBe("Tearing down");
  });

  it.each([
    [ModelDeploymentDesiredState.PRESENT, ModelDeploymentStatus.READY, false],
    [ModelDeploymentDesiredState.PRESENT, ModelDeploymentStatus.LOADING, false],
    [ModelDeploymentDesiredState.PRESENT, ModelDeploymentStatus.FAILED, false],
    [ModelDeploymentDesiredState.PRESENT, ModelDeploymentStatus.UNAVAILABLE, false],
    [ModelDeploymentDesiredState.PRESENT, ModelDeploymentStatus.PAUSED, true],
    [ModelDeploymentDesiredState.PRESENT, ModelDeploymentStatus.PAUSING, true],
    [ModelDeploymentDesiredState.PAUSED, ModelDeploymentStatus.READY, false],
    [ModelDeploymentDesiredState.PAUSED, ModelDeploymentStatus.PAUSED, false],
    [ModelDeploymentDesiredState.TEARDOWN, ModelDeploymentStatus.PAUSED, false],
  ])("desired %s with status %s shows the target: %s", (desired, status, visible) => {
    expect(isModelDesiredStateVisible(deploymentInState(status, {}, desired))).toBe(visible);
  });

  it("keeps the failure message reachable while a pause or teardown rewrites the status word", () => {
    const failure = { code: "EXIT", message: "Engine exited with status 1" };

    expect(
      projectModelDeploymentStatus(
        deploymentInState(
          ModelDeploymentStatus.FAILED,
          { failure },
          ModelDeploymentDesiredState.PAUSED,
        ),
      ),
    ).toEqual({ attention: "Engine exited with status 1", label: "Pausing", tone: "info" });
    expect(
      projectModelDeploymentStatus(
        deploymentInState(
          ModelDeploymentStatus.FAILED,
          { failure },
          ModelDeploymentDesiredState.TEARDOWN,
        ),
      ).attention,
    ).toBe("Engine exited with status 1");
  });

  it.each([
    ["empty", "", undefined],
    ["the status word in another case", "ready", undefined],
    ["the failure message", "Engine exited with status 1", undefined],
    ["a transitional note", "Reconciling", "Reconciling"],
  ])("shows the status message only when it adds information: %s", (_name, message, expected) => {
    const status =
      message === "Engine exited with status 1"
        ? ModelDeploymentStatus.FAILED
        : ModelDeploymentStatus.READY;
    const state =
      message === "Engine exited with status 1"
        ? { statusMessage: message, failure: { code: "EXIT", message } }
        : { statusMessage: message };

    expect(modelStatusMessage(deploymentInState(status, state))).toBe(expected);
  });
});
