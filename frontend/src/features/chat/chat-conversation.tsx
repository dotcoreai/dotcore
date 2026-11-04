import { ArrowDown, ArrowUp, Clock, type LucideIcon, Zap } from "lucide-react";
import { useEffect, useRef } from "react";

import type { ChatReplyMetrics, ChatTurn } from "@/features/chat/use-chat";
import { cn } from "@/lib/utils";

/**
 * Scrolling list of turns as plain text, each assistant reply with its time, tokens and speed. The
 * newest content stays in view while streaming.
 */
export function ChatConversation({
  messages,
  streaming,
}: {
  messages: ChatTurn[];
  streaming: boolean;
}) {
  const endRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    endRef.current?.scrollIntoView?.({ block: "end" });
  }, [messages]);

  return (
    <div
      aria-busy={streaming}
      aria-live="polite"
      className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-3"
      role="log"
    >
      {messages.map((turn, index) => {
        // Models stream blank lines around the reasoning and the answer; pre-wrap would render them as a gap.
        const reasoning = turn.reasoning?.trim();
        const content = turn.content.trim();

        return (
          <div
            className={cn(
              "flex max-w-[85%] flex-col gap-1.5 rounded-lg px-3 py-2 text-sm",
              turn.role === "user"
                ? "self-end bg-primary text-primary-foreground"
                : "self-start bg-secondary text-secondary-foreground",
            )}
            data-role={turn.role}
            key={turn.id}
          >
            {reasoning ? (
              <details className="text-xs text-muted-foreground">
                <summary className="cursor-pointer">Reasoning</summary>
                <p className="mt-1 break-words whitespace-pre-wrap">{reasoning}</p>
              </details>
            ) : null}
            {content ? <p className="break-words whitespace-pre-wrap">{content}</p> : null}
            {turn.role === "assistant" && turn.metrics ? (
              <ReplyMetrics
                metrics={turn.metrics}
                streaming={streaming && index === messages.length - 1}
              />
            ) : null}
          </div>
        );
      })}
      <div ref={endRef} />
    </div>
  );
}

/**
 * A reply's total time, input and output tokens, and generation speed. While the reply streams,
 * only the time shows: token counts arrive with the stream's last event.
 */
function ReplyMetrics({ metrics, streaming }: { metrics: ChatReplyMetrics; streaming: boolean }) {
  const { generationMs, inputTokens, outputTokens, totalMs } = metrics;
  const items: { icon: LucideIcon; label: string; value: string }[] = [
    { icon: Clock, label: "Total time", value: formatSeconds(totalMs) },
  ];
  if (!streaming) {
    if (inputTokens !== undefined) {
      items.push({ icon: ArrowDown, label: "Input tokens", value: String(inputTokens) });
    }
    if (outputTokens !== undefined) {
      items.push({ icon: ArrowUp, label: "Output tokens", value: String(outputTokens) });
      if (generationMs) {
        const rate = Math.round(outputTokens / (generationMs / 1000));
        items.push({ icon: Zap, label: "Speed", value: `${rate} tok/s` });
      }
    }
  }

  return (
    <div
      className="flex flex-wrap gap-x-3 gap-y-0.5 font-mono text-[11px] text-muted-foreground tabular-nums"
      data-slot="reply-metrics"
    >
      {items.map(({ icon: Icon, label, value }) => (
        <span className="inline-flex items-center gap-1" key={label} title={label}>
          <Icon aria-hidden="true" className="size-3" />
          <span className="sr-only">{label}: </span>
          {value}
        </span>
      ))}
    </div>
  );
}

/** Seconds with one decimal below ten, whole seconds above. */
function formatSeconds(ms: number): string {
  const seconds = ms / 1000;
  return seconds < 10 ? `${seconds.toFixed(1)} s` : `${Math.round(seconds)} s`;
}
