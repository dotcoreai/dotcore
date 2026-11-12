import { MessageSquarePlus, Minimize2 } from "lucide-react";
import { useEffect, useId, useLayoutEffect, useRef } from "react";
import { useSearchParams } from "react-router-dom";

import { Button } from "@/components/ui/button";
import { PageHeader, PageHeaderHeading } from "@/components/ui/page-header";
import { ChatSectionIcon } from "@/components/ui/section-icons";
import { useChatConversation, useChatRouteSession } from "@/features/chat/chat-session-context";
import {
  ChatHeaderButton,
  ChatModeSwitch,
  ChatRoutePicker,
  ChatSessionBody,
  ClearAnswersButton,
} from "@/features/chat/chat-view";

/**
 * The console chat as a full page: the same chat session as the panel, with room for the
 * conversation. The URL's `route` parameter is the page's route: entering the page selects it, and
 * the page's picker replaces it, so Back still leaves the page. Without one, the page keeps the
 * session's route.
 */
export function ChatPage() {
  const headingId = useId();
  const [searchParams, setSearchParams] = useSearchParams();
  const requestedRoute = searchParams.get("route");
  const session = useChatRouteSession();
  const { selectRoute } = session;
  const chat = useChatConversation();
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  // Before paint, so the page never shows the session's previous route.
  useLayoutEffect(() => {
    if (requestedRoute !== null) selectRoute(requestedRoute);
  }, [requestedRoute, selectRoute]);

  useEffect(() => {
    textareaRef.current?.focus();
  }, []);

  const changeRoute = (route: string | null) => {
    selectRoute(route);
    setSearchParams(
      (params) => {
        const next = new URLSearchParams(params);
        if (route === null) next.delete("route");
        else next.set("route", route);
        return next;
      },
      { replace: true },
    );
  };
  const newChat = () => {
    chat.reset();
    textareaRef.current?.focus();
  };

  return (
    <div className="flex flex-col gap-6">
      <PageHeader>
        <PageHeaderHeading icon={ChatSectionIcon} id={headingId}>
          Chat
        </PageHeaderHeading>
      </PageHeader>
      <section
        aria-labelledby={headingId}
        data-chat-mode={session.mode}
        className="flex h-[calc(100dvh-var(--header-height)---spacing(32))] min-h-96 flex-col overflow-hidden rounded-xl border bg-card"
      >
        <header className="flex flex-wrap items-center gap-2 border-b p-3">
          <ChatRoutePicker onRouteChange={changeRoute} />
          <ChatModeSwitch className="max-lg:order-last" />
          <div className="ml-auto flex shrink-0 items-center gap-2">
            {session.mode === "chat" ? (
              <ChatHeaderButton label="New chat" onClick={newChat}>
                <MessageSquarePlus />
              </ChatHeaderButton>
            ) : (
              <ClearAnswersButton />
            )}
            <Button onClick={session.showAsPanel} size="console" type="button" variant="ghost">
              <Minimize2 data-icon="inline-start" />
              <span className="max-sm:sr-only">Show as panel</span>
            </Button>
          </div>
        </header>
        <ChatSessionBody page textareaRef={textareaRef} />
      </section>
    </div>
  );
}
