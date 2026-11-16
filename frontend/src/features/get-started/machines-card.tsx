import { Plus } from "lucide-react";

import { MetricRows } from "@/components/metrics/metric-row/metric-row";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { MachinesSectionIcon } from "@/components/ui/section-icons";
import { StatusBadge } from "@/components/ui/status-badge";
import { nodeConnectionStatus } from "@/domain/execution-target";
import { machineName } from "@/domain/machine-name";
import { MemoryMetricRows } from "@/features/memory/memory-rows";
import { pastTotalCapacity } from "@/features/memory/pool-spans";
import { rowsFromSpans, type SiteMemory } from "@/features/memory/site-memory";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";

/**
 * The site's machines: each one's connection status and its memory, one row per GPU and one for
 * system memory, with the models and apps using it. Adding a machine opens its steps.
 */
export function MachinesCard({
  machines,
  memory,
  onAddMachine,
}: {
  machines: readonly Machine[];
  /** The site's memory; absent while it is read or when it can't be. */
  memory: SiteMemory | undefined;
  onAddMachine: () => void;
}) {
  return (
    <Card aria-labelledby="get-started-machines-heading" className="px-5 py-4.5" role="region">
      <div className="flex items-center gap-2">
        <h2
          className="flex items-center gap-2 text-sm font-semibold"
          id="get-started-machines-heading"
        >
          <MachinesSectionIcon aria-hidden="true" className="size-4" />
          Machines
        </h2>
        <Button
          className="ml-auto"
          onClick={onAddMachine}
          size="sm"
          type="button"
          variant="outline"
        >
          <Plus aria-hidden="true" data-icon="inline-start" />
          Add a machine
        </Button>
      </div>
      <div className="grid gap-4">
        {machines.map((machine) => (
          <MachineMemory key={machine.machineId} machine={machine} memory={memory} />
        ))}
      </div>
    </Card>
  );
}

function MachineMemory({ machine, memory }: { machine: Machine; memory: SiteMemory | undefined }) {
  const name = machineName(machine);
  const own = memory?.machines.get(machine.machineId);
  const devices = [...(own?.deviceSpans.values() ?? [])];
  const deviceRows = devices.map((span) => rowsFromSpans([span]));
  const ramRows = rowsFromSpans(own?.spans ?? []).filter((row) => row.kind === "ram");
  const unavailable = own?.unavailable ?? [];

  return (
    <div aria-label={name} className="grid gap-2" role="group">
      <div className="flex items-center justify-between gap-2">
        <span className="inline-flex items-center text-sm font-semibold">
          <ResourceMarker colorKey={machine.machineId} />
          {name}
        </span>
        <StatusBadge status={nodeConnectionStatus(machine.observationFreshness?.state)} />
      </div>
      <MetricRows fitLabels>
        {deviceRows.map((rows, index) => (
          <MemoryMetricRows
            capacity={pastTotalCapacity(rows)}
            deviceLabel={devices[index].devices[0]?.label}
            expanded={() => false}
            key={devices[index].devices[0]?.label ?? index}
            // Each GPU names itself only when the machine has several.
            label={() => (devices.length > 1 ? `VRAM ${index + 1}` : "VRAM")}
            onExpandedChange={() => {}}
            rows={rows}
          >
            {() => null}
          </MemoryMetricRows>
        ))}
        <MemoryMetricRows
          capacity={pastTotalCapacity(ramRows)}
          expanded={() => false}
          kinds={["ram"]}
          onExpandedChange={() => {}}
          rows={ramRows}
          unavailable={unavailable}
        >
          {() => null}
        </MemoryMetricRows>
      </MetricRows>
    </div>
  );
}
