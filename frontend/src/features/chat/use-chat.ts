import { useCallback, useEffect, useRef, useState } from "react";

import { type ChatClient, type ChatMessage } from "@/api/chat-client";

export interface ChatTurn extends ChatMessage {
  id: number;
  /** Reasoning text streamed alongside an assistant reply. Never sent back to the API. */
  reasoning?: string;
  /** How long an assistant reply took and how many tokens it used, measured as it streams. */
  metrics?: ChatReplyMetrics;
}

/**
 * Timing and token counts of one assistant reply. Token counts come from the stream's usage
 * report, so they stay unset until it arrives or when the route sends none.
 */
export interface ChatReplyMetrics {
  /** Milliseconds from sending to the last event received. */
  totalMs: number;
  /** Milliseconds from the first generated token to the last; unset before the first. */
  generationMs?: number;
  inputTokens?: number;
  outputTokens?: number;
}

export interface Chat {
  messages: ChatTurn[];
  streaming: boolean;
  error?: string;
  /** Sends a user message to the route; ignored while streaming or without text. */
  send: (text: string) => void;
  /** Stops the reply being streamed and keeps the text received so far. */
  stop: () => void;
  /** Resends the last user message after a failed reply. */
  retry: () => void;
  /** Clears the conversation. */
  reset: () => void;
}

/**
 * Conversation state for the chat panel: the turns kept in browser memory and the reply being
 * streamed for the selected route. `route` is the inference route name, sent as the model, and
 * `now` is the millisecond clock replies are timed with.
 */
export function useChat(
  client: ChatClient,
  route: string | undefined,
  now: () => number = performanceNow,
): Chat {
  const [messages, setMessages] = useState<ChatTurn[]>([]);
  const [streaming, setStreaming] = useState(false);
  const [error, setError] = useState<string>();
  const controllerRef = useRef<AbortController | undefined>(undefined);
  const nextId = useRef(1);
  const messagesRef = useRef(messages);

  useEffect(() => {
    messagesRef.current = messages;
  }, [messages]);

  useEffect(() => () => controllerRef.current?.abort(), []);

  const run = useCallback(
    async (history: ChatTurn[]) => {
      if (!route) return;

      const controller = new AbortController();
      controllerRef.current = controller;
      const replyId = nextId.current++;
      setError(undefined);
      setStreaming(true);
      setMessages([...history, { id: replyId, role: "assistant", content: "" }]);

      const update = (change: (reply: ChatTurn) => ChatTurn) =>
        setMessages((current) =>
          current.map((turn) => (turn.id === replyId ? change(turn) : turn)),
        );

      const startedAt = now();
      let firstTokenAt: number | undefined;
      // Reads the clock once per event, outside the state updater, which React may run twice.
      const measure = (change: Partial<ChatReplyMetrics> = {}) => {
        const totalMs = now() - startedAt;
        return (reply: ChatTurn): ChatTurn => ({
          ...reply,
          metrics: { ...reply.metrics, ...change, totalMs },
        });
      };
      const measureToken = () => {
        const at = now();
        firstTokenAt ??= at;
        return measure({ generationMs: at - firstTokenAt });
      };

      const payload = history.map(({ role, content }) => ({ role, content }));
      for await (const event of client.streamChat({
        model: route,
        messages: payload,
        signal: controller.signal,
      })) {
        // A reset or a newer send replaced this run: its remaining events are stale.
        if (controllerRef.current !== controller) break;

        if (event.type === "content") {
          const measured = measureToken();
          update((reply) => measured({ ...reply, content: reply.content + event.text }));
        } else if (event.type === "reasoning") {
          const measured = measureToken();
          update((reply) =>
            measured({ ...reply, reasoning: (reply.reasoning ?? "") + event.text }),
          );
        } else if (event.type === "usage") {
          const { promptTokens, completionTokens } = event.usage;
          update(measure({ inputTokens: promptTokens, outputTokens: completionTokens }));
        } else if (event.type === "error") {
          setError(event.message);
        }
      }

      if (controllerRef.current === controller) {
        controllerRef.current = undefined;
        setStreaming(false);
        update(measure());
        // An empty reply carries nothing to show; the error or the stop is the whole outcome.
        setMessages((current) =>
          current.filter((turn) => turn.id !== replyId || turn.content || turn.reasoning),
        );
      }
    },
    [client, now, route],
  );

  const send = useCallback(
    (text: string) => {
      const content = text.trim();
      if (!route || !content || controllerRef.current) return;

      const user: ChatTurn = { id: nextId.current++, role: "user", content };
      void run([...messagesRef.current, user]);
    },
    [route, run],
  );

  const stop = useCallback(() => controllerRef.current?.abort(), []);

  const retry = useCallback(() => {
    if (controllerRef.current) return;

    const history = messagesRef.current;
    const lastUser = history.map((turn) => turn.role).lastIndexOf("user");
    if (lastUser === -1) return;

    void run(history.slice(0, lastUser + 1));
  }, [run]);

  const reset = useCallback(() => {
    controllerRef.current?.abort();
    controllerRef.current = undefined;
    setMessages([]);
    setStreaming(false);
    setError(undefined);
  }, []);

  return { messages, streaming, error, send, stop, retry, reset };
}

function performanceNow(): number {
  return performance.now();
}
