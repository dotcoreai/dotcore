import { describe, expect, it, vi } from "vitest";

import { type ChatStreamEvent, createChatClient } from "@/api/chat-client";
import { contentChunk, openStreamResponse, sseFrame, streamResponse } from "@/test/stream-response";

const messages = [{ role: "user" as const, content: "hi" }];

async function collect(stream: AsyncGenerator<ChatStreamEvent, void>) {
  const events: ChatStreamEvent[] = [];
  for await (const event of stream) events.push(event);
  return events;
}

function clientFor(response: () => Response) {
  const fetchMock = vi.fn<(input: string, init?: RequestInit) => Promise<Response>>(() =>
    Promise.resolve(response()),
  );
  return { client: createChatClient("http://host:1/", fetchMock), fetchMock };
}

describe("chat client", () => {
  it("posts only role and content with streaming options", async () => {
    const { client, fetchMock } = clientFor(() => streamResponse([sseFrame("[DONE]")]));

    await collect(
      client.streamChat({
        model: "route-a",
        messages: [{ role: "user", content: "hi", id: "ui-1" } as never],
      }),
    );

    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe("http://host:1/v1/chat/completions");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(init?.body as string)).toEqual({
      model: "route-a",
      messages: [{ role: "user", content: "hi" }],
      stream: true,
      stream_options: { include_usage: true },
    });
  });

  it("streams content, reasoning and usage, ending on done", async () => {
    const { client } = clientFor(() =>
      streamResponse([
        sseFrame({ choices: [], usage: null }),
        sseFrame({ choices: [{ delta: { reasoning: "think" } }] }),
        sseFrame({ choices: [{ delta: { reasoning_content: "more" } }] }),
        sseFrame(contentChunk("Hel")),
        sseFrame({ choices: [{ delta: { content: null } }] }),
        sseFrame(contentChunk("lo")),
        sseFrame({ choices: [{ delta: {}, finish_reason: "stop" }] }),
        sseFrame({
          choices: [],
          usage: { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 },
        }),
        sseFrame("[DONE]"),
      ]),
    );

    expect(await collect(client.streamChat({ model: "r", messages }))).toEqual([
      { type: "reasoning", text: "think" },
      { type: "reasoning", text: "more" },
      { type: "content", text: "Hel" },
      { type: "content", text: "lo" },
      { type: "usage", usage: { promptTokens: 3, completionTokens: 2, totalTokens: 5 } },
      { type: "done" },
    ]);
  });

  it("parses frames split mid-line and across CRLF boundaries, skipping comments", async () => {
    const frame = `: keep-alive\r\ndata: ${JSON.stringify(contentChunk("ab"))}\r\n\r\n`;
    const { client } = clientFor(() =>
      streamResponse([
        frame.slice(0, 20),
        frame.slice(20, frame.length - 3),
        frame.slice(frame.length - 3, frame.length - 1),
        frame.slice(frame.length - 1),
        "data: [DONE]\r\n\r\n",
      ]),
    );

    expect(await collect(client.streamChat({ model: "r", messages }))).toEqual([
      { type: "content", text: "ab" },
      { type: "done" },
    ]);
  });

  it("reports a mid-stream error frame and keeps earlier content", async () => {
    const { client } = clientFor(() =>
      streamResponse([
        sseFrame(contentChunk("part")),
        sseFrame({ error: { message: "upstream died", type: "upstream_error" } }),
      ]),
    );

    expect(await collect(client.streamChat({ model: "r", messages }))).toEqual([
      { type: "content", text: "part" },
      { type: "error", message: "upstream died" },
    ]);
  });

  it("reports a stream that ends without done or a finish reason as interrupted", async () => {
    const { client } = clientFor(() => streamResponse([sseFrame(contentChunk("part"))]));

    expect(await collect(client.streamChat({ model: "r", messages }))).toEqual([
      { type: "content", text: "part" },
      { type: "error", message: "The reply was interrupted before it finished." },
    ]);
  });

  it("surfaces the JSON error message of a failed request", async () => {
    const { client } = clientFor(
      () =>
        new Response(
          JSON.stringify({ error: { message: "model not ready", code: "model_not_ready" } }),
          {
            status: 503,
            statusText: "Service Unavailable",
          },
        ),
    );

    expect(await collect(client.streamChat({ model: "r", messages }))).toEqual([
      { type: "error", message: "model not ready" },
    ]);
  });

  it("falls back to the status for a non-JSON error body", async () => {
    const { client } = clientFor(
      () => new Response("<html>bad gateway</html>", { status: 502, statusText: "Bad Gateway" }),
    );

    expect(await collect(client.streamChat({ model: "r", messages }))).toEqual([
      { type: "error", message: "Request failed: 502 Bad Gateway" },
    ]);
  });

  it("reports an unreachable API", async () => {
    const client = createChatClient("http://host:1", () =>
      Promise.reject(new TypeError("fetch failed")),
    );

    expect(await collect(client.streamChat({ model: "r", messages }))).toEqual([
      { type: "error", message: "Could not reach the inference API: fetch failed" },
    ]);
  });

  it("ends silently without an error when aborted mid-stream", async () => {
    const controller = new AbortController();
    const client = createChatClient("http://host:1", (_input, init) =>
      Promise.resolve(
        openStreamResponse([sseFrame(contentChunk("one"))], init?.signal ?? undefined),
      ),
    );

    const events: ChatStreamEvent[] = [];
    for await (const event of client.streamChat({
      model: "r",
      messages,
      signal: controller.signal,
    })) {
      events.push(event);
      controller.abort();
    }

    expect(events).toEqual([{ type: "content", text: "one" }]);
  });
});
