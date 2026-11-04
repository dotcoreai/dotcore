import {
  isJsonObject,
  type JsonObject,
  jsonObject,
  type JsonValue,
  parseJson,
  writeJson,
} from "@/features/chat/decisions/ordered-json";

/** The System One question types the form edits: one option, an expected level, or yes or no. */
export type DecisionQuestionType = "choice" | "score" | "noul";

/** One option of a choice question; an empty description leaves the option undescribed. */
export interface DecisionOptionDraft {
  name: string;
  description: string;
}

/**
 * One question as the operator edits it. Choice options and score levels are kept apart, so
 * switching the type back and forth keeps what was typed for each.
 */
export interface DecisionQuestionDraft {
  name: string;
  type: DecisionQuestionType;
  instructions: string;
  /** The options of a choice question, in order. */
  options: DecisionOptionDraft[];
  /** The level descriptions of a score question, lowest first. */
  levels: string[];
}

/** The decision form: the state the questions are about, and the ordered questions. */
export interface DecisionForm {
  state: string;
  questions: DecisionQuestionDraft[];
}

/**
 * The operator's decision draft. It keeps its own shape rather than a request body, so a draft
 * that can't build a request yet, such as a question without a name, is still a draft. In the
 * JSON view the request is the body text, which the form may not be able to represent.
 */
export interface DecisionDraft {
  view: "form" | "json";
  form: DecisionForm;
  /** The request body text edited in the JSON view, without `model`. */
  jsonText?: string;
}

/** A question as it was asked, kept with its answers: its name, type, and labels in order. */
export interface AskedQuestion {
  name: string;
  type: DecisionQuestionType;
  /** Choice option names, or score level descriptions lowest first; empty for yes or no. */
  labels: readonly string[];
}

/** A request ready to send: the System One body text and the questions it asks, in order. */
export interface DecisionAskRequest {
  body: string;
  questions: readonly AskedQuestion[];
}

export const decisionQuestionTypes: readonly { label: string; value: DecisionQuestionType }[] = [
  { label: "Choice", value: "choice" },
  { label: "Score", value: "score" },
  { label: "Yes / no", value: "noul" },
];

/** The fewest and most levels a score question can have. */
export const SCORE_LEVELS = { min: 2, max: 10 } as const;

/** The request a draft asks, or why it can't build one yet. */
export function requestForDraft(draft: DecisionDraft): DecisionAskRequest | { problem: string } {
  if (draft.view === "json") {
    // The JSON view's text is sent as written; the inference API reports what it rejects.
    const body = draft.jsonText ?? "";
    const parsed = parseBody(body);
    return "problem" in parsed ? parsed : { body, questions: askedInBody(parsed.fields) };
  }

  const problem = formProblem(draft.form);
  if (problem) return { problem };
  return { body: formToBody(draft.form), questions: draft.form.questions.map(askedQuestion) };
}

/**
 * Why a form can't build a request body, or nothing when it can. Only what the body needs is
 * checked: named, unique and instructed questions, named choice options, and the level count.
 * The inference API reports everything else.
 */
export function formProblem(form: DecisionForm): string | undefined {
  if (form.questions.length === 0) return "Add at least one question.";

  const names = new Set<string>();
  for (const question of form.questions) {
    if (!question.name.trim()) return "Every question needs a name.";
    if (names.has(question.name)) return `Two questions are named ${question.name}.`;
    names.add(question.name);
    if (!question.instructions.trim()) return `${question.name} needs instructions.`;

    if (question.type === "choice") {
      if (question.options.length < 2) return `${question.name} needs at least 2 options.`;
      if (question.options.some((option) => !option.name.trim())) {
        return `Every option of ${question.name} needs a name.`;
      }
    }
    if (
      question.type === "score" &&
      (question.levels.length < SCORE_LEVELS.min || question.levels.length > SCORE_LEVELS.max)
    ) {
      return `${question.name} needs ${SCORE_LEVELS.min} to ${SCORE_LEVELS.max} levels.`;
    }
  }
  return undefined;
}

/**
 * Writes a form as System One request body text, without `model`. The text is written field by
 * field in the form's order, since the inference API reads question order from key order and a
 * JavaScript object would move integer-like names first. It is indented like a formatted JSON
 * document, so it reads well when edited as JSON.
 */
export function formToBody(form: DecisionForm): string {
  return writeJson(
    jsonObject([
      ["state", form.state],
      [
        "questions",
        jsonObject(form.questions.map((question) => [question.name, questionValue(question)])),
      ],
    ]),
    "",
  );
}

/**
 * The form a request body shows as, or why the form can't show it. The form holds a text state
 * and questions of its three types with their instructions and criteria; a body with anything
 * else stays in the JSON view. Missing text fields read as empty.
 */
export function bodyToForm(body: string): DecisionForm | { problem: string } {
  const parsed = parseBody(body);
  return "problem" in parsed ? parsed : formFromFields(parsed.fields);
}

/** Whether a form still asks the questions of an earlier request, by name and type, in order. */
export function askedMatches(
  asked: readonly Pick<AskedQuestion, "name" | "type">[],
  form: DecisionForm,
): boolean {
  return (
    asked.length === form.questions.length &&
    asked.every(
      (question, index) =>
        question.name === form.questions[index].name &&
        question.type === form.questions[index].type,
    )
  );
}

/** A new question for a form, named after its position and not yet instructed. */
export function newQuestion(form: DecisionForm): DecisionQuestionDraft {
  const names = new Set(form.questions.map((question) => question.name));
  let index = form.questions.length + 1;
  while (names.has(`q${index}`)) index += 1;
  return { name: `q${index}`, type: "noul", instructions: "", options: [], levels: [] };
}

/** The form with a new question at the end. */
export function addQuestion(form: DecisionForm): DecisionForm {
  return { ...form, questions: [...form.questions, newQuestion(form)] };
}

/** The form with the question at `index` replaced. */
export function replaceQuestion(
  form: DecisionForm,
  index: number,
  question: DecisionQuestionDraft,
): DecisionForm {
  return {
    ...form,
    questions: form.questions.map((entry, position) => (position === index ? question : entry)),
  };
}

/** The form without the question at `index`. */
export function removeQuestion(form: DecisionForm, index: number): DecisionForm {
  return { ...form, questions: form.questions.filter((_, position) => position !== index) };
}

function askedQuestion(question: DecisionQuestionDraft): AskedQuestion {
  const labels =
    question.type === "choice"
      ? question.options.map((option) => option.name)
      : question.type === "score"
        ? question.levels
        : [];
  return { name: question.name, type: question.type, labels };
}

function questionValue(question: DecisionQuestionDraft): JsonObject {
  const fields: [string, JsonValue][] = [
    ["type", question.type],
    ["instructions", question.instructions],
  ];
  if (question.type === "choice") {
    fields.push([
      "criteria",
      jsonObject(question.options.map((option) => [option.name, option.description || null])),
    ]);
  }
  if (question.type === "score") fields.push(["criteria", question.levels]);
  return jsonObject(fields);
}

/** A body's top-level members, or why it can't be sent. */
function parseBody(body: string): { fields: [string, JsonValue][] } | { problem: string } {
  const value = parseJson(body);
  if (value === undefined) return { problem: "The JSON doesn't parse." };
  if (!isJsonObject(value)) return { problem: "The request body must be a JSON object." };
  if (value.fields.some(([name]) => name === "model")) {
    return { problem: "Pick the route above; remove model." };
  }
  return { fields: value.fields };
}

function formFromFields(fields: [string, JsonValue][]): DecisionForm | { problem: string } {
  const repeated = repeatedName(fields);
  if (repeated) return { problem: `${repeated} is set twice.` };

  let state = "";
  let questions: DecisionQuestionDraft[] = [];
  for (const [name, value] of fields) {
    if (name === "state") {
      if (typeof value !== "string") return { problem: "The form only edits a text state." };
      state = value;
    } else if (name === "questions") {
      if (!isJsonObject(value)) return { problem: "questions must be an object of questions." };
      const drafts = value.fields.map(([questionName, entry]) =>
        questionFromJson(questionName, entry),
      );
      const problem = drafts.find((draft) => "problem" in draft);
      if (problem) return problem;
      questions = drafts as DecisionQuestionDraft[];
    } else {
      return { problem: `The form doesn't edit ${name}.` };
    }
  }
  return { state, questions };
}

function questionFromJson(
  name: string,
  value: JsonValue,
): DecisionQuestionDraft | { problem: string } {
  if (!isJsonObject(value)) return { problem: `${name} must be an object.` };
  const repeated = repeatedName(value.fields);
  if (repeated) return { problem: `${repeated} is set twice in ${name}.` };

  const fields = new Map(value.fields);
  const unknown = [...fields.keys()].find((field) => !questionFields.has(field));
  if (unknown) return { problem: `The form doesn't edit ${unknown} in ${name}.` };

  const type = fields.get("type");
  if (!decisionQuestionTypes.some((entry) => entry.value === type)) {
    return { problem: `${name} has a type the form doesn't edit.` };
  }
  const instructions = fields.get("instructions") ?? "";
  if (typeof instructions !== "string") {
    return { problem: `${name} instructions must be text.` };
  }

  const question: DecisionQuestionDraft = {
    name,
    type: type as DecisionQuestionType,
    instructions,
    options: [],
    levels: [],
  };
  const criteria = fields.get("criteria");
  if (question.type === "noul") {
    if (criteria !== undefined && criteria !== null) {
      return { problem: `The form doesn't edit yes / no criteria (${name}).` };
    }
  } else if (question.type === "choice") {
    const options = criteria === undefined ? [] : choiceOptions(criteria);
    if (!options) return { problem: `${name} criteria must name options with text or null.` };
    question.options = options;
  } else {
    const levels = criteria === undefined ? [] : scoreLevels(criteria);
    if (!levels) return { problem: `${name} criteria must list levels as text.` };
    question.levels = levels;
  }
  return question;
}

const questionFields = new Set(["type", "instructions", "criteria"]);

/**
 * The questions a body asks, in order, as far as they can be read: a question with a type the form
 * doesn't know is left out, and labels that aren't text read as empty.
 */
function askedInBody(fields: [string, JsonValue][]): AskedQuestion[] {
  const questions = fields.find(([name]) => name === "questions")?.[1];
  if (questions === undefined || !isJsonObject(questions)) return [];

  return questions.fields.flatMap(([name, value]) => {
    const fields = isJsonObject(value) ? new Map(value.fields) : undefined;
    const type = decisionQuestionTypes.find((entry) => entry.value === fields?.get("type"))?.value;
    if (!type) return [];

    const criteria = fields?.get("criteria");
    const labels =
      type === "choice" && criteria !== undefined && isJsonObject(criteria)
        ? criteria.fields.map(([option]) => option)
        : type === "score" && Array.isArray(criteria)
          ? criteria.map((level) => (typeof level === "string" ? level : ""))
          : [];
    return [{ name, type, labels }];
  });
}

function choiceOptions(criteria: JsonValue): DecisionOptionDraft[] | undefined {
  if (!isJsonObject(criteria)) return undefined;
  const options: DecisionOptionDraft[] = [];
  for (const [name, description] of criteria.fields) {
    if (description !== null && typeof description !== "string") return undefined;
    options.push({ name, description: description ?? "" });
  }
  return options;
}

function scoreLevels(criteria: JsonValue): string[] | undefined {
  if (!Array.isArray(criteria)) return undefined;
  return criteria.every((level) => typeof level === "string") ? criteria : undefined;
}

/** The first member name an object sets more than once. */
function repeatedName(fields: [string, JsonValue][]): string | undefined {
  const seen = new Set<string>();
  for (const [name] of fields) {
    if (seen.has(name)) return name;
    seen.add(name);
  }
  return undefined;
}
