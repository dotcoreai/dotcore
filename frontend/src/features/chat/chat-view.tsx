import { Eraser } from "lucide-react";
import type { ReactNode, RefObject } from "react";
import { Link } from "react-router-dom";

import { Button, buttonVariants } from "@/components/ui/button";
import { Combobox } from "@/components/ui/combobox";
import { Notice } from "@/components/ui/notice";
import { SegmentedControl, type SegmentedControlOption } from "@/components/ui/segmented-control";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { requestKindsLabel } from "@/domain/request-kinds";
import { ChatComposer } from "@/features/chat/chat-composer";
import { ChatConversation } from "@/features/chat/chat-conversation";
import type { ChatMode } from "@/features/chat/chat-opener";
import {
  useChatConversation,
  useChatDecisions,
  useChatRouteSession,
} from "@/features/chat/chat-session-context";
import { DecisionEditor, DecisionFooter } from "@/features/chat/decisions/decision-controls";
import { DecisionWorkspace } from "@/features/chat/decisions/decision-workspace";
import type { Chat } from "@/features/chat/use-chat";
import { InferenceRequestKind } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import { cn } from "@/lib/utils";

/** The chat session's route picker, labelled "Route" beside it. */
export function ChatRoutePicker({
  onRouteChange,
}: {
  onRouteChange?: (route: string | null) => void;
}) {
  const session = useChatRouteSession();

  return (
    <div className="flex min-w-40 flex-1 items-center gap-2">
      <span className="text-sm font-medium">Route</span>
      <Combobox
        className="min-w-0 flex-1"
        hideLabel
        label="Route"
        loading={session.loading}
        onValueChange={onRouteChange ?? session.selectRoute}
        options={session.pickerOptions}
        placeholder="No route available"
        searchable={false}
        value={session.chosenUnavailable ? session.chosenRoute : (session.route ?? null)}
      />
    </div>
  );
}

/**
 * The Chat / Decisions switch of a route serving both kinds; nothing for other routes. The header
 * passes `className` to place it after the buttons where the header is narrow.
 */
export function ChatModeSwitch({ className }: { className?: string }) {
  const session = useChatRouteSession();
  if (!session.modeSwitchable) return null;

  return (
    <SegmentedControl
      aria-label="Request kind"
      className={cn("shrink-0", className)}
      onValueChange={session.selectMode}
      options={modeOptions}
      value={session.mode}
    />
  );
}

/** An icon-only header control with its label as the accessible name and tooltip. */
export function ChatHeaderButton({
  children,
  label,
  onClick,
}: {
  children: ReactNode;
  label: string;
  onClick: () => void;
}) {
  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <Button
            aria-label={label}
            onClick={onClick}
            size="icon-sm"
            type="button"
            variant="ghost"
          />
        }
      >
        {children}
      </TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
  );
}

/** The header control dropping the decision answers, and the Ask in flight. */
export function ClearAnswersButton() {
  const { clear } = useChatDecisions();

  return (
    <ChatHeaderButton label="Clear answers" onClick={clear}>
      <Eraser />
    </ChatHeaderButton>
  );
}

/**
 * The body of the chat session for its mode: in chat mode the turns, their notices and the message
 * box; in decision mode the decision draft and its answers, compact with an Ask footer in the
 * panel, and as the full form or JSON editor on the page. The no-route state replaces either, and
 * an unavailable chosen route is noted under both. `onNavigateAway` runs when the operator follows
 * the link to Routes, and `onOpenPage` leads to the Chat page.
 */
export function ChatSessionBody({
  onNavigateAway,
  onOpenPage,
  page = false,
  textareaRef,
}: {
  onNavigateAway?: () => void;
  onOpenPage?: () => void;
  /** Whether the body fills the Chat page rather than the panel. */
  page?: boolean;
  textareaRef: RefObject<HTMLTextAreaElement | null>;
}) {
  const session = useChatRouteSession();
  const chat = useChatConversation();
  const conversation = session.mode === "chat";

  return (
    <>
      {session.noRoute ? (
        <NoRoute onNavigate={onNavigateAway} />
      ) : conversation ? (
        <ChatConversation messages={chat.messages} streaming={chat.streaming} />
      ) : page ? (
        <DecisionWorkspace />
      ) : (
        <DecisionEditor onOpenPage={onOpenPage} />
      )}
      {session.chosenUnavailable ? (
        <Notice className="mx-3 mb-3" role="status" tone="warning">
          {session.chosenRoute} is not available. Pick another route to continue.
        </Notice>
      ) : null}
      {conversation ? (
        <ConversationFooter chat={chat} textareaRef={textareaRef} />
      ) : page ? null : (
        <DecisionFooter />
      )}
    </>
  );
}

function ConversationFooter({
  chat,
  textareaRef,
}: {
  chat: Chat;
  textareaRef: RefObject<HTMLTextAreaElement | null>;
}) {
  const session = useChatRouteSession();

  return (
    <>
      {chat.error ? (
        <Notice
          className="mx-3 mb-3 flex items-center justify-between gap-2"
          role="alert"
          tone="destructive"
        >
          <span className="min-w-0 break-words">{chat.error}</span>
          <Button onClick={chat.retry} size="xs" type="button" variant="outline">
            Retry
          </Button>
        </Notice>
      ) : null}
      <ChatComposer
        canSend={Boolean(session.route)}
        onSend={(text) => {
          chat.send(text);
          // Keeping focus on a touch device leaves the on-screen keyboard covering the reply.
          if (window.matchMedia("(pointer: coarse)").matches) textareaRef.current?.blur();
          else textareaRef.current?.focus();
        }}
        onStop={chat.stop}
        streaming={chat.streaming}
        textareaRef={textareaRef}
      />
    </>
  );
}

const modeOptions: readonly SegmentedControlOption<ChatMode>[] = [
  { label: requestKindsLabel([InferenceRequestKind.CHAT]), value: "chat" },
  { label: requestKindsLabel([InferenceRequestKind.DECISION]), value: "decision" },
];

function NoRoute({ onNavigate }: { onNavigate?: () => void }) {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-3 p-6">
      <p className="text-sm text-muted-foreground">No route available</p>
      <Link
        className={buttonVariants({ size: "console", variant: "outline" })}
        onClick={onNavigate}
        to="/routes"
      >
        Go to Routes
      </Link>
    </div>
  );
}
