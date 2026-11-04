import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import { type NodeMemory, NodeMemoryState } from "@/gen/dotcore/capacity/v1/memory_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";

import { deploymentRefs, type PoolOwner, poolSpans } from "./pool-spans";
import {
  type MachineMemory,
  type MemoryMachine,
  rowsFromSpans,
  type SiteMemory,
  type UnmeasuredReason,
} from "./site-memory";
import { measurementGap } from "./unavailable-memory-codes";

/** The reads the site's memory draws from: the capacity component's node memory and the names. */
export interface SiteMemoryInputs {
  /** Application deployments, which name application segments. */
  applications?: readonly ApplicationDeployment[];
  /** Model deployments, which name model segments and say why a deployment is unsized. */
  deployments: readonly ModelDeployment[];
  /** The machines to draw, with their names; a node not listed here is left out. */
  machines: readonly MemoryMachine[];
  /** Every site node's memory, in site order. */
  nodes: readonly NodeMemory[];
  /** Whether the latest node memory or deployment read failed, so drawn values may be outdated. */
  readFailed: boolean;
}

/**
 * The site's memory as the capacity component draws it, for the listed machines in site order. A
 * node without a memory reading contributes no span and is listed as unavailable; a stale one
 * draws its totals and claims, marked stale.
 */
export function projectSiteMemory(inputs: SiteMemoryInputs): SiteMemory {
  const byId = new Map(inputs.machines.map((machine) => [machine.machineId, machine]));
  const machines = new Map<string, MachineMemory>();
  const unavailable: SiteMemory["unavailable"] = [];

  for (const node of inputs.nodes) {
    const machine = byId.get(node.siteNodeId);
    if (!machine) continue;

    const memory = projectNodeMemory(node, machine, inputs);
    machines.set(machine.machineId, memory);
    unavailable.push(...memory.unavailable);
  }

  const spans = [...machines.values()].flatMap((memory) => memory.spans);
  return {
    machines,
    rows: rowsFromSpans(spans),
    stale: inputs.readFailed || spans.some((span) => span.stale),
    unavailable,
  };
}

/** One node's memory as spans for its machine, named after the site's deployments. */
export function projectNodeMemory(
  node: NodeMemory,
  machine: MemoryMachine,
  {
    applications = [],
    deployments,
    readFailed,
  }: Pick<SiteMemoryInputs, "applications" | "deployments" | "readFailed">,
): MachineMemory {
  if (node.state === NodeMemoryState.UNAVAILABLE || !node.pools.length) {
    return { deviceSpans: new Map(), spans: [], unavailable: [{ deployments: [], machine }] };
  }

  const stale = node.state === NodeMemoryState.STALE;
  return poolSpans({
    machine,
    observedAt: node.observedAt,
    pools: node.pools.flatMap(({ pool, unsized }) => (pool ? [{ pool, unsized }] : [])),
    refs: deploymentRefs({ applications, deployments }),
    stale: stale || readFailed,
    unsizedReason: stale ? () => "stale" : unsizedReason(deployments, applications),
  });
}

/**
 * Why an unsized deployment has no drawn size, from its own observation: a lifecycle not ready
 * yet, or for a ready deployment, a measurement without a reading or without a result, or nothing
 * to size it. A deployment not listed yet has just been created, so it reads as not ready.
 */
function unsizedReason(
  deployments: readonly ModelDeployment[],
  applications: readonly ApplicationDeployment[],
): (owner: PoolOwner) => UnmeasuredReason {
  const models = new Map(deployments.map((entry) => [entry.modelDeploymentId, entry]));
  const apps = new Map(applications.map((entry) => [entry.applicationDeploymentId, entry]));

  return (owner) => {
    if (owner.case === "modelDeploymentId") {
      const deployment = models.get(owner.value);
      // A deployment still starting is measured only once it holds its memory.
      if (deployment?.state?.status !== ModelDeploymentStatus.READY) return "notReady";

      // A ready deployment without a measurement result reads as an unavailable measurement.
      const result = deployment.acceleratorMemory?.result;
      if (result?.case === "unavailable" || result?.case === undefined) {
        return measurementGap(result?.case === "unavailable" ? result.value.code : undefined);
      }
      return "noEstimate";
    }
    if (owner.case === "applicationDeploymentId") {
      const unavailable = apps
        .get(owner.value)
        ?.serviceAcceleratorMemory.map((unit) => unit.acceleratorMemory?.result)
        .find((result) => result?.case === "unavailable");
      return unavailable?.case === "unavailable"
        ? measurementGap(unavailable.value.code)
        : "pending";
    }

    return "notReady";
  };
}
