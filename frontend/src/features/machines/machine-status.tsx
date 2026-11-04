import { StatusBadge } from "@/components/ui/status-badge";

/** A reading the machine reports as unavailable, such as memory it could not measure. */
export function UnavailableBadge() {
  return <StatusBadge status={{ label: "Unavailable", tone: "neutral" }} />;
}
