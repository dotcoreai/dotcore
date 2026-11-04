import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import {
  ApplicationDeploymentDesiredState,
  ApplicationDeploymentSchema,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  ApplicationDeploymentStateSchema,
  ApplicationLifecycleState,
  ApplicationReadinessState,
} from "@/gen/dotcore/application/v1/state_pb";
import type { Application } from "@/gen/dotcore/catalog/v1/application_pb";
import { ApplicationSchema } from "@/gen/dotcore/catalog/v1/application_pb";

import {
  applicationDeploymentDesiredStateLabel,
  applicationDeploymentEndpoint,
  applicationDeploymentLifecycleState,
  applicationDeploymentReadinessState,
  applicationLifecycleLabel,
  applicationLifecycleTone,
  isPausableApplication,
  isRemovableApplication,
  isResumableApplication,
  projectApplicationDeploymentStatus,
  projectApplicationSnapshot,
} from "./application-projection";

function resolvedSnapshot(application: Application) {
  return {
    applicationId: application.applicationId,
    displayName: application.displayName,
    version: application.version,
    graph: application.graph,
  };
}

function deployment(
  lifecycleState: ApplicationLifecycleState,
  readinessState: ApplicationReadinessState,
  desiredState = ApplicationDeploymentDesiredState.PRESENT,
  endpointUrl?: string,
) {
  return create(ApplicationDeploymentSchema, {
    applicationDeploymentId: "deployment-1",
    applicationDeploymentName: "open-webui-1",
    desiredState,
    state: create(ApplicationDeploymentStateSchema, {
      lifecycleState,
      readinessState,
      endpoint: endpointUrl ? { url: endpointUrl } : undefined,
    }),
  });
}

describe("Application projection", () => {
  // A deployment with no endpoint yet, so the attention column also covers what it waits for.
  it.each([
    [
      ApplicationLifecycleState.QUEUED,
      ApplicationReadinessState.NOT_READY,
      "Queued",
      "info",
      "Waiting for endpoint",
    ],
    [
      ApplicationLifecycleState.STARTING,
      ApplicationReadinessState.NOT_READY,
      "Starting",
      "info",
      "Waiting for endpoint",
    ],
    [
      ApplicationLifecycleState.STOPPING,
      ApplicationReadinessState.NOT_READY,
      "Stopping",
      "info",
      "Waiting for endpoint",
    ],
    [
      ApplicationLifecycleState.PAUSING,
      ApplicationReadinessState.NOT_READY,
      "Pausing",
      "info",
      "Waiting for endpoint",
    ],
    [
      ApplicationLifecycleState.PAUSED,
      ApplicationReadinessState.READY,
      "Paused",
      "neutral",
      undefined,
    ],
    [
      ApplicationLifecycleState.STOPPED,
      ApplicationReadinessState.NOT_READY,
      "Stopped",
      "neutral",
      undefined,
    ],
    [
      ApplicationLifecycleState.UNAVAILABLE,
      ApplicationReadinessState.NOT_READY,
      "Unavailable",
      "destructive",
      undefined,
    ],
    [
      ApplicationLifecycleState.RUNNING,
      ApplicationReadinessState.NOT_READY,
      "Not ready",
      "destructive",
      "Waiting for endpoint",
    ],
    [
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.NOT_READY,
      "Not ready",
      "destructive",
      undefined,
    ],
    [
      ApplicationLifecycleState.FAILED,
      99 as ApplicationReadinessState,
      "Failed",
      "destructive",
      undefined,
    ],
    [
      ApplicationLifecycleState.UNSPECIFIED,
      ApplicationReadinessState.UNSPECIFIED,
      "Unknown",
      "neutral",
      undefined,
    ],
    [
      ApplicationLifecycleState.STOPPING,
      ApplicationReadinessState.READY,
      "Stopping",
      "info",
      "Waiting for endpoint",
    ],
  ])(
    "projects %s/%s as one word %s with %s tone",
    (lifecycle, readiness, label, tone, attention) => {
      expect(projectApplicationDeploymentStatus(deployment(lifecycle, readiness))).toEqual({
        attention,
        label,
        tone,
      });
    },
  );

  it("never joins the lifecycle and readiness words into one label", () => {
    const value = deployment(
      ApplicationLifecycleState.RUNNING,
      ApplicationReadinessState.NOT_READY,
    );

    expect(projectApplicationDeploymentStatus(value).label).not.toContain(" · ");
  });

  it("reports the backend failure message as the attention label", () => {
    const value = create(ApplicationDeploymentSchema, {
      desiredState: ApplicationDeploymentDesiredState.PRESENT,
      state: create(ApplicationDeploymentStateSchema, {
        failure: {
          code: "EXIT",
          message: "Container exited during startup: port 8080 already bound",
        },
        lifecycleState: ApplicationLifecycleState.FAILED,
        readinessState: ApplicationReadinessState.NOT_READY,
      }),
    });

    expect(projectApplicationDeploymentStatus(value)).toEqual({
      attention: "Container exited during startup: port 8080 already bound",
      label: "Failed",
      tone: "destructive",
    });
  });

  it("drops the waiting-for label once the endpoint is assigned", () => {
    const value = deployment(
      ApplicationLifecycleState.STARTING,
      ApplicationReadinessState.NOT_READY,
      ApplicationDeploymentDesiredState.PRESENT,
      "http://localhost:8080",
    );

    expect(projectApplicationDeploymentStatus(value).attention).toBeUndefined();
  });

  it("projects only Ready/Ready as the compact success badge", () => {
    expect(
      projectApplicationDeploymentStatus(
        deployment(ApplicationLifecycleState.READY, ApplicationReadinessState.READY),
      ),
    ).toEqual({ label: "Ready", tone: "success" });
  });

  it("uses application lifecycle and readiness enums independently", () => {
    const value = deployment(
      ApplicationLifecycleState.RUNNING,
      ApplicationReadinessState.NOT_READY,
    );

    expect(applicationDeploymentLifecycleState(value)).toBe(ApplicationLifecycleState.RUNNING);
    expect(applicationDeploymentReadinessState(value)).toBe(ApplicationReadinessState.NOT_READY);
    expect(applicationLifecycleLabel(ApplicationLifecycleState.STARTING)).toBe("Starting");
    expect(applicationDeploymentDesiredStateLabel(ApplicationDeploymentDesiredState.PAUSED)).toBe(
      "Paused",
    );
    expect(applicationDeploymentDesiredStateLabel(ApplicationDeploymentDesiredState.TEARDOWN)).toBe(
      "Teardown",
    );
  });

  it.each([
    [ApplicationLifecycleState.PAUSING, "Pausing", "info"],
    [ApplicationLifecycleState.PAUSED, "Paused", "neutral"],
    [99 as ApplicationLifecycleState, "Unknown", "neutral"],
  ] as const)("maps %s to the exact lifecycle label %s and %s tone", (state, label, tone) => {
    expect(applicationLifecycleLabel(state)).toBe(label);
    expect(applicationLifecycleTone(state)).toBe(tone);
  });

  it("projects pause actions from durable desired intent", () => {
    const present = deployment(
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
      ApplicationDeploymentDesiredState.PRESENT,
    );
    const paused = deployment(
      ApplicationLifecycleState.PAUSED,
      ApplicationReadinessState.NOT_READY,
      ApplicationDeploymentDesiredState.PAUSED,
    );
    const teardown = deployment(
      ApplicationLifecycleState.STOPPED,
      ApplicationReadinessState.NOT_READY,
      ApplicationDeploymentDesiredState.TEARDOWN,
    );

    expect(isPausableApplication(present)).toBe(true);
    expect(isPausableApplication(paused)).toBe(false);
    expect(isPausableApplication(teardown)).toBe(false);
    expect(isResumableApplication(paused)).toBe(true);
    expect(isResumableApplication(present)).toBe(false);
    expect(isResumableApplication(teardown)).toBe(false);
    expect(isRemovableApplication(present)).toBe(true);
    expect(isRemovableApplication(paused)).toBe(true);
    expect(isRemovableApplication(teardown)).toBe(false);
  });

  it("suppresses stale paused readiness and endpoint projection", () => {
    const endpointUrl = "http://100.64.0.7:49152";
    const present = deployment(
      ApplicationLifecycleState.RUNNING,
      ApplicationReadinessState.NOT_READY,
      ApplicationDeploymentDesiredState.PRESENT,
      endpointUrl,
    );
    expect(applicationDeploymentReadinessState(present)).toBe(ApplicationReadinessState.NOT_READY);
    expect(applicationDeploymentEndpoint(present)).toBe(endpointUrl);

    const paused = deployment(
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.READY,
      ApplicationDeploymentDesiredState.PAUSED,
      endpointUrl,
    );

    expect(applicationDeploymentLifecycleState(paused)).toBe(ApplicationLifecycleState.PAUSING);
    expect(applicationDeploymentReadinessState(paused)).toBe(ApplicationReadinessState.UNSPECIFIED);
    expect(applicationDeploymentEndpoint(paused)).toBeUndefined();

    const settledPaused = deployment(
      ApplicationLifecycleState.PAUSED,
      ApplicationReadinessState.READY,
      ApplicationDeploymentDesiredState.PAUSED,
      endpointUrl,
    );
    expect(applicationDeploymentLifecycleState(settledPaused)).toBe(
      ApplicationLifecycleState.PAUSED,
    );
    expect(applicationDeploymentReadinessState(settledPaused)).toBe(
      ApplicationReadinessState.UNSPECIFIED,
    );
    expect(applicationDeploymentEndpoint(settledPaused)).toBeUndefined();

    const unavailable = deployment(
      ApplicationLifecycleState.UNAVAILABLE,
      ApplicationReadinessState.READY,
      ApplicationDeploymentDesiredState.PAUSED,
      endpointUrl,
    );
    expect(applicationDeploymentLifecycleState(unavailable)).toBe(
      ApplicationLifecycleState.UNAVAILABLE,
    );
    expect(projectApplicationDeploymentStatus(unavailable)).toEqual({
      label: "Unavailable",
      tone: "destructive",
    });
    expect(applicationDeploymentEndpoint(unavailable)).toBeUndefined();
  });

  it("marks a stale machine report on the card", () => {
    const stale = create(ApplicationDeploymentSchema, {
      ...deployment(ApplicationLifecycleState.READY, ApplicationReadinessState.READY),
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState: ApplicationLifecycleState.READY,
        observationStale: true,
        readinessState: ApplicationReadinessState.READY,
      }),
    });
    const tearingDown = create(ApplicationDeploymentSchema, {
      ...stale,
      desiredState: ApplicationDeploymentDesiredState.TEARDOWN,
    });

    expect(projectApplicationDeploymentStatus(stale)).toEqual({
      attention: "Machine stale",
      label: "Ready",
      tone: "warning",
    });
    expect(projectApplicationDeploymentStatus(tearingDown)).toEqual({
      attention: "Machine stale",
      label: "Tearing down",
      tone: "info",
    });
  });

  it("keeps teardown deployments visible until authoritative deletion", () => {
    const value = create(ApplicationDeploymentSchema, {
      desiredState: ApplicationDeploymentDesiredState.TEARDOWN,
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState: ApplicationLifecycleState.STOPPED,
        readinessState: ApplicationReadinessState.READY,
      }),
    });

    expect(applicationDeploymentLifecycleState(value)).toBe(ApplicationLifecycleState.STOPPED);
    expect(projectApplicationDeploymentStatus(value)).toEqual({
      label: "Tearing down",
      tone: "info",
    });
  });

  it("keeps paused deployments and immutable snapshots visible after catalog removal", () => {
    const snapshotApplication = create(ApplicationSchema, {
      applicationId: "historic-application",
      displayName: "Historic Application Snapshot",
    });
    const value = create(ApplicationDeploymentSchema, {
      applicationDeploymentId: "paused-deployment",
      desiredState: ApplicationDeploymentDesiredState.PAUSED,
      snapshot: resolvedSnapshot(snapshotApplication),
      state: create(ApplicationDeploymentStateSchema, {
        lifecycleState: ApplicationLifecycleState.PAUSED,
        readinessState: ApplicationReadinessState.READY,
        endpoint: { url: "http://100.64.0.7:49152" },
      }),
    });

    const projected = projectApplicationSnapshot([], [], [value], []);

    expect(projected.catalogApplications).toEqual([]);
    expect(projected.deployments).toEqual([value]);
    expect(projected.deployments[0]?.snapshot?.displayName).toBe("Historic Application Snapshot");
  });
});
