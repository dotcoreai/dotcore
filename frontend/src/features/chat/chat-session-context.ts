import { createContext, useContext } from "react";

import type { ChatMode } from "@/app/chat-opener";
import type { ComboboxOption } from "@/components/ui/combobox";
import type { Decisions } from "@/features/chat/decisions/use-decisions";
import type { Chat } from "@/features/chat/use-chat";

/**
 * The route side of the console chat session, shared by the chat panel and the Chat page. A chosen
 * route that is no longer available stays chosen and is reported as unavailable, so the chat never
 * sends to a route the operator did not choose.
 */
export interface ChatRouteSession {
  /** The route the operator picked or asked for; null lets the chat use the first available one. */
  chosenRoute: string | null;
  /** The route requests go to; absent while the chosen route is unavailable or none is available. */
  route: string | undefined;
  chosenUnavailable: boolean;
  /** Whether the route list is still loading long enough to show it. */
  loading: boolean;
  /** Whether the route list is read and offers nothing to talk to. */
  noRoute: boolean;
  /** Available routes as picker options naming their kinds, led by an unavailable chosen route. */
  pickerOptions: ComboboxOption[];
  selectRoute: (route: string | null) => void;
  /** What the chat shows for the selected route, from the kinds it serves. */
  mode: ChatMode;
  /** Whether the selected route serves both kinds, so the operator switches the mode. */
  modeSwitchable: boolean;
  /** Switches the selected route's mode; it is kept for that route for the session. */
  selectMode: (mode: ChatMode) => void;
  /** Leaves the Chat page for the previous console page, with the chat panel open on the route. */
  showAsPanel: () => void;
}

export const ChatRouteSessionContext = createContext<ChatRouteSession | null>(null);
export const ChatConversationContext = createContext<Chat | null>(null);
export const ChatDecisionsContext = createContext<Decisions | null>(null);

/** The route side of the chat session; only inside the chat session provider. */
export function useChatRouteSession(): ChatRouteSession {
  const session = useContext(ChatRouteSessionContext);
  if (!session) throw new Error("useChatRouteSession needs a ChatSessionProvider");
  return session;
}

/** The conversation of the chat session; only inside the chat session provider. */
export function useChatConversation(): Chat {
  const chat = useContext(ChatConversationContext);
  if (!chat) throw new Error("useChatConversation needs a ChatSessionProvider");
  return chat;
}

/** The decision draft and last Ask of the chat session; only inside the chat session provider. */
export function useChatDecisions(): Decisions {
  const decisions = useContext(ChatDecisionsContext);
  if (!decisions) throw new Error("useChatDecisions needs a ChatSessionProvider");
  return decisions;
}
