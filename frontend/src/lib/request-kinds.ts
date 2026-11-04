import { InferenceRequestKind } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";

const kindLabels: ReadonlyMap<InferenceRequestKind, string> = new Map([
  [InferenceRequestKind.CHAT, "Chat"],
  [InferenceRequestKind.DECISION, "Decisions"],
]);

/** The request kinds served by a model or route, as "Chat", "Decisions" or "Chat, Decisions"; empty when none is declared. */
export function requestKindsLabel(kinds: readonly InferenceRequestKind[]): string {
  return [...kindLabels]
    .filter(([kind]) => kinds.includes(kind))
    .map(([, label]) => label)
    .join(", ");
}

/** Whether the kinds include chat, the only kind applications and the chat-only API surfaces can use. */
export function servesChat(kinds: readonly InferenceRequestKind[]): boolean {
  return kinds.includes(InferenceRequestKind.CHAT);
}

/** The distinct request kinds across several declarations, in enum order. */
export function mergeRequestKinds(
  lists: readonly (readonly InferenceRequestKind[])[],
): InferenceRequestKind[] {
  const merged = new Set(lists.flat());
  return [...kindLabels.keys()].filter((kind) => merged.has(kind));
}
