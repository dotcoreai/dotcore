import { describeNetworkError, isAbortError, parseErrorEnvelope } from "@/api/inference-errors";
import { consoleInferenceOrigin } from "@/api/inference-origin";

export interface ChatMessage {
  role: "user" | "assistant" | "system";
  content: string;
}

export interface ChatRequest {
  /** The inference route name, sent as the OpenAI `model`. */
  model: string;
  messages: ChatMessage[];
  signal?: AbortSignal;
}

export interface ChatUsage {
  promptTokens?: number;
  completionTokens?: number;
  totalTokens?: number;
}

/**
 * One step of a streamed reply. A stream always ends with exactly one `done` or `error` event,
 * unless the caller aborts it, which ends the stream silently.
 */
export type ChatStreamEvent =
  | { type: "content"; text: string }
  | { type: "reasoning"; text: string }
  | { type: "usage"; usage: ChatUsage }
  | { type: "done" }
  | { type: "error"; message: string };

export interface ChatClient {
  streamChat: (request: ChatRequest) => AsyncGenerator<ChatStreamEvent, void>;
}

type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;

/**
 * Streaming client for the OpenAI-compatible chat completions endpoint of the inference API.
 * Failures before and during the stream surface as `error` events carrying an operator-readable
 * message, so callers never need to catch.
 */
export function createChatClient(
  origin = consoleInferenceOrigin(),
  fetchImpl: FetchLike = (input, init) => globalThis.fetch(input, init),
): ChatClient {
  const endpoint = `${origin.replace(/\/+$/, "")}/v1/chat/completions`;

  async function* streamChat({
    model,
    messages,
    signal,
  }: ChatRequest): AsyncGenerator<ChatStreamEvent, void> {
    try {
      const response = await fetchImpl(endpoint, {
        method: "POST",
        headers: { "Content-Type": "application/json", Accept: "text/event-stream" },
        // Only role and content: the backend rejects unknown message fields.
        body: JSON.stringify({
          model,
          messages: messages.map(({ role, content }) => ({ role, content })),
          stream: true,
          stream_options: { include_usage: true },
        }),
        signal,
      });

      if (!response.ok) {
        const text = await response.text().catch(() => "");
        yield { type: "error", message: parseErrorEnvelope(response, text).message };
        return;
      }
      if (!response.body) {
        yield { type: "error", message: "The inference API returned no response body." };
        return;
      }

      yield* readCompletionStream(response.body);
    } catch (error) {
      if (signal?.aborted || isAbortError(error)) return;
      yield { type: "error", message: describeNetworkError(error) };
    }
  }

  return { streamChat };
}

async function* readCompletionStream(
  body: ReadableStream<Uint8Array>,
): AsyncGenerator<ChatStreamEvent, void> {
  let finished = false;

  for await (const data of readSseData(body)) {
    if (data === "[DONE]") {
      yield { type: "done" };
      return;
    }

    let chunk: CompletionChunk;
    try {
      chunk = JSON.parse(data) as CompletionChunk;
    } catch {
      yield { type: "error", message: "The inference API sent an unreadable response." };
      return;
    }

    if (chunk.error) {
      yield {
        type: "error",
        message: chunk.error.message ?? "The inference API reported an error.",
      };
      return;
    }

    const choice = chunk.choices?.[0];
    const reasoning = choice?.delta?.reasoning ?? choice?.delta?.reasoning_content;
    if (reasoning) yield { type: "reasoning", text: reasoning };
    if (choice?.delta?.content) yield { type: "content", text: choice.delta.content };
    if (choice?.finish_reason) finished = true;
    if (chunk.usage) {
      yield {
        type: "usage",
        usage: {
          promptTokens: chunk.usage.prompt_tokens,
          completionTokens: chunk.usage.completion_tokens,
          totalTokens: chunk.usage.total_tokens,
        },
      };
    }
  }

  yield finished
    ? { type: "done" }
    : { type: "error", message: "The reply was interrupted before it finished." };
}

/** Yields the joined `data:` payload of each server-sent event frame in the body. */
async function* readSseData(body: ReadableStream<Uint8Array>): AsyncGenerator<string, void> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  try {
    for (;;) {
      const { done, value } = await reader.read();
      buffer += decoder.decode(value, { stream: !done });
      // Normalizing the whole buffer joins a "\r" and "\n" that arrived in separate chunks.
      buffer = buffer.replace(/\r\n/g, "\n");

      let boundary = buffer.indexOf("\n\n");
      while (boundary !== -1) {
        const data = frameData(buffer.slice(0, boundary));
        buffer = buffer.slice(boundary + 2);
        if (data !== undefined) yield data;
        boundary = buffer.indexOf("\n\n");
      }

      if (done) break;
    }

    const data = frameData(buffer);
    if (data !== undefined) yield data;
  } finally {
    // Cancels the underlying request when the consumer stops early or aborts.
    await reader.cancel().catch(() => undefined);
  }
}

function frameData(frame: string): string | undefined {
  const lines = frame
    .split("\n")
    .filter((line) => line.startsWith("data:"))
    .map((line) => (line.startsWith("data: ") ? line.slice(6) : line.slice(5)));

  return lines.length > 0 ? lines.join("\n") : undefined;
}

interface CompletionChunk {
  choices?: {
    delta?: {
      content?: string | null;
      reasoning?: string | null;
      reasoning_content?: string | null;
    };
    finish_reason?: string | null;
  }[];
  usage?: { prompt_tokens?: number; completion_tokens?: number; total_tokens?: number } | null;
  error?: { message?: string };
}
