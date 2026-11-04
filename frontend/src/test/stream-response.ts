/** Builds a streaming `Response` whose body delivers each string as one separate chunk. */
export function streamResponse(
  chunks: string[],
  init: ResponseInit = { headers: { "Content-Type": "text/event-stream" } },
): Response {
  const encoder = new TextEncoder();

  return new Response(
    new ReadableStream<Uint8Array>({
      start(controller) {
        for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
        controller.close();
      },
    }),
    init,
  );
}

/** Builds a stream that delivers its chunks, then stays open until the signal aborts. */
export function openStreamResponse(chunks: string[], signal: AbortSignal | undefined): Response {
  const encoder = new TextEncoder();

  return new Response(
    new ReadableStream<Uint8Array>({
      start(controller) {
        for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
        signal?.addEventListener("abort", () =>
          controller.error(new DOMException("Aborted", "AbortError")),
        );
      },
    }),
    { headers: { "Content-Type": "text/event-stream" } },
  );
}

/** Formats one OpenAI chat completion chunk as a server-sent event frame. */
export function sseFrame(payload: unknown): string {
  return `data: ${typeof payload === "string" ? payload : JSON.stringify(payload)}\n\n`;
}

export function contentChunk(text: string) {
  return { choices: [{ index: 0, delta: { content: text }, finish_reason: null }] };
}
