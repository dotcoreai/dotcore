import {
  type DecisionDraft,
  type DecisionForm,
  type DecisionOptionDraft,
  type DecisionQuestionDraft,
  decisionQuestionTypes,
} from "@/features/chat/decisions/decision-draft";
import { isJsonObject, parseJson } from "@/features/chat/decisions/ordered-json";
import type { StorageLike } from "@/lib/browser-storage";

export const DECISIONS_STORAGE_KEY = "dotcore.console.decisions.v1";

/** A named question set: a System One request body, state included, without `model`. */
export interface DecisionSet {
  name: string;
  body: string;
}

/**
 * The decision draft and the named question sets, kept in the browser. Each write reads what is
 * stored, changes its own part and writes it back, so a set saved from another tab survives.
 * Unreadable entries are dropped on read, and a storage that fails leaves the chat working
 * without it.
 */
export interface DecisionStore {
  /** The stored draft, or nothing when none is stored or it can't be read. */
  readDraft(): DecisionDraft | undefined;
  writeDraft(draft: DecisionDraft): void;
  /** The stored sets, in the order they were first saved. */
  readSets(): DecisionSet[];
  /** Saves a set, replacing the one with the same name, and returns the stored sets. */
  saveSet(set: DecisionSet): DecisionSet[];
  /** Deletes the set with this name and returns the stored sets. */
  deleteSet(name: string): DecisionSet[];
}

export function createDecisionStore(storage: StorageLike | undefined): DecisionStore {
  const readText = (): string | null => {
    try {
      return storage?.getItem(DECISIONS_STORAGE_KEY) ?? null;
    } catch {
      return null;
    }
  };
  const read = (): StoredDecisions => parseStored(readText());
  /** Writes what is stored, reporting whether the storage took it. */
  const write = (next: object): boolean => {
    if (!storage) return false;
    try {
      storage.setItem(DECISIONS_STORAGE_KEY, JSON.stringify(next));
      return true;
    } catch {
      // Browser storage is an optional convenience; the draft stays usable in memory.
      return false;
    }
  };
  /** Applies a change to what is stored and returns what is stored afterwards. */
  const update = (change: (stored: StoredDecisions) => StoredDecisions): StoredDecisions => {
    const current = read();
    const next = change(current);
    return write(next) ? next : current;
  };

  return {
    readDraft: () => read().draft,
    writeDraft: (draft) => {
      // The draft is written on every keystroke, so the sets go back as stored, without reading
      // each body.
      write({ sets: storedSets(readText()), draft });
    },
    readSets: () => read().sets,
    saveSet: (set) =>
      update((stored) => {
        const index = stored.sets.findIndex((entry) => entry.name === set.name);
        const sets =
          index === -1
            ? [...stored.sets, set]
            : stored.sets.map((entry, at) => (at === index ? set : entry));
        return { ...stored, sets };
      }).sets,
    deleteSet: (name) =>
      update((stored) => ({
        ...stored,
        sets: stored.sets.filter((entry) => entry.name !== name),
      })).sets,
  };
}

/** What the store keeps under its key. Sets are a list, so their order survives as saved. */
interface StoredDecisions {
  draft?: DecisionDraft;
  sets: DecisionSet[];
}

function parseStored(value: string | null): StoredDecisions {
  if (!value) return { sets: [] };

  let stored: unknown;
  try {
    stored = JSON.parse(value);
  } catch {
    return { sets: [] };
  }
  if (!isRecord(stored)) return { sets: [] };

  const draft = parseDraft(stored.draft);
  const sets = Array.isArray(stored.sets) ? stored.sets.filter(isSet) : [];
  return draft ? { draft, sets } : { sets };
}

/** The stored sets as written, unread. */
function storedSets(value: string | null): unknown[] {
  try {
    const stored: unknown = JSON.parse(value ?? "");
    return isRecord(stored) && Array.isArray(stored.sets) ? stored.sets : [];
  } catch {
    return [];
  }
}

function parseDraft(value: unknown): DecisionDraft | undefined {
  if (!isRecord(value) || (value.view !== "form" && value.view !== "json")) return undefined;
  if (!isForm(value.form)) return undefined;
  if (value.jsonText !== undefined && typeof value.jsonText !== "string") return undefined;
  // The JSON view works from its text, so a JSON draft without one isn't a draft.
  if (value.view === "json" && value.jsonText === undefined) return undefined;

  return value.jsonText === undefined
    ? { view: value.view, form: value.form }
    : { view: value.view, form: value.form, jsonText: value.jsonText };
}

function isForm(value: unknown): value is DecisionForm {
  return (
    isRecord(value) &&
    typeof value.state === "string" &&
    Array.isArray(value.questions) &&
    value.questions.every(isQuestion)
  );
}

function isQuestion(value: unknown): value is DecisionQuestionDraft {
  return (
    isRecord(value) &&
    typeof value.name === "string" &&
    decisionQuestionTypes.some((type) => type.value === value.type) &&
    typeof value.instructions === "string" &&
    Array.isArray(value.options) &&
    value.options.every(isOption) &&
    Array.isArray(value.levels) &&
    value.levels.every((level) => typeof level === "string")
  );
}

function isOption(value: unknown): value is DecisionOptionDraft {
  return isRecord(value) && typeof value.name === "string" && typeof value.description === "string";
}

/** A named set whose body is still a JSON object, as every saved set is. */
function isSet(value: unknown): value is DecisionSet {
  if (!isRecord(value) || typeof value.name !== "string" || typeof value.body !== "string") {
    return false;
  }
  const body = parseJson(value.body);
  return body !== undefined && isJsonObject(body);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
