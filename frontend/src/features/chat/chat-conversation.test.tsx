import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { ChatConversation } from "@/features/chat/chat-conversation";
import type { ChatTurn } from "@/features/chat/use-chat";

function renderTurns(messages: ChatTurn[]) {
  const { container } = render(<ChatConversation messages={messages} streaming={false} />);
  return Array.from(container.querySelectorAll("p")).map((paragraph) => paragraph.textContent);
}

describe("ChatConversation", () => {
  it("drops boundary blank lines of reasoning and answer but keeps interior line breaks", () => {
    const texts = renderTurns([
      {
        id: 1,
        role: "assistant",
        reasoning: "\nfirst\nsecond\n\n",
        content: "\n\nanswer one\nanswer two\n",
      },
    ]);

    expect(texts).toEqual(["first\nsecond", "answer one\nanswer two"]);
  });

  it("shows time, tokens and speed under a finished reply, and nothing under user messages", () => {
    const { container } = render(
      <ChatConversation
        messages={[
          { id: 1, role: "user", content: "hello" },
          {
            id: 2,
            role: "assistant",
            content: "answer",
            metrics: { totalMs: 2400, generationMs: 2000, inputTokens: 128, outputTokens: 412 },
          },
        ]}
        streaming={false}
      />,
    );

    expect(metricItems(container)).toEqual([
      ["Total time: 2.4 s", "Input tokens: 128", "Output tokens: 412", "Speed: 206 tok/s"],
    ]);
    expect(container.querySelector('[data-role="user"] [data-slot="reply-metrics"]')).toBeNull();
  });

  it("shows only the time while streaming and for a reply without usage", () => {
    const streaming = render(
      <ChatConversation
        messages={[
          { id: 1, role: "assistant", content: "a", metrics: { totalMs: 12_345 } },
          { id: 2, role: "user", content: "again" },
          {
            id: 3,
            role: "assistant",
            content: "b",
            metrics: { totalMs: 870, generationMs: 300 },
          },
        ]}
        streaming
      />,
    );

    expect(metricItems(streaming.container)).toEqual([["Total time: 12 s"], ["Total time: 0.9 s"]]);
  });

  it("renders no reasoning block when the reasoning is only whitespace", () => {
    const { container } = render(
      <ChatConversation
        messages={[{ id: 1, role: "assistant", reasoning: "\n\n", content: "answer" }]}
        streaming={false}
      />,
    );

    expect(container.querySelector("details")).toBeNull();
    expect(container.querySelectorAll("p")).toHaveLength(1);
  });
});

/** The text of each metric, per reply footer. */
function metricItems(container: HTMLElement) {
  return [...container.querySelectorAll('[data-slot="reply-metrics"]')].map((footer) =>
    [...footer.children].map((item) => item.textContent),
  );
}
