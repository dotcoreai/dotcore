import { servesChat } from "@/domain/request-kinds";
import type { ChatMode } from "@/features/chat/chat-opener";
import { InferenceRequestKind } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";

/** The inspector action's label: "Ask" for decisions only, "Chat" whenever chat is served. */
export type ChatActionLabel = "Chat" | "Ask";

/**
 * Where a Chat or Ask action takes the operator: a route the console chat can use, in the mode
 * matching the kinds it is used for, or why there is none.
 */
export type ChatTarget =
  | { label: ChatActionLabel; mode: ChatMode; route: string }
  | { label: ChatActionLabel; route?: undefined; unavailableReason: string };

/** Whether the console chat can use a route: it is enabled and currently available. */
export function isChattableRoute(view: InferenceRouteView): boolean {
  return Boolean(view.route?.enabled && view.availability?.available);
}

/**
 * The console chat mode for a route's request kinds: decisions only when it serves decisions and
 * not chat, otherwise chat, the mode routes that declare nothing get.
 */
export function chatModeForKinds(kinds: readonly InferenceRequestKind[]): ChatMode {
  return !servesChat(kinds) && kinds.includes(InferenceRequestKind.DECISION) ? "decision" : "chat";
}

/** The target of a route's own inspector: the route itself when available. */
export function routeChatTarget(view: InferenceRouteView): ChatTarget {
  const mode = chatModeForKinds(view.requestKinds);
  const label = actionLabel(mode);
  if (!view.route?.enabled) return { label, unavailableReason: enableReason(mode) };
  if (!isChattableRoute(view)) return { label, unavailableReason: "The route is unavailable." };
  return { label, mode, route: view.route.name };
}

/**
 * The target of a model deployment: the first available route listing it as a member, used for
 * the kinds both the deployment and the route serve. A deployment that declares no kinds takes
 * the route's.
 */
export function deploymentChatTarget(
  routes: readonly InferenceRouteView[],
  modelDeploymentId: string,
  deploymentKinds: readonly InferenceRequestKind[],
): ChatTarget {
  const declaredLabel = actionLabel(chatModeForKinds(deploymentKinds));
  const memberOf = routes.filter((view) =>
    view.route?.fallbackGroups.some((group) =>
      group.modelDeploymentIds.includes(modelDeploymentId),
    ),
  );
  if (memberOf.length === 0) {
    return {
      label: declaredLabel,
      unavailableReason: `Add the deployment to a route to ${declaredLabel === "Ask" ? "ask" : "chat with"} it.`,
    };
  }

  const available = memberOf.find(isChattableRoute);
  if (!available) {
    return {
      label: declaredLabel,
      unavailableReason: "None of the deployment's routes is available.",
    };
  }

  const shared = deploymentKinds.length
    ? available.requestKinds.filter((kind) => deploymentKinds.includes(kind))
    : available.requestKinds;
  const mode = chatModeForKinds(shared);
  return { label: actionLabel(mode), mode, route: available.route!.name };
}

function actionLabel(mode: ChatMode): ChatActionLabel {
  return mode === "decision" ? "Ask" : "Chat";
}

function enableReason(mode: ChatMode): string {
  return mode === "decision" ? "Enable the route to ask it." : "Enable the route to chat with it.";
}
