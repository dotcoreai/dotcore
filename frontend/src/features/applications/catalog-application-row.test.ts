import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import {
  ApplicationDeploymentDesiredState,
  ApplicationDeploymentSchema,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  type Application,
  ApplicationSchema,
  type ApplicationService,
  ApplicationServiceSchema,
} from "@/gen/dotcore/catalog/v1/application_pb";

import {
  type CatalogApplicationRow,
  projectCatalogApplicationRows,
} from "./catalog-application-row";

function application(
  applicationId: string,
  services: ApplicationService[],
  version = "1.0",
): Application {
  return create(ApplicationSchema, {
    applicationId,
    displayName: applicationId.toUpperCase(),
    graph: { services },
    version,
  });
}

function service(
  serviceName: string,
  platforms: { architecture: string; variant?: string }[] = [],
  bindings: Pick<
    MessageInitShape<typeof ApplicationServiceSchema>,
    "configurationBindings" | "secretBindings" | "storageBindings"
  > = {},
): ApplicationService {
  return create(ApplicationServiceSchema, {
    artifact: {
      supportedPlatforms: platforms.map((platform) => ({ operatingSystem: "linux", ...platform })),
    },
    serviceName,
    ...bindings,
  });
}

const routeBinding = {
  bindingName: "route",
  input: { kind: { case: "inferenceRoute" as const, value: {} } },
};

function deployment(applicationId: string, desiredState: ApplicationDeploymentDesiredState) {
  return create(ApplicationDeploymentSchema, {
    applicationDeploymentId: `${applicationId}-${desiredState}`,
    desiredState,
    snapshot: { applicationId },
  });
}

function row(
  input: Application,
  deployments: ReturnType<typeof deployment>[] = [],
): CatalogApplicationRow {
  return projectCatalogApplicationRows({ applications: [input], deployments })[0];
}

describe("projectCatalogApplicationRows", () => {
  it("keeps the platforms every service supports, comparing the variant as part of the platform", () => {
    const projected = row(
      application("multi", [
        service("web", [
          { architecture: "amd64" },
          { architecture: "arm64", variant: "v8" },
          { architecture: "arm64" },
          { architecture: "riscv64" },
        ]),
        service("worker", [
          { architecture: "arm64", variant: "v8" },
          { architecture: "amd64" },
          { architecture: "riscv64" },
        ]),
        service("db", [
          { architecture: "amd64" },
          { architecture: "arm64", variant: "v8" },
          { architecture: "arm64" },
        ]),
      ]),
    );

    expect(projected).toEqual({
      applicationId: "multi",
      colorKey: "multi",
      displayName: "MULTI",
      platforms: ["linux/amd64", "linux/arm64/v8"],
      uses: [],
      runningCount: 0,
      runningStatus: undefined,
      serviceCount: 3,
      version: "1.0",
    });
  });

  it("has no platforms when the services share none", () => {
    const projected = row(
      application("split", [
        service("web", [{ architecture: "amd64" }]),
        service("worker", [{ architecture: "arm64" }]),
      ]),
    );

    expect(projected.platforms).toEqual([]);
  });

  it("uses the union of every service's inference route and storage bindings", () => {
    const projected = row(
      application("both", [
        service("web", [], { configurationBindings: [routeBinding] }),
        service("db", [], { storageBindings: [{ bindingName: "data", mountPath: "/data" }] }),
      ]),
    );

    expect(projected.uses).toEqual(["inference-route", "persistent-storage"]);
  });

  it("uses nothing for a secret or plain bindings", () => {
    expect(
      row(
        application("secret", [
          service("web", [], {
            secretBindings: [{ bindingName: "token", environmentVariable: "TOKEN" }],
          }),
        ]),
      ).uses,
    ).toEqual([]);
    expect(
      row(
        application("plain", [
          service("web", [], {
            configurationBindings: [
              { bindingName: "mode", source: { case: "literalValue", value: "on" } },
            ],
          }),
        ]),
      ).uses,
    ).toEqual([]);
  });

  it("carries the description", () => {
    const projected = row(
      create(ApplicationSchema, {
        applicationId: "described",
        description: "Chat interface for local models.",
        displayName: "Described",
      }),
    );

    expect(projected.description).toBe("Chat interface for local models.");
  });

  it("leaves a missing description, version, and graph absent", () => {
    const projected = row(
      create(ApplicationSchema, { applicationId: "bare", displayName: "Bare" }),
    );

    expect(projected).toEqual({
      applicationId: "bare",
      colorKey: "bare",
      displayName: "Bare",
      platforms: [],
      uses: [],
      runningCount: 0,
      runningStatus: undefined,
      serviceCount: 0,
    });
  });

  it("counts running and paused application deployments of the application but not torn-down ones", () => {
    const projected = row(application("web", [service("web")]), [
      deployment("web", ApplicationDeploymentDesiredState.PRESENT),
      deployment("web", ApplicationDeploymentDesiredState.PAUSED),
      deployment("web", ApplicationDeploymentDesiredState.TEARDOWN),
      deployment("other", ApplicationDeploymentDesiredState.PRESENT),
    ]);

    expect(projected.runningCount).toBe(2);
    expect(projected.runningStatus).toEqual({ label: "2 deployed", tone: "success" });
  });
});
