import { describe, expect, it } from "vitest";

import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";

import { applicationDeploymentPresentationName } from "./application-deployment-name";

function deployment(overrides: Partial<ApplicationDeployment>): ApplicationDeployment {
  return {
    applicationDeploymentId: "deployment-1",
    applicationDeploymentName: "deployment-1",
    snapshot: { applicationId: "app-1", displayName: "Chatbot" },
    ...overrides,
  } as ApplicationDeployment;
}

describe("applicationDeploymentPresentationName", () => {
  it("returns the caller-supplied name", () => {
    const subject = deployment({ applicationDeploymentName: "Support bot" });
    expect(applicationDeploymentPresentationName(subject)).toBe("Support bot");
  });

  it("falls back to the display name when the name is the deployment ID", () => {
    const subject = deployment({ applicationDeploymentName: "deployment-1" });
    expect(applicationDeploymentPresentationName(subject)).toBe("Chatbot");
  });

  it("falls back to the display name when the name is the auto-derived ID", () => {
    const subject = deployment({ applicationDeploymentName: "app-1-1" });
    expect(applicationDeploymentPresentationName(subject)).toBe("Chatbot");
  });

  it("falls back to a generic label when the snapshot is missing", () => {
    const subject = deployment({
      applicationDeploymentName: "deployment-1",
      snapshot: undefined,
    });
    expect(applicationDeploymentPresentationName(subject)).toBe("Application");
  });
});
