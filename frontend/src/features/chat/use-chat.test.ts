import { act, renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { ChatClient, ChatRequest, ChatStreamEvent } from "@/api/chat-client";
import { useChat } from "@/features/chat/use-chat";

/** A client whose streams are driven by the test: each request exposes push and finish controls. */
function controlledClient() {
  const requests: {
    request: ChatRequest;
    push: (event: ChatStreamEvent) => void;
    finish: () => void;
  }[] = [];

  const client: ChatClient = {
    async *streamChat(request) {
      const queue: ChatStreamEvent[] = [];
      let wake: (() => void) | undefined;
      let finished = false;
      requests.push({
        request,
        push: (event) => {
          queue.push(event);
          wake?.();
        },
        finish: () => {
          finished = true;
          wake?.();
        },
      });
      request.signal?.addEventListener("abort", () => {
        finished = true;
        wake?.();
      });

      while (!finished || queue.length > 0) {
        if (request.signal?.aborted) return;
        const event = queue.shift();
        if (event) {
          yield event;
          continue;
        }
        await new Promise<void>((resolve) => (wake = resolve));
      }
    },
  };

  return { client, requests };
}

describe("useChat", () => {
  it("streams a reply into the conversation and sends only role and content", async () => {
    const { client, requests } = controlledClient();
    const { result } = renderHook(() => useChat(client, "route-a"));

    act(() => result.current.send("  hello  "));
    await waitFor(() => expect(requests).toHaveLength(1));

    expect(result.current.streaming).toBe(true);
    expect(requests[0].request.model).toBe("route-a");
    expect(requests[0].request.messages).toEqual([{ role: "user", content: "hello" }]);

    act(() => {
      requests[0].push({ type: "reasoning", text: "hm" });
      requests[0].push({ type: "content", text: "Hi " });
      requests[0].push({ type: "content", text: "there" });
      requests[0].push({ type: "done" });
      requests[0].finish();
    });

    await waitFor(() => expect(result.current.streaming).toBe(false));
    expect(
      result.current.messages.map(({ role, content, reasoning }) => ({ role, content, reasoning })),
    ).toEqual([
      { role: "user", content: "hello", reasoning: undefined },
      { role: "assistant", content: "Hi there", reasoning: "hm" },
    ]);
    expect(result.current.error).toBeUndefined();
  });

  it("measures each reply's time, generation time and tokens on the given clock", async () => {
    const { client, requests } = controlledClient();
    let clock = 1000;
    const { result } = renderHook(() => useChat(client, "r", () => clock));
    const reply = () => result.current.messages[1];
    const step = async (event: ChatStreamEvent, at: number, settled: () => boolean) => {
      clock = at;
      act(() => requests[0].push(event));
      await waitFor(() => expect(settled()).toBe(true));
    };

    act(() => result.current.send("hello"));
    await waitFor(() => expect(requests).toHaveLength(1));
    await step({ type: "reasoning", text: "hm" }, 1300, () => reply().reasoning === "hm");
    await step({ type: "content", text: "Hi" }, 1500, () => reply().content === "Hi");

    expect(reply().metrics).toEqual({ totalMs: 500, generationMs: 200 });

    await step({ type: "content", text: " there" }, 3300, () => reply().content === "Hi there");
    clock = 3400;
    act(() => {
      requests[0].push({ type: "usage", usage: { promptTokens: 128, completionTokens: 412 } });
      requests[0].push({ type: "done" });
      requests[0].finish();
    });
    await waitFor(() => expect(result.current.streaming).toBe(false));

    expect(reply().metrics).toEqual({
      totalMs: 2400,
      generationMs: 2000,
      inputTokens: 128,
      outputTokens: 412,
    });
    expect(result.current.messages[0].metrics).toBeUndefined();
  });

  it("keeps the metrics measured until a stop", async () => {
    const { client, requests } = controlledClient();
    let clock = 0;
    const { result } = renderHook(() => useChat(client, "r", () => clock));

    act(() => result.current.send("hello"));
    await waitFor(() => expect(requests).toHaveLength(1));
    clock = 400;
    act(() => requests[0].push({ type: "content", text: "partial" }));
    await waitFor(() => expect(result.current.messages[1].content).toBe("partial"));
    clock = 900;
    act(() => result.current.stop());
    await waitFor(() => expect(result.current.streaming).toBe(false));

    expect(result.current.messages[1].metrics).toEqual({ totalMs: 900, generationMs: 0 });
  });

  it("never sends reasoning back in later requests", async () => {
    const { client, requests } = controlledClient();
    const { result } = renderHook(() => useChat(client, "r"));

    act(() => result.current.send("one"));
    await waitFor(() => expect(requests).toHaveLength(1));
    act(() => {
      requests[0].push({ type: "reasoning", text: "secret" });
      requests[0].push({ type: "content", text: "a" });
      requests[0].push({ type: "done" });
      requests[0].finish();
    });
    await waitFor(() => expect(result.current.streaming).toBe(false));

    act(() => result.current.send("two"));
    await waitFor(() => expect(requests).toHaveLength(2));

    expect(requests[1].request.messages).toEqual([
      { role: "user", content: "one" },
      { role: "assistant", content: "a" },
      { role: "user", content: "two" },
    ]);
  });

  it("keeps the user message and shows the error when the reply fails, then retries it", async () => {
    const { client, requests } = controlledClient();
    const { result } = renderHook(() => useChat(client, "r"));

    act(() => result.current.send("question"));
    await waitFor(() => expect(requests).toHaveLength(1));
    act(() => {
      requests[0].push({ type: "error", message: "model not ready" });
      requests[0].finish();
    });

    await waitFor(() => expect(result.current.streaming).toBe(false));
    expect(result.current.error).toBe("model not ready");
    expect(result.current.messages.map(({ role, content }) => ({ role, content }))).toEqual([
      { role: "user", content: "question" },
    ]);

    act(() => result.current.retry());
    await waitFor(() => expect(requests).toHaveLength(2));
    expect(requests[1].request.messages).toEqual([{ role: "user", content: "question" }]);
    expect(result.current.error).toBeUndefined();
  });

  it("stop aborts the request and keeps the received text without an error", async () => {
    const { client, requests } = controlledClient();
    const { result } = renderHook(() => useChat(client, "r"));

    act(() => result.current.send("go"));
    await waitFor(() => expect(requests).toHaveLength(1));
    act(() => requests[0].push({ type: "content", text: "partial" }));
    await waitFor(() => expect(result.current.messages.at(-1)?.content).toBe("partial"));

    act(() => result.current.stop());

    await waitFor(() => expect(result.current.streaming).toBe(false));
    expect(requests[0].request.signal?.aborted).toBe(true);
    expect(result.current.error).toBeUndefined();
    expect(result.current.messages.at(-1)).toMatchObject({ role: "assistant", content: "partial" });
  });

  it("new chat clears the conversation and aborts a running reply", async () => {
    const { client, requests } = controlledClient();
    const { result } = renderHook(() => useChat(client, "r"));

    act(() => result.current.send("go"));
    await waitFor(() => expect(requests).toHaveLength(1));

    act(() => result.current.reset());

    expect(requests[0].request.signal?.aborted).toBe(true);
    expect(result.current.messages).toEqual([]);
    expect(result.current.streaming).toBe(false);
  });

  it("blocks sending without a route and while streaming", async () => {
    const streamChat = vi.fn(controlledClient().client.streamChat);
    const { result, rerender } = renderHook(
      ({ route }: { route?: string }) => useChat({ streamChat }, route),
      { initialProps: {} },
    );

    act(() => result.current.send("hi"));
    expect(streamChat).not.toHaveBeenCalled();
    expect(result.current.messages).toEqual([]);

    rerender({ route: "r" });
    act(() => result.current.send("hi"));
    await waitFor(() => expect(streamChat).toHaveBeenCalledTimes(1));
    act(() => result.current.send("again"));
    expect(streamChat).toHaveBeenCalledTimes(1);
  });

  it("keeps history when the route changes", async () => {
    const { client, requests } = controlledClient();
    const { result, rerender } = renderHook(({ route }) => useChat(client, route), {
      initialProps: { route: "a" },
    });

    act(() => result.current.send("one"));
    await waitFor(() => expect(requests).toHaveLength(1));
    act(() => {
      requests[0].push({ type: "content", text: "x" });
      requests[0].push({ type: "done" });
      requests[0].finish();
    });
    await waitFor(() => expect(result.current.streaming).toBe(false));

    rerender({ route: "b" });
    act(() => result.current.send("two"));
    await waitFor(() => expect(requests).toHaveLength(2));

    expect(requests[1].request.model).toBe("b");
    expect(requests[1].request.messages).toHaveLength(3);
  });
});
