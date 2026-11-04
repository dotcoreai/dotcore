/**
 * The GPU devices an application's launches receive, as the detail rows show them: the device
 * identifiers in order, or "none" when no device is passed.
 */
export function formatGpuDevices(deviceIds: readonly string[]): string {
  return deviceIds.length ? deviceIds.join(", ") : "none";
}
