import { create } from "@bufbuild/protobuf";
import { render } from "@testing-library/react";
import { createRef } from "react";
import { describe, expect, it, vi } from "vitest";

import { DataTable } from "@/components/data-table/data-table";
import { buildDataTable } from "@/components/data-table/data-table-model";
import { projectApplicationDeploymentStatus } from "@/domain/application-projection";
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

import { projectApplicationDeploymentRows } from "./application-deployment-row";
import {
  applicationDeploymentTableDefinition,
  applicationDeploymentTableUi,
  applicationDeploymentViewCodec,
} from "./application-deployment-table";
import { DeploymentApplicationInspector } from "./application-inspector";

/**
 * A table row shows an aggregate status and an inspector shows the underlying states as separate
 * rows, so the two surfaces deliberately show different words for the same deployment. What they
 * may never do is give one state two different colours.
 *
 * This is the seam the console got wrong: the list projected a status while the inspector computed
 * its own tones from the raw fields, so a deployment reported up but not serving rendered a red
 * `Not ready` row that opened a green `Ready` panel.
 */

const application = create(ApplicationSchema, {
  applicationId: "open-webui",
  displayName: "OpenWebUI",
  version: "0.6.0",
});

function resolvedSnapshot(application: Application) {
  return {
    applicationId: application.applicationId,
    displayName: application.displayName,
    version: application.version,
    graph: application.graph,
  };
}

function deploymentIn(
  lifecycleState: ApplicationLifecycleState,
  readinessState: ApplicationReadinessState,
  desiredState = ApplicationDeploymentDesiredState.PRESENT,
  observationStale = false,
) {
  return create(ApplicationDeploymentSchema, {
    applicationDeploymentId: "deployment-1",
    applicationDeploymentName: "Team chat",
    desiredState,
    snapshot: resolvedSnapshot(application),
    state: create(ApplicationDeploymentStateSchema, {
      lifecycleState,
      observationStale,
      readinessState,
    }),
  });
}

/** Renders the Apps table with one row for the deployment. */
function renderRow(deployment: ReturnType<typeof deploymentIn>) {
  const view = applicationDeploymentViewCodec.defaultView;
  return render(
    <DataTable
      onViewChange={vi.fn()}
      table={buildDataTable(
        applicationDeploymentTableDefinition,
        projectApplicationDeploymentRows([deployment], {
          machines: [],
          resources: new Map(),
        }),
        view,
      )}
      ui={applicationDeploymentTableUi}
      view={view}
    />,
  );
}

/** Every status badge on screen, as the word it shows paired with the tone it carries. */
function badgeTones(): [string, string][] {
  return [...document.querySelectorAll('[data-slot="badge"]')].map((badge) => [
    badge.textContent ?? "",
    badge.getAttribute("data-variant") ?? "",
  ]);
}

const PRESENT = ApplicationDeploymentDesiredState.PRESENT;

const cases: [
  string,
  ApplicationLifecycleState,
  ApplicationReadinessState,
  ApplicationDeploymentDesiredState,
  boolean?,
][] = [
  ["ready and serving", ApplicationLifecycleState.READY, ApplicationReadinessState.READY, PRESENT],
  [
    "ready but not serving",
    ApplicationLifecycleState.READY,
    ApplicationReadinessState.NOT_READY,
    PRESENT,
  ],
  [
    "running and serving",
    ApplicationLifecycleState.RUNNING,
    ApplicationReadinessState.READY,
    PRESENT,
  ],
  [
    "running but not serving",
    ApplicationLifecycleState.RUNNING,
    ApplicationReadinessState.NOT_READY,
    PRESENT,
  ],
  [
    "unavailable",
    ApplicationLifecycleState.UNAVAILABLE,
    ApplicationReadinessState.UNSPECIFIED,
    PRESENT,
  ],
  ["failed", ApplicationLifecycleState.FAILED, ApplicationReadinessState.NOT_READY, PRESENT],
  ["starting", ApplicationLifecycleState.STARTING, ApplicationReadinessState.NOT_READY, PRESENT],
  [
    "paused",
    ApplicationLifecycleState.PAUSED,
    ApplicationReadinessState.UNSPECIFIED,
    ApplicationDeploymentDesiredState.PAUSED,
  ],
  [
    "tearing down",
    ApplicationLifecycleState.RUNNING,
    ApplicationReadinessState.READY,
    ApplicationDeploymentDesiredState.TEARDOWN,
  ],
  [
    "stale machine report",
    ApplicationLifecycleState.READY,
    ApplicationReadinessState.READY,
    PRESENT,
    true,
  ],
];

describe("application status tone consistency", () => {
  it.each(cases)(
    "gives every state one tone across the row and the inspector: %s",
    (_name, lifecycleState, readinessState, desiredState, observationStale) => {
      const deployment = deploymentIn(
        lifecycleState,
        readinessState,
        desiredState,
        observationStale,
      );
      const tonesByLabel = new Map<string, Set<string>>();

      const record = () => {
        for (const [label, tone] of badgeTones()) {
          const tones = tonesByLabel.get(label) ?? new Set<string>();
          tones.add(tone);
          tonesByLabel.set(label, tones);
        }
      };

      const row = renderRow(deployment);
      record();
      row.unmount();

      const panel = render(
        <DeploymentApplicationInspector
          deployment={deployment}
          onOpenChange={vi.fn()}
          onRemove={vi.fn()}
          open
          openerRef={createRef<HTMLElement>()}
          pending={false}
        />,
      );
      record();
      panel.unmount();

      // Both surfaces rendered something, so an empty map cannot pass this vacuously.
      expect(tonesByLabel.size).toBeGreaterThan(0);

      const conflicts = [...tonesByLabel]
        .filter(([, tones]) => tones.size > 1)
        .map(([label, tones]) => `${label}: ${[...tones].sort().join(" and ")}`);
      expect(conflicts).toEqual([]);
    },
  );

  it("shows the row aggregate as the panel State, with no separate readiness badge", () => {
    const deployment = deploymentIn(
      ApplicationLifecycleState.READY,
      ApplicationReadinessState.NOT_READY,
    );

    const row = renderRow(deployment);
    const rowBadges = badgeTones();
    row.unmount();

    render(
      <DeploymentApplicationInspector
        deployment={deployment}
        onOpenChange={vi.fn()}
        onRemove={vi.fn()}
        open
        openerRef={createRef<HTMLElement>()}
        pending={false}
      />,
    );

    // One badge carries the aggregate: a ready lifecycle with an unready deployment reads as the
    // readiness that contradicts it, in the row and as the panel's State.
    expect(rowBadges).toEqual([["Not ready", "destructive"]]);
    expect(badgeTones()).toEqual(rowBadges);
  });

  it("colours the row aggregate from the state it chose to show", () => {
    const notServing = projectApplicationDeploymentStatus(
      deploymentIn(ApplicationLifecycleState.READY, ApplicationReadinessState.NOT_READY),
    );
    expect(notServing).toMatchObject({ label: "Not ready", tone: "destructive" });

    const unavailable = projectApplicationDeploymentStatus(
      deploymentIn(ApplicationLifecycleState.UNAVAILABLE, ApplicationReadinessState.UNSPECIFIED),
    );
    expect(unavailable).toMatchObject({ label: "Unavailable", tone: "destructive" });
  });
});
