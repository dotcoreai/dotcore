import { describe, expect, it } from "vitest";

import { InferenceRequestKind } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";

import { mergeRequestKinds, requestKindsLabel, servesChat } from "./request-kinds";

const { CHAT, DECISION } = InferenceRequestKind;

describe("request kinds", () => {
  it("labels chat-only, decision-only, mixed and undeclared kinds", () => {
    expect(requestKindsLabel([CHAT])).toBe("Chat");
    expect(requestKindsLabel([DECISION])).toBe("Decisions");
    expect(requestKindsLabel([DECISION, CHAT])).toBe("Chat, Decisions");
    expect(requestKindsLabel([])).toBe("");
  });

  it("serves chat only when chat is declared", () => {
    expect(servesChat([CHAT])).toBe(true);
    expect(servesChat([DECISION, CHAT])).toBe(true);
    expect(servesChat([DECISION])).toBe(false);
    expect(servesChat([])).toBe(false);
  });

  it("merges kinds without duplicates in enum order", () => {
    expect(mergeRequestKinds([[DECISION], [CHAT], [DECISION]])).toEqual([CHAT, DECISION]);
    expect(mergeRequestKinds([])).toEqual([]);
  });
});
