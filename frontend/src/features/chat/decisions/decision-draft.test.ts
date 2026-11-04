import { describe, expect, it } from "vitest";

import {
  askedMatches,
  bodyToForm,
  type DecisionForm,
  type DecisionQuestionDraft,
  formProblem,
  formToBody,
  newQuestion,
  requestForDraft,
} from "@/features/chat/decisions/decision-draft";

function question(
  fields: Partial<DecisionQuestionDraft> & { name: string },
): DecisionQuestionDraft {
  return { type: "noul", instructions: "Is it?", options: [], levels: [], ...fields };
}

const form: DecisionForm = {
  state: 'Line one\n"quoted"',
  questions: [
    question({
      name: "10",
      type: "choice",
      instructions: "Which topic?",
      options: [
        { name: "billing", description: "Charges" },
        { name: "2", description: "" },
      ],
    }),
    question({
      name: "urgency",
      type: "score",
      instructions: "How urgent?",
      levels: ["low", "mid", "high"],
      // Options typed before switching to score are not sent.
      options: [{ name: "stale", description: "" }],
    }),
    question({ name: "1", instructions: "Refund?", levels: ["stale"] }),
  ],
};

describe("decision draft", () => {
  it("writes the body in form order, integer-like names included", () => {
    expect(formToBody(form)).toBe(
      [
        "{",
        '  "state": "Line one\\n\\"quoted\\"",',
        '  "questions": {',
        '    "10": {',
        '      "type": "choice",',
        '      "instructions": "Which topic?",',
        '      "criteria": {',
        '        "billing": "Charges",',
        '        "2": null',
        "      }",
        "    },",
        '    "urgency": {',
        '      "type": "score",',
        '      "instructions": "How urgent?",',
        '      "criteria": [',
        '        "low",',
        '        "mid",',
        '        "high"',
        "      ]",
        "    },",
        '    "1": {',
        '      "type": "noul",',
        '      "instructions": "Refund?"',
        "    }",
        "  }",
        "}",
      ].join("\n"),
    );
  });

  it("writes a body JSON.parse reads back to the same values", () => {
    expect(JSON.parse(formToBody(form))).toEqual({
      state: 'Line one\n"quoted"',
      questions: {
        "10": {
          type: "choice",
          instructions: "Which topic?",
          criteria: { billing: "Charges", "2": null },
        },
        urgency: { type: "score", instructions: "How urgent?", criteria: ["low", "mid", "high"] },
        "1": { type: "noul", instructions: "Refund?" },
      },
    });
  });

  it("builds a request with the asked questions and their labels in order", () => {
    expect(requestForDraft({ view: "form", form })).toEqual({
      body: formToBody(form),
      questions: [
        { name: "10", type: "choice", labels: ["billing", "2"] },
        { name: "urgency", type: "score", labels: ["low", "mid", "high"] },
        { name: "1", type: "noul", labels: [] },
      ],
    });
  });

  it("gives the first reason a form can't build a body", () => {
    const withQuestions = (...questions: DecisionQuestionDraft[]) => ({ state: "", questions });
    const choice = (options: string[]) =>
      question({
        name: "pick",
        type: "choice",
        options: options.map((name) => ({ name, description: "" })),
      });
    const score = (levels: number) =>
      question({ name: "rate", type: "score", levels: Array.from({ length: levels }, String) });

    expect(formProblem(withQuestions())).toBe("Add at least one question.");
    expect(formProblem(withQuestions(question({ name: " " })))).toBe(
      "Every question needs a name.",
    );
    expect(formProblem(withQuestions(question({ name: "a" }), question({ name: "a" })))).toBe(
      "Two questions are named a.",
    );
    expect(formProblem(withQuestions(question({ name: "a", instructions: "" })))).toBe(
      "a needs instructions.",
    );
    expect(formProblem(withQuestions(choice(["one"])))).toBe("pick needs at least 2 options.");
    expect(formProblem(withQuestions(choice(["one", ""])))).toBe(
      "Every option of pick needs a name.",
    );
    expect(formProblem(withQuestions(score(1)))).toBe("rate needs 2 to 10 levels.");
    expect(formProblem(withQuestions(score(11)))).toBe("rate needs 2 to 10 levels.");
    expect(formProblem(withQuestions(score(2), score(10)))).toBe("Two questions are named rate.");
    expect(formProblem(withQuestions(choice(["one", "two"]), score(10)))).toBeUndefined();
    expect(requestForDraft({ view: "form", form: withQuestions() })).toEqual({
      problem: "Add at least one question.",
    });
  });

  it("matches asked questions to a form by name and type in order", () => {
    const named = (...names: string[]) => ({
      state: "",
      questions: names.map((name) => question({ name })),
    });
    const asked = (...names: string[]) => named(...names).questions;
    const retyped = { state: "", questions: [question({ name: "a", type: "score" })] };

    expect(askedMatches(asked("a", "b"), named("a", "b"))).toBe(true);
    expect(askedMatches(asked("a", "b"), named("b", "a"))).toBe(false);
    expect(askedMatches(asked("a", "b"), named("a"))).toBe(false);
    expect(askedMatches(asked("a"), named("ab"))).toBe(false);
    expect(askedMatches(asked("a"), retyped)).toBe(false);
  });

  it("names a new question after the first free position", () => {
    const named = (...names: string[]) => ({
      state: "",
      questions: names.map((name) => question({ name })),
    });

    expect(newQuestion(named("a", "b"))).toEqual(question({ name: "q3", instructions: "" }));
    expect(newQuestion(named("q2", "q3")).name).toBe("q4");
  });

  it("reads a written body back as the form, keeping only each type's entries", () => {
    expect(bodyToForm(formToBody(form))).toEqual({
      state: 'Line one\n"quoted"',
      questions: [
        question({
          name: "10",
          type: "choice",
          instructions: "Which topic?",
          options: [
            { name: "billing", description: "Charges" },
            { name: "2", description: "" },
          ],
        }),
        question({
          name: "urgency",
          type: "score",
          instructions: "How urgent?",
          levels: ["low", "mid", "high"],
        }),
        question({ name: "1", instructions: "Refund?" }),
      ],
    });
  });

  it("reads questions in body order, with repeated names and missing text kept for the form", () => {
    const body =
      '{"questions": {"b": {"type": "noul", "instructions": "B?", "criteria": null},' +
      ' "2": {"type": "choice", "criteria": {"x": null, "x": "again"}},' +
      ' "b": {"type": "score", "instructions": "", "criteria": []}}}';

    expect(bodyToForm(body)).toEqual({
      state: "",
      questions: [
        question({ name: "b", instructions: "B?" }),
        question({
          name: "2",
          type: "choice",
          instructions: "",
          options: [
            { name: "x", description: "" },
            { name: "x", description: "again" },
          ],
        }),
        question({ name: "b", type: "score", instructions: "" }),
      ],
    });
  });

  it("gives the reason the form can't show a body", () => {
    const reason = (body: string) => {
      const result = bodyToForm(body);
      return "problem" in result ? result.problem : undefined;
    };
    const asking = (questionJson: string) => `{"questions": {"q": ${questionJson}}}`;

    expect(reason('{"state": ')).toBe("The JSON doesn't parse.");
    expect(reason("[]")).toBe("The request body must be a JSON object.");
    expect(reason('{"model": "clef", "state": ""}')).toBe("Pick the route above; remove model.");
    expect(reason('{"state": "a", "state": "b"}')).toBe("state is set twice.");
    expect(reason('{"stream": false}')).toBe("The form doesn't edit stream.");
    expect(reason('{"state": {"ticket": 1}}')).toBe("The form only edits a text state.");
    expect(reason('{"questions": []}')).toBe("questions must be an object of questions.");
    expect(reason(asking('"Is it?"'))).toBe("q must be an object.");
    expect(reason(asking('{"type": "noul", "type": "noul"}'))).toBe("type is set twice in q.");
    expect(reason(asking('{"type": "noul", "weight": 1}'))).toBe(
      "The form doesn't edit weight in q.",
    );
    expect(reason(asking('{"type": "rank"}'))).toBe("q has a type the form doesn't edit.");
    expect(reason(asking('{"type": "noul", "instructions": ["a"]}'))).toBe(
      "q instructions must be text.",
    );
    expect(reason(asking('{"type": "noul", "criteria": {"true": "Yes"}}'))).toBe(
      "The form doesn't edit yes / no criteria (q).",
    );
    expect(reason(asking('{"type": "choice", "criteria": {"a": {"area": "x"}}}'))).toBe(
      "q criteria must name options with text or null.",
    );
    expect(reason(asking('{"type": "score", "criteria": ["low", 2]}'))).toBe(
      "q criteria must list levels as text.",
    );
  });

  it("sends the JSON view's body as written, with the questions it can read in order", () => {
    const shown = '{"state": "s", "questions": {"2": {"type": "noul", "instructions": "?"}}}';
    const jsonOnly =
      '{"state": {"ticket": 1}, "questions": {"10": {"type": "rank"}, "2": "?",' +
      ' "s": {"type": "score", "criteria": ["low", {"level": 2}]},' +
      ' "c": {"type": "choice", "criteria": {"b": {}, "a": null}}}}';
    const json = (jsonText: string) => requestForDraft({ view: "json", form, jsonText });

    expect(json(shown)).toEqual({
      body: shown,
      questions: [{ name: "2", type: "noul", labels: [] }],
    });
    expect(json(jsonOnly)).toEqual({
      body: jsonOnly,
      questions: [
        { name: "s", type: "score", labels: ["low", ""] },
        { name: "c", type: "choice", labels: ["b", "a"] },
      ],
    });
    expect(json("{")).toEqual({ problem: "The JSON doesn't parse." });
    expect(json('"text"')).toEqual({ problem: "The request body must be a JSON object." });
    expect(json('{"model": "clef"}')).toEqual({ problem: "Pick the route above; remove model." });
  });
});
