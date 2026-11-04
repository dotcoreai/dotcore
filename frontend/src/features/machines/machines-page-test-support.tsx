/* eslint-disable react-refresh/only-export-components -- test support exports a wrapper and its helpers */
import { create } from "@bufbuild/protobuf";
import { act } from "@testing-library/react";
import { type ComponentProps, useState } from "react";

import type { ApplicationClient } from "@/api/application-client";
import type { ModelClient } from "@/api/model-client";
import type { SiteClient } from "@/api/site-client";
import { ResourceWatchProvider } from "@/app/resource-watch-provider";
import { nodeMemoryPayload } from "@/features/memory/node-memory-test-support";
import { ListApplicationDeploymentsResponseSchema } from "@/gen/dotcore/application/v1/service_pb";
import { ListModelDeploymentsResponseSchema } from "@/gen/dotcore/model/v1/service_pb";
import { FakeResourceWatch, type ResourceSources } from "@/test/fake-resource-watch";

import { MachinesPage as RealMachinesPage } from "./machines-page";

const watches: FakeResourceWatch[] = [];

/**
 * The site reads the Machines page makes, with the machines and hardware snapshots it watches
 * held as the payloads the manager sends each tick.
 */
export type MachinesSite = SiteClient & {
  watched: Required<Pick<ResourceSources, "hardwareSnapshots" | "machines">> &
    Pick<ResourceSources, "nodeMemory">;
};

/** Reads the latest page's sources again and sends them, like the manager's next tick. */
export async function tick() {
  await act(async () => {
    await watches.at(-1)?.tick();
  });
}

/**
 * The Machines page under a resource watch answering from the given site's watched payloads and
 * the clients' list reads, read again every manager tick like the management API does. Deployment
 * lists and node memory default to empty.
 */
export function MachinesPage({
  applicationClient,
  client,
  metricsClient,
  modelClient,
}: {
  applicationClient?: ApplicationClient;
  client: MachinesSite;
  metricsClient?: ComponentProps<typeof RealMachinesPage>["metricsClient"];
  modelClient?: ModelClient;
}) {
  const [watch] = useState(() => {
    const watch = new FakeResourceWatch({
      sources: {
        applicationDeployments: () =>
          applicationClient
            ? applicationClient.application.listApplicationDeployments({})
            : Promise.resolve(create(ListApplicationDeploymentsResponseSchema)),
        executionTargets: () => client.listConfiguredExecutionTargets({}),
        hardwareSnapshots: () => client.watched.hardwareSnapshots(),
        machines: () => client.watched.machines(),
        modelDeployments: () =>
          modelClient
            ? modelClient.model.listModelDeployments({})
            : Promise.resolve(create(ListModelDeploymentsResponseSchema)),
        nodeMemory: () => client.watched.nodeMemory?.() ?? Promise.resolve(nodeMemoryPayload([])),
      },
      tickMs: 1_000,
    });
    watches.push(watch);

    return watch;
  });

  return (
    <ResourceWatchProvider client={watch.client()} reopenDelayMs={0}>
      <RealMachinesPage metricsClient={metricsClient} />
    </ResourceWatchProvider>
  );
}
