import { Code } from "lucide-react";

import type { InspectorAction } from "@/components/inspector-actions/inspector-actions";
import type { ConnectTarget } from "@/features/chat/chattable-route";

/**
 * The Connect action of a model deployment's inspector: it opens the connect page of the target's
 * route, or is disabled with the reason no route can be used. Absent without a target.
 */
export function deploymentConnectAction(
  target: ConnectTarget | undefined,
  onConnect: ((routeId: string) => void) | undefined,
  pending: boolean | undefined,
): InspectorAction | undefined {
  if (!target || !onConnect) return undefined;

  if (target.routeId !== undefined) {
    const { routeId } = target;
    return { disabled: pending, icon: Code, label: "Connect", onSelect: () => onConnect(routeId) };
  }
  return {
    disabled: true,
    disabledReason: target.unavailableReason,
    icon: Code,
    label: "Connect",
    onSelect: () => {},
  };
}
