import { describe, expect, it } from "vitest";

import type { DecisionDraft } from "@/features/chat/decisions/decision-draft";
import { decisionExamples } from "@/features/chat/decisions/decision-examples";
import {
  createDecisionStore,
  DECISIONS_STORAGE_KEY,
} from "@/features/chat/decisions/decision-store";
import { FailingStorage, MemoryStorage } from "@/test/memory-storage";

const triage = { name: "triage", body: '{"state": "s", "questions": {"2": {}, "10": {}}}' };
const invoice = { name: "invoice", body: '{"state": "t", "questions": {}}' };

/** A form draft that can't build a request: an unnamed question and a one-option choice. */
const invalidForm: DecisionDraft = {
  view: "form",
  form: {
    state: "Ticket 7",
    questions: [
      { name: "", type: "noul", instructions: "?", options: [], levels: [] },
      {
        name: "topic",
        type: "choice",
        instructions: "Which?",
        options: [{ name: "billing", description: "" }],
        levels: ["low", "high"],
      },
    ],
  },
};

describe("Decision store", () => {
  it("saves, replaces and deletes named sets, keeping their order and body text", () => {
    const storage = new MemoryStorage();
    const store = createDecisionStore(storage);

    expect(store.readSets()).toEqual([]);
    expect(store.saveSet(triage)).toEqual([triage]);
    expect(store.saveSet(invoice)).toEqual([triage, invoice]);

    const edited = { name: "triage", body: '{"state": "u", "questions": {}}' };
    expect(store.saveSet(edited)).toEqual([edited, invoice]);
    expect(createDecisionStore(storage).readSets()).toEqual([edited, invoice]);

    expect(store.deleteSet("triage")).toEqual([invoice]);
    expect(store.deleteSet("missing")).toEqual([invoice]);
    expect(createDecisionStore(storage).readSets()).toEqual([invoice]);
  });

  it("restores drafts that can't build a request, form and JSON alike", () => {
    const storage = new MemoryStorage();
    const unparsable: DecisionDraft = {
      view: "json",
      form: decisionExamples[0].form,
      jsonText: '{"state": "s",',
    };

    createDecisionStore(storage).writeDraft(invalidForm);
    expect(createDecisionStore(storage).readDraft()).toEqual(invalidForm);

    createDecisionStore(storage).writeDraft(unparsable);
    expect(createDecisionStore(storage).readDraft()).toEqual(unparsable);
  });

  it("keeps the sets when the draft is written, and the draft when a set is saved", () => {
    const storage = new MemoryStorage();
    const store = createDecisionStore(storage);

    store.saveSet(triage);
    store.writeDraft(invalidForm);
    store.saveSet(invoice);

    expect(JSON.parse(storage.getItem(DECISIONS_STORAGE_KEY)!)).toEqual({
      draft: invalidForm,
      sets: [triage, invoice],
    });
  });

  it("writes the draft back with the sets as stored", () => {
    const storage = new MemoryStorage();
    const sets = [triage, { name: "not json", body: '{"state":' }, "loose"];
    storage.setItem(DECISIONS_STORAGE_KEY, JSON.stringify({ sets }));

    createDecisionStore(storage).writeDraft(invalidForm);

    expect(JSON.parse(storage.getItem(DECISIONS_STORAGE_KEY)!)).toEqual({
      draft: invalidForm,
      sets,
    });
  });

  it("keeps a set saved by another store when this one saves", () => {
    const storage = new MemoryStorage();
    const thisTab = createDecisionStore(storage);
    const otherTab = createDecisionStore(storage);
    expect(thisTab.readSets()).toEqual([]);

    otherTab.saveSet(triage);

    expect(thisTab.saveSet(invoice)).toEqual([triage, invoice]);
    expect(otherTab.deleteSet("invoice")).toEqual([triage]);
    expect(thisTab.readSets()).toEqual([triage]);
  });

  it("drops corrupt entries and loads the rest", () => {
    const storage = new MemoryStorage();
    storage.setItem(
      DECISIONS_STORAGE_KEY,
      JSON.stringify({
        draft: { view: "form", form: { state: "s", questions: [{ name: "q", type: "rank" }] } },
        sets: [
          triage,
          { name: "not json", body: '{"state":' },
          { name: "array", body: "[]" },
          { name: "deep", body: `{"state": ${"[".repeat(100_000)}${"]".repeat(100_000)}}` },
          { name: 7, body: "{}" },
          "loose",
          invoice,
        ],
      }),
    );
    const store = createDecisionStore(storage);

    expect(store.readDraft()).toBeUndefined();
    expect(store.readSets()).toEqual([triage, invoice]);
  });

  it("drops a JSON draft without its text and reads unreadable storage as empty", () => {
    const storage = new MemoryStorage();
    storage.setItem(
      DECISIONS_STORAGE_KEY,
      JSON.stringify({ draft: { view: "json", form: { state: "s", questions: [] } }, sets: [] }),
    );
    expect(createDecisionStore(storage).readDraft()).toBeUndefined();

    storage.setItem(DECISIONS_STORAGE_KEY, "{not json");
    const store = createDecisionStore(storage);
    expect(store.readDraft()).toBeUndefined();
    expect(store.readSets()).toEqual([]);

    store.saveSet(triage);
    expect(JSON.parse(storage.getItem(DECISIONS_STORAGE_KEY)!)).toEqual({ sets: [triage] });
  });

  it("works without storage when the browser denies it", () => {
    for (const storage of [new FailingStorage(), undefined]) {
      const store = createDecisionStore(storage);

      expect(() => store.writeDraft(invalidForm)).not.toThrow();
      expect(store.readDraft()).toBeUndefined();
      expect(store.saveSet(triage)).toEqual([]);
      expect(store.deleteSet("triage")).toEqual([]);
      expect(store.readSets()).toEqual([]);
    }
  });
});
