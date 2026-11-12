import { Maximize2, MessageSquarePlus, XIcon } from "lucide-react";
import type { KeyboardEvent } from "react";
import { useEffect, useRef } from "react";
import { useNavigate } from "react-router-dom";

import { useSidebar } from "@/components/ui/sidebar";
import { useChatConversation, useChatRouteSession } from "@/features/chat/chat-session-context";
import {
  ChatHeaderButton,
  ChatModeSwitch,
  ChatRoutePicker,
  ChatSessionBody,
  ClearAnswersButton,
} from "@/features/chat/chat-view";
import { useIsMobile } from "@/hooks/use-mobile";
import { cn } from "@/lib/utils";

interface ChatPanelProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/**
 * The console chat as a floating panel: a view of the chat session with a route picker and the
 * conversation or the decision controls. It stays mounted while closed so its message box keeps its draft, and expands to
 * the Chat page on the same route.
 */
export function ChatPanel({ onOpenChange, open }: ChatPanelProps) {
  const isMobile = useIsMobile();
  const collapsed = useSidebar().state === "collapsed";
  const navigate = useNavigate();
  const session = useChatRouteSession();
  const chat = useChatConversation();
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (open) textareaRef.current?.focus();
  }, [open]);

  const close = () => onOpenChange(false);
  const newChat = () => {
    chat.reset();
    textareaRef.current?.focus();
  };
  const expand = () => {
    const route = session.chosenRoute ?? session.route;
    void navigate(route === undefined ? "/chat" : `/chat?${new URLSearchParams({ route })}`);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    // An open route list handles its own Escape.
    const popupOpen =
      event.target instanceof Element && event.target.closest('[aria-expanded="true"]');
    if (event.key === "Escape" && !event.defaultPrevented && !popupOpen) close();
  };

  return (
    <>
      {/* Escape dismisses this non-modal dialog, so the key handler sits on the dialog itself. */}
      {/* eslint-disable-next-line jsx-a11y/no-noninteractive-element-interactions */}
      <section
        aria-label="Chat"
        data-chat-mode={session.mode}
        hidden={!open}
        aria-modal="false"
        className={cn(
          "fixed z-20 flex flex-col overflow-hidden border bg-background shadow-lg",
          "bottom-3 h-[560px] max-h-[calc(100dvh-var(--header-height)---spacing(6))] w-[400px] rounded-xl",
          collapsed
            ? "left-[calc(var(--sidebar-width-icon)+--spacing(3))]"
            : "left-[calc(var(--sidebar-width)+--spacing(3))]",
          // The sidebar leaves the layout below md, so the panel anchors to the page edge. Bounded
          // below so it can't override the full-screen `inset-0`, which Tailwind emits earlier.
          "min-[721px]:max-md:left-3",
          // Full screen up to the inspector's narrow breakpoint (720px).
          "max-[721px]:inset-0 max-[721px]:h-dvh max-[721px]:max-h-none max-[721px]:w-full max-[721px]:rounded-none max-[721px]:border-0",
        )}
        // The inspector stays open while the operator uses the chat beside it.
        data-inspector-companion=""
        onKeyDown={onKeyDown}
        role="dialog"
      >
        <header className="flex flex-wrap items-center gap-2 border-b p-3">
          <ChatRoutePicker />
          {/* The panel is narrow at any viewport, so the switch always takes the row below. */}
          <ChatModeSwitch className="order-last" />
          <div className="ml-auto flex shrink-0 items-center gap-2">
            {session.mode === "chat" ? (
              <ChatHeaderButton label="New chat" onClick={newChat}>
                <MessageSquarePlus />
              </ChatHeaderButton>
            ) : (
              <ClearAnswersButton />
            )}
            <ChatHeaderButton label="Open the Chat page" onClick={expand}>
              <Maximize2 />
            </ChatHeaderButton>
            <ChatHeaderButton label="Close" onClick={close}>
              <XIcon />
            </ChatHeaderButton>
          </div>
        </header>
        <ChatSessionBody
          onNavigateAway={isMobile ? close : undefined}
          onOpenPage={expand}
          textareaRef={textareaRef}
        />
      </section>
    </>
  );
}
