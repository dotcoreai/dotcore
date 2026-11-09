import { describe, expect, it } from "vitest";

import {
  applicationDeploymentDesiredStateTone,
  applicationLifecycleLabel,
  applicationLifecycleTone,
  applicationReadinessLabel,
  applicationReadinessTone,
} from "@/domain/application-projection";
import {
  modelDeploymentDesiredStateTone,
  modelStatusLabel,
  modelStatusTone,
} from "@/domain/model-projection";
import { ApplicationDeploymentDesiredState } from "@/gen/dotcore/application/v1/deployment_pb";
import {
  ApplicationLifecycleState,
  ApplicationReadinessState,
} from "@/gen/dotcore/application/v1/state_pb";
import { ModelDeploymentDesiredState } from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";

/**
 * Every word a status badge is allowed to show. A card renders the label and nothing else, so this
 * set is the whole vocabulary a reader ever sees in a badge.
 */
const VOCABULARY = new Set([
  "Available",
  "Disabled",
  "Downloading",
  "Failed",
  "Loading",
  "Not ready",
  "Paused",
  "Pausing",
  "Preparing",
  "Queued",
  "Ready",
  "Running",
  "Starting",
  "Stopped",
  "Stopping",
  "Tearing down",
  "Unavailable",
  "Unknown",
  "Warming up",
]);

/** Every value of a numeric protobuf enum, excluding its reverse name mapping. */
function enumValues(enumObject: Record<string, unknown>): number[] {
  return Object.values(enumObject).filter((value): value is number => typeof value === "number");
}

describe("status vocabulary", () => {
  const cases: [string, (value: number) => string, Record<string, unknown>][] = [
    ["model deployment status", modelStatusLabel, ModelDeploymentStatus],
    ["application lifecycle", applicationLifecycleLabel, ApplicationLifecycleState],
    ["application readiness", applicationReadinessLabel, ApplicationReadinessState],
  ];

  it.each(cases)("draws every %s label from the shared vocabulary", (_name, label, enumObject) => {
    const labels = enumValues(enumObject).map(label);

    expect(labels.length).toBeGreaterThan(0);
    for (const value of labels) expect(VOCABULARY).toContain(value);
  });

  it.each(cases)("builds no %s label by concatenation", (_name, label, enumObject) => {
    for (const value of enumValues(enumObject).map(label)) {
      // A badge carries the status and nothing qualifying it: no progress figure, no fused second
      // value, no appended detail. Detail reaches the reader through the tooltip and the inspector.
      expect(value).not.toMatch(/\d/);
      expect(value).not.toMatch(/[·:(]|\s-\s/);
    }
  });
});

/**
 * A tone is chosen in exactly one place per state domain, so that one state reads the same colour on
 * a card, which shows an aggregate, and in an inspector, which shows that state on its own row.
 */
describe("status tones", () => {
  const TONES = new Set(["success", "info", "neutral", "destructive"]);

  const domains: [string, (value: number) => string, Record<string, unknown>][] = [
    ["model deployment status", modelStatusTone, ModelDeploymentStatus],
    ["application lifecycle", applicationLifecycleTone, ApplicationLifecycleState],
    ["application readiness", applicationReadinessTone, ApplicationReadinessState],
    ["model deployment target", modelDeploymentDesiredStateTone, ModelDeploymentDesiredState],
    [
      "application deployment target",
      applicationDeploymentDesiredStateTone,
      ApplicationDeploymentDesiredState,
    ],
  ];

  it.each(domains)("gives every %s state exactly one known tone", (_name, tone, enumObject) => {
    const values = enumValues(enumObject);

    expect(values.length).toBeGreaterThan(0);
    for (const value of values) {
      expect(TONES).toContain(tone(value));
      // A tone is a pure function of its state: the same state cannot yield two colours.
      expect(tone(value)).toBe(tone(value));
    }
  });

  it("treats an unavailable deployment as a fault in both domains, not an intentional pause", () => {
    // The proto defines it as a deployment unavailable for controller operation, so it is toned as a
    // fault wherever it appears rather than sharing the paused states' neutral.
    expect(modelStatusTone(ModelDeploymentStatus.UNAVAILABLE)).toBe("destructive");
    expect(applicationLifecycleTone(ApplicationLifecycleState.UNAVAILABLE)).toBe("destructive");
  });

  it("treats a deployment that is up but not serving as a fault", () => {
    expect(applicationReadinessTone(ApplicationReadinessState.NOT_READY)).toBe("destructive");
    expect(applicationReadinessTone(ApplicationReadinessState.READY)).toBe("success");
  });

  it("tones a deployment target only when it is teardown, since a target is an intent", () => {
    expect(modelDeploymentDesiredStateTone(ModelDeploymentDesiredState.TEARDOWN)).toBe("info");
    expect(modelDeploymentDesiredStateTone(ModelDeploymentDesiredState.PRESENT)).toBe("neutral");
    expect(applicationDeploymentDesiredStateTone(ApplicationDeploymentDesiredState.TEARDOWN)).toBe(
      "info",
    );
    expect(applicationDeploymentDesiredStateTone(ApplicationDeploymentDesiredState.PRESENT)).toBe(
      "neutral",
    );
  });
});
