import {
  type PanelEntry,
  type PanelLocation,
  parsePanelStack,
  selectPanelLocation,
} from "@/panels/panel-stack";

/** The id of the machine sidebar, for the controls that open it. */
export const machineInspectorPanelId = "machine-inspector-panel";

/**
 * The machine a machine sidebar shows at `search`: the active machine panel, or the machine
 * beneath an active execution target.
 */
export function inspectedMachineId(search: string): string | undefined {
  const stack = parsePanelStack(search);
  const active = stack.at(-1);
  if (active?.page !== "machines") return undefined;

  const machine = active.screen === "execution-target" ? stack.at(-2) : active;
  return machine?.page === "machines" && machine.screen === "machine"
    ? machine.resourceId
    : undefined;
}

/** Location opening `machineId`'s sidebar from `location`, on the page hosting it. */
export function machineSelectionLocation(
  location: PanelLocation,
  machineId: string,
): PanelLocation {
  return selectPanelLocation(location, machineEntry(machineId));
}

/** The panel entry of `machineId`'s sidebar. */
export function machineEntry(machineId: string): PanelEntry {
  return { page: "machines", resourceId: machineId, screen: "machine" };
}
