import { createContext, useContext } from "react";

/** What the console chat shows for a route: the conversation or the decision controls. */
type ChatMode = "chat" | "decision";

/**
 * Opens the console chat with the named route selected, in the given mode when the route serves
 * both kinds; without a mode, the route keeps the mode it had in this session.
 */
type ChatOpener = (routeName: string, mode?: ChatMode) => void;

const ChatOpenerContext = createContext<ChatOpener | null>(null);

/** The console chat opener, or null outside the console layout, where no chat exists. */
function useChatOpener(): ChatOpener | null {
  return useContext(ChatOpenerContext);
}

export { ChatOpenerContext, useChatOpener };
export type { ChatMode, ChatOpener };
