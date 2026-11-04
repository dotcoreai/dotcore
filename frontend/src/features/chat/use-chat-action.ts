import { useChatOpener } from "@/app/chat-opener";
import { ChatSectionIcon } from "@/app/console-section-icons";
import type { InspectorAction } from "@/components/inspector-actions/inspector-actions";

import type { ChatTarget } from "./chattable-route";

/**
 * The inspector Chat or Ask action for a target: it opens the console chat on the target's route
 * in the target's mode, or is disabled with the reason no route can be used. Absent without a
 * target or outside the console layout.
 */
export function useChatAction(target: ChatTarget | undefined): InspectorAction | undefined {
  const openChat = useChatOpener();
  if (!openChat || !target) return undefined;

  if (target.route !== undefined) {
    const { mode, route } = target;
    return { icon: ChatSectionIcon, label: target.label, onSelect: () => openChat(route, mode) };
  }
  return {
    disabled: true,
    disabledReason: target.unavailableReason,
    icon: ChatSectionIcon,
    label: target.label,
    onSelect: () => {},
  };
}
