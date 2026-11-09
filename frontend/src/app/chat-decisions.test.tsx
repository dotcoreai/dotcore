import { create } from "@bufbuild/protobuf";
import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createMemoryRouter, RouterProvider } from "react-router-dom";
import { describe, expect, it } from "vitest";

import type { DecisionClient, DecisionRequest, DecisionResult } from "@/api/decision-client";
import { ConsoleLayout } from "@/app/console-layout";
import { ChatPage } from "@/features/chat/chat-page";
import { type DecisionForm, formToBody } from "@/features/chat/decisions/decision-draft";
import { decisionExamples } from "@/features/chat/decisions/decision-examples";
import { createDecisionStore } from "@/features/chat/decisions/decision-store";
import { InferenceRequestKind } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import {
  InferenceRouteAvailabilitySchema,
  InferenceRouteSchema,
  InferenceRouteViewSchema,
  ListRoutesResponseSchema,
} from "@/gen/dotcore/inference/v1/routes_pb";
import type { StorageLike } from "@/lib/browser-storage";
import { FakeResourceWatch } from "@/test/fake-resource-watch";
import { FailingStorage, MemoryStorage } from "@/test/memory-storage";
import { ThemeProvider } from "@/theme/theme-provider";

/** A decision client whose answers the test releases one by one, in order. */
function fakeDecisionClient() {
  const requests: DecisionRequest[] = [];
  const pending: ((result: DecisionResult | undefined) => void)[] = [];
  const client: DecisionClient = {
    ask: (request) => {
      requests.push(request);
      return new Promise((resolve) => pending.push(resolve));
    },
  };
  const answer = async (result: DecisionResult) => {
    const resolve = pending.shift();
    if (!resolve) throw new Error("no request is waiting for an answer");
    // An async act flushes the state updates of the awaited answer.
    await act(() => {
      resolve(result);
      return Promise.resolve();
    });
  };
  return { answer, client, requests };
}

function decisionRoute(name: string) {
  return create(InferenceRouteViewSchema, {
    requestKinds: [InferenceRequestKind.DECISION],
    route: create(InferenceRouteSchema, { inferenceRouteId: `id-${name}`, name, enabled: true }),
    availability: create(InferenceRouteAvailabilitySchema, { available: true }),
  });
}

/** Opens the decision panel; `storage` holds the browser's, empty unless a test shares one. */
async function openDecisions(
  decisionClient: DecisionClient,
  storage: StorageLike = new MemoryStorage(),
) {
  const user = renderConsole(decisionClient, storage, "/models");
  await user.click(screen.getByRole("button", { name: "Chat" }));
  await waitFor(() => expect(panel()).toHaveAttribute("data-chat-mode", "decision"));
  return user;
}

/** Renders the console on a page, with the routes `clef` and `clef-q8` serving decisions. */
function renderConsole(decisionClient: DecisionClient, storage: StorageLike, entry: string) {
  const routes = create(ListRoutesResponseSchema, {
    routes: [decisionRoute("clef"), decisionRoute("clef-q8")],
  });
  const watch = new FakeResourceWatch({ sources: { routes: () => Promise.resolve(routes) } });
  const router = createMemoryRouter(
    [
      {
        element: (
          <ConsoleLayout
            decisionClient={decisionClient}
            eventTarget={new EventTarget()}
            resourceWatchClient={watch.client()}
            storage={storage}
          />
        ),
        children: [
          { path: "/models", element: <p>Models page</p> },
          { path: "/routes", element: <p>Routes page</p> },
          { path: "/chat", element: <ChatPage /> },
        ],
      },
    ],
    { initialEntries: [entry] },
  );
  render(
    <ThemeProvider document={document} eventTarget={new EventTarget()} storage={storage}>
      <RouterProvider router={router} />
    </ThemeProvider>,
  );
  return userEvent.setup();
}

const panel = () => {
  const element = document.querySelector<HTMLElement>('section[aria-label="Chat"]');
  if (!element) throw new Error("chat panel is not mounted");
  return element;
};

const askButton = () => within(panel()).getByRole("button", { name: /^Ask/ });

/** A question row of the panel, found by its summary line. */
const row = (name: string) => {
  const summary = within(panel()).getByRole("button", { name: new RegExp(`^${name} \\(`) });
  return summary.closest<HTMLElement>("[data-open]")!;
};

/** The verdict and probability lines of a row's answer, as text. */
const answerLines = (name: string) => {
  const answer = row(name).querySelector('[data-slot="decision-answer"]');
  if (!answer) return undefined;
  return [...answer.children].map((line) =>
    [...line.children]
      .map((part) => part.textContent)
      .filter(Boolean)
      .join(" | "),
  );
};

const supportTriage = decisionExamples[0].form;

const answered: DecisionResult = {
  kind: "answered",
  responseText: "{}",
  answers: new Map([
    [
      "topic",
      {
        type: "choice",
        choice: "billing",
        probabilities: new Map([
          ["other", 0.05],
          ["billing", 0.75],
          ["technical", 0.2],
        ]),
        confidence: 0.5,
      },
    ],
    ["urgency", { type: "score", score: 1.25, probabilities: [0.1, 0.55, 0.35], confidence: 0.4 }],
    ["refund", { type: "noul", yesProbability: 0.8 }],
  ]),
  usage: { inputTokens: 321 },
  latencyMs: 42.6,
};

const contextTooLarge: DecisionResult = {
  kind: "failed",
  responseText: "{}",
  error: {
    status: 400,
    type: "invalid_request_error",
    code: "context_length_exceeded",
    message: "The request is larger than the deployment's context.",
  },
};

describe("Chat decisions", () => {
  it("asks the draft and shows each answer in request order with latency and input tokens", async () => {
    const decisions = fakeDecisionClient();
    const user = await openDecisions(decisions.client);

    expect(within(panel()).getByRole("textbox", { name: "State" })).toHaveValue(
      supportTriage.state,
    );
    await user.click(askButton());

    expect(decisions.requests.map(({ body, model }) => ({ body, model }))).toEqual([
      { body: formToBody(supportTriage), model: "clef" },
    ]);
    expect(decisions.requests[0].signal?.aborted).toBe(false);
    expect(askButton()).toBeDisabled();
    expect(askButton()).toHaveTextContent("Asking…");

    await decisions.answer(answered);

    expect(answerLines("topic")).toEqual([
      "billing | 50% confidence",
      "billing | 75%",
      "technical | 20%",
      "other | 5%",
    ]);
    expect(answerLines("urgency")).toEqual([
      "somewhat urgent | level 1.25 of 2 · 40% confidence",
      "not urgent | 10%",
      "somewhat urgent | 55%",
      "urgent | 35%",
    ]);
    expect(answerLines("refund")).toEqual(["Yes | 80% probability of yes", "yes | 80%"]);
    expect(
      row("topic").querySelector('[data-top="true"]')?.querySelector("span")?.textContent,
    ).toBe("billing");
    expect(within(panel()).getByText("43 ms")).toBeVisible();
    expect(within(panel()).getByText("321").parentElement).toHaveTextContent("321 input tokens");
    expect(askButton()).toBeEnabled();
  });

  it("clears the answers on a failed Ask, shows its status, code and message, and retries it", async () => {
    const decisions = fakeDecisionClient();
    const user = await openDecisions(decisions.client);

    await user.click(askButton());
    await decisions.answer(answered);
    await user.click(askButton());
    await decisions.answer(contextTooLarge);

    expect(answerLines("topic")).toBeUndefined();
    expect(within(panel()).queryByText("43 ms")).toBeNull();
    const alert = within(panel()).getByRole("alert");
    expect(alert).toHaveTextContent(
      "400 context_length_exceeded · The request is larger than the deployment's context.",
    );

    await user.click(within(alert).getByRole("button", { name: "Retry" }));

    expect(decisions.requests.map((request) => request.body)).toEqual([
      formToBody(supportTriage),
      formToBody(supportTriage),
      formToBody(supportTriage),
    ]);
    expect(within(panel()).queryByRole("alert")).toBeNull();
    await decisions.answer(answered);
    expect(answerLines("refund")).toEqual(["Yes | 80% probability of yes", "yes | 80%"]);
  });

  it("shows a network failure's message with Retry", async () => {
    const decisions = fakeDecisionClient();
    const user = await openDecisions(decisions.client);

    await user.click(askButton());
    await decisions.answer({ kind: "unreachable", message: "Could not reach the inference API." });

    const alert = within(panel()).getByRole("alert");
    expect(alert).toHaveTextContent(/^Could not reach the inference API\.Retry$/);
  });

  it("drops the answer of a request still in flight when the route changes", async () => {
    const decisions = fakeDecisionClient();
    const user = await openDecisions(decisions.client);

    await user.click(askButton());
    await user.click(within(panel()).getByRole("button", { name: "Show Route options" }));
    await user.click(await screen.findByRole("option", { name: /^clef-q8/ }));

    expect(decisions.requests[0].signal?.aborted).toBe(true);
    expect(askButton()).toHaveTextContent(/^Ask$/);
    await decisions.answer(answered);

    expect(answerLines("topic")).toBeUndefined();
    expect(within(panel()).queryByText("43 ms")).toBeNull();
  });

  it("keeps the first available route's answers across closing the panel, in flight too", async () => {
    const decisions = fakeDecisionClient();
    const user = await openDecisions(decisions.client);
    const toggleChat = () => user.click(screen.getByRole("button", { name: "Chat" }));

    await user.click(askButton());
    await toggleChat();
    expect(decisions.requests[0].signal?.aborted).toBe(false);
    await decisions.answer(answered);
    await toggleChat();
    expect(answerLines("refund")).toEqual(["Yes | 80% probability of yes", "yes | 80%"]);

    await toggleChat();
    await toggleChat();
    expect(answerLines("refund")).toEqual(["Yes | 80% probability of yes", "yes | 80%"]);
    expect(within(panel()).getByText("43 ms")).toBeVisible();
  });

  it("keeps the answers of a Chat page entered without a route when shown as a panel", async () => {
    const decisions = fakeDecisionClient();
    const user = renderConsole(decisions.client, new MemoryStorage(), "/chat");
    await waitFor(() => expect(pageAsk()).toBeEnabled());

    await user.click(pageAsk());
    await decisions.answer(answered);
    await user.click(within(page()).getByRole("button", { name: "Show as panel" }));

    await screen.findByText("Routes page");
    expect(decisions.requests.map((request) => request.model)).toEqual(["clef"]);
    expect(answerLines("refund")).toEqual(["Yes | 80% probability of yes", "yes | 80%"]);
  });

  it("keeps the answers through a state edit and clears them when a question is renamed", async () => {
    const decisions = fakeDecisionClient();
    const user = await openDecisions(decisions.client);
    await user.click(askButton());
    await decisions.answer(answered);

    await user.type(within(panel()).getByRole("textbox", { name: "State" }), " Thanks.");
    expect(answerLines("refund")).toEqual(["Yes | 80% probability of yes", "yes | 80%"]);

    await user.click(within(panel()).getByRole("button", { name: /^refund \(/ }));
    await user.type(within(row("refund")).getByRole("textbox", { name: "Question name" }), "s");

    expect(answerLines("topic")).toBeUndefined();
    expect(answerLines("refunds")).toBeUndefined();
    expect(within(panel()).queryByText("43 ms")).toBeNull();
  });

  it("removes a collapsed question from its header, with one remove control per question", async () => {
    const decisions = fakeDecisionClient();
    const user = await openDecisions(decisions.client);
    const questionNames = () =>
      [...panel().querySelectorAll("[data-open] [aria-expanded]")].map((summary) =>
        summary.getAttribute("aria-label"),
      );
    expect(questionNames()).toEqual(["topic (Choice)", "urgency (Score)", "refund (Yes / no)"]);

    await user.click(within(panel()).getByRole("button", { name: "Remove urgency" }));

    expect(questionNames()).toEqual(["topic (Choice)", "refund (Yes / no)"]);
    await user.click(within(panel()).getByRole("button", { name: /^refund \(/ }));
    expect(within(row("refund")).getAllByRole("button", { name: "Remove refund" })).toHaveLength(1);
  });

  it("clears the answers from the header", async () => {
    const decisions = fakeDecisionClient();
    const user = await openDecisions(decisions.client);
    await user.click(askButton());
    await decisions.answer(answered);

    await user.click(within(panel()).getByRole("button", { name: "Clear answers" }));

    expect(answerLines("topic")).toBeUndefined();
    expect(within(panel()).queryByText("43 ms")).toBeNull();
  });

  it("clears the answers when a question's type changes", async () => {
    const decisions = fakeDecisionClient();
    const user = await openDecisions(decisions.client);
    await user.click(askButton());
    await decisions.answer(answered);

    await user.click(within(panel()).getByRole("button", { name: /^refund \(/ }));
    await user.selectOptions(
      within(row("refund")).getByRole("combobox", { name: "Question type" }),
      "score",
    );

    expect(answerLines("topic")).toBeUndefined();
    expect(answerLines("refund")).toBeUndefined();
    expect(within(panel()).queryByText("43 ms")).toBeNull();
  });

  it("disables Ask with the reason while the draft can't build a request", async () => {
    const decisions = fakeDecisionClient();
    const user = await openDecisions(decisions.client);

    await user.click(within(panel()).getByRole("button", { name: /^topic \(/ }));
    await user.clear(within(row("topic")).getByRole("textbox", { name: "Question name" }));

    expect(askButton()).toBeDisabled();
    expect(within(panel()).getByRole("status")).toHaveTextContent("Every question needs a name.");
    await user.click(askButton());
    expect(decisions.requests).toEqual([]);
  });

  it("loads an example and asks it", async () => {
    const decisions = fakeDecisionClient();
    const user = await openDecisions(decisions.client);

    await user.selectOptions(
      within(panel()).getByRole("combobox", { name: "Load example" }),
      "Security alert",
    );
    await user.click(askButton());

    expect(within(panel()).getByText("Questions (2)")).toBeVisible();
    expect(decisions.requests[0].body).toBe(formToBody(decisionExamples[2].form));
  });
});

/** Opens the decision panel, then expands it to the Chat page. */
async function openPage(decisionClient: DecisionClient, storage?: StorageLike) {
  const user = await openDecisions(decisionClient, storage);
  await user.click(within(panel()).getByRole("button", { name: "Open the Chat page" }));
  await screen.findByRole("region", { name: "Chat" });
  return user;
}

const page = () => screen.getByRole("region", { name: "Chat" });

const pageAsk = () => within(page()).getByRole("button", { name: /^Ask/ });

const requestBody = () => within(page()).getByRole("textbox", { name: "POST /v1/systemone" });

async function switchEditor(user: ReturnType<typeof userEvent.setup>, view: "Form" | "JSON") {
  const editor = within(page()).getByRole("radiogroup", { name: "Editor" });
  await user.click(within(editor).getByRole("radio", { name: view }));
}

async function replaceBody(user: ReturnType<typeof userEvent.setup>, text: string) {
  await user.clear(requestBody());
  await user.click(requestBody());
  await user.paste(text);
}

/** The verdict and probability lines of every answer under a container, by question. */
const answerBlocks = (container: HTMLElement) =>
  [...container.querySelectorAll<HTMLElement>('[data-slot="decision-answer"]')].map((answer) =>
    [...answer.children].map((line) =>
      [...line.children]
        .map((part) => part.textContent)
        .filter(Boolean)
        .join(" | "),
    ),
  );

describe("Chat page decisions", () => {
  const retyped: DecisionForm = {
    state: "Server down since noon.",
    questions: [
      {
        name: "2",
        type: "score",
        instructions: "How bad?",
        options: [],
        levels: ["minor", "major"],
      },
      { name: "page", type: "noul", instructions: "Page someone?", options: [], levels: [] },
    ],
  };

  it("edits the request as JSON and takes it back into the form", async () => {
    const decisions = fakeDecisionClient();
    const user = await openPage(decisions.client);

    expect(within(page()).getByRole("textbox", { name: "State" })).toHaveValue(supportTriage.state);
    await switchEditor(user, "JSON");
    expect(requestBody()).toHaveValue(formToBody(supportTriage));

    await replaceBody(user, formToBody(retyped));
    await switchEditor(user, "Form");

    expect(within(page()).getByRole("textbox", { name: "State" })).toHaveValue(retyped.state);
    expect(
      within(page())
        .getAllByRole("textbox", { name: "Question name" })
        .map((input) => (input as HTMLInputElement).value),
    ).toEqual(["2", "page"]);
    await user.click(pageAsk());
    expect(decisions.requests.map((request) => request.body)).toEqual([formToBody(retyped)]);
  });

  it("keeps a body the form can't show in JSON with its reason, and the panel notes it", async () => {
    const decisions = fakeDecisionClient();
    const user = await openPage(decisions.client);
    const jsonOnly =
      '{"state": {"ticket": 7}, "questions": {"q": {"type": "noul", "instructions": "?"}}}';

    await switchEditor(user, "JSON");
    await replaceBody(user, jsonOnly);
    await switchEditor(user, "Form");

    expect(within(page()).getByRole("radio", { name: "JSON" })).toBeChecked();
    expect(requestBody()).toHaveValue(jsonOnly);
    expect(within(page()).getByRole("status")).toHaveTextContent(
      "The form only edits a text state.",
    );
    await user.click(pageAsk());
    expect(decisions.requests.map(({ body, model }) => ({ body, model }))).toEqual([
      { body: jsonOnly, model: "clef" },
    ]);

    await user.click(within(page()).getByRole("button", { name: "Show as panel" }));
    const notice = within(panel()).getByRole("status");
    expect(notice).toHaveTextContent("Edited as JSON");
    expect(within(notice).getByRole("button", { name: "Open the Chat page" })).toBeVisible();
    expect(within(panel()).queryByRole("textbox", { name: "State" })).toBeNull();
  });

  it("blocks Ask while the JSON doesn't parse or sets model, with the reason", async () => {
    const decisions = fakeDecisionClient();
    const user = await openPage(decisions.client);
    await switchEditor(user, "JSON");

    await replaceBody(user, '{"state": "s",');
    expect(pageAsk()).toBeDisabled();
    expect(within(page()).getByRole("status")).toHaveTextContent("The JSON doesn't parse.");

    await replaceBody(user, '{"model": "other", "state": "s", "questions": {}}');
    expect(pageAsk()).toBeDisabled();
    expect(within(page()).getByRole("status")).toHaveTextContent(
      "Pick the route above; remove model.",
    );
    await user.click(pageAsk());
    expect(decisions.requests).toEqual([]);
  });

  it("clears the answers of a body the form can't show once the JSON asks the form's questions", async () => {
    const decisions = fakeDecisionClient();
    const user = await openPage(decisions.client);
    await switchEditor(user, "JSON");
    await replaceBody(user, '{"state": 1, "questions": {"refund": {"type": "noul"}}}');
    await user.click(pageAsk());
    await decisions.answer(answered);
    expect(answerBlocks(page())[0]).toEqual(["Yes | 80% probability of yes", "yes | 80%"]);
    expect(within(page()).getByText("43 ms")).toBeVisible();

    await replaceBody(user, formToBody(supportTriage));

    expect(answerBlocks(page())).toEqual([]);
    expect(within(page()).queryByText("43 ms")).toBeNull();
  });

  it("shows the answers in request order, the response JSON indented, and copies both", async () => {
    const decisions = fakeDecisionClient();
    const user = await openPage(decisions.client);
    const responseText =
      '{"model":"clef","answers":{"refund":{"type":"noul","noul":0.8},"2":{"type":"noul","noul":0.1}}} ';
    const formatted = [
      "{",
      '  "model": "clef",',
      '  "answers": {',
      '    "refund": {',
      '      "type": "noul",',
      '      "noul": 0.8',
      "    },",
      '    "2": {',
      '      "type": "noul",',
      '      "noul": 0.1',
      "    }",
      "  }",
      "}",
    ].join("\n");
    await switchEditor(user, "JSON");

    await user.click(pageAsk());
    await decisions.answer({ ...answered, responseText });

    expect(answerBlocks(page())).toEqual([
      ["billing | 50% confidence", "billing | 75%", "technical | 20%", "other | 5%"],
      [
        "somewhat urgent | level 1.25 of 2 · 40% confidence",
        "not urgent | 10%",
        "somewhat urgent | 55%",
        "urgent | 35%",
      ],
      ["Yes | 80% probability of yes", "yes | 80%"],
    ]);
    await user.click(within(page()).getByRole("radio", { name: "Response JSON" }));
    expect(page().querySelector('[data-slot="decision-response"]')?.textContent).toBe(formatted);

    await user.click(within(page()).getByRole("button", { name: "Copy response" }));
    expect(await navigator.clipboard.readText()).toBe(formatted);
    await user.click(within(page()).getByRole("button", { name: "Copy request" }));
    expect(await navigator.clipboard.readText()).toBe(
      `{"model":"clef",${formToBody(supportTriage).slice(1)}`,
    );

    await user.click(within(page()).getByRole("button", { name: "Show as panel" }));
    expect(answerLines("refund")).toEqual(["Yes | 80% probability of yes", "yes | 80%"]);
  });

  it("shows a response that isn't JSON as it arrived", async () => {
    const decisions = fakeDecisionClient();
    const user = await openPage(decisions.client);
    const responseText = "upstream connect error {";
    await switchEditor(user, "JSON");

    await user.click(pageAsk());
    await decisions.answer({ ...contextTooLarge, responseText });
    await user.click(within(page()).getByRole("radio", { name: "Response JSON" }));

    expect(page().querySelector('[data-slot="decision-response"]')?.textContent).toBe(responseText);
    await user.click(within(page()).getByRole("button", { name: "Copy response" }));
    expect(await navigator.clipboard.readText()).toBe(responseText);
  });

  it("shows the answers of a body the form can't show, the questions it names first", async () => {
    const decisions = fakeDecisionClient();
    const user = await openPage(decisions.client);
    await switchEditor(user, "JSON");
    await replaceBody(
      user,
      '{"state": 1, "questions": {"refund": {"type": "noul"}, "urgency": {"type": "rank"}}}',
    );

    await user.click(pageAsk());
    await decisions.answer(answered);

    expect(answerBlocks(page())).toEqual([
      ["Yes | 80% probability of yes", "yes | 80%"],
      ["billing | 50% confidence", "other | 5%", "billing | 75%", "technical | 20%"],
      [
        "Level 1 | level 1.25 of 2 · 40% confidence",
        "Level 0 | 10%",
        "Level 1 | 55%",
        "Level 2 | 35%",
      ],
    ]);
  });
});

/** The Saved sets menu, opened from the Chat page's toolbar. */
async function openSets(user: ReturnType<typeof userEvent.setup>) {
  await user.click(within(page()).getByRole("button", { name: /^Saved sets/ }));
  return screen.findByRole("dialog", { name: "Saved sets" });
}

const setNames = (menu: HTMLElement) =>
  within(menu)
    .queryAllByRole("listitem")
    .map((item) => item.textContent);

describe("Chat decision storage", () => {
  it("restores a draft that can't build a request after a reload", async () => {
    const storage = new MemoryStorage();
    const decisions = fakeDecisionClient();
    const user = await openDecisions(decisions.client, storage);

    await user.type(within(panel()).getByRole("textbox", { name: "State" }), " Thanks.");
    await user.click(within(panel()).getByRole("button", { name: /^topic \(/ }));
    await user.clear(within(row("topic")).getByRole("textbox", { name: "Question name" }));
    cleanup();

    await openDecisions(decisions.client, storage);

    expect(within(panel()).getByRole("textbox", { name: "State" })).toHaveValue(
      `${supportTriage.state} Thanks.`,
    );
    expect(within(panel()).getByText("Questions (3)")).toBeVisible();
    expect(within(panel()).getByRole("status")).toHaveTextContent("Every question needs a name.");
    expect(askButton()).toBeDisabled();
  });

  it("restores JSON text that doesn't parse after a reload", async () => {
    const storage = new MemoryStorage();
    const decisions = fakeDecisionClient();
    const user = await openPage(decisions.client, storage);
    await switchEditor(user, "JSON");
    await replaceBody(user, '{"state": "s",');
    cleanup();

    const reloaded = await openDecisions(decisions.client, storage);
    const notices = within(panel()).getAllByRole("status");
    expect(notices.map((notice) => notice.textContent)).toEqual([
      "Edited as JSONOpen the Chat page",
      "The JSON doesn't parse.",
    ]);
    const notice = notices[0];
    await reloaded.click(within(notice).getByRole("button", { name: "Open the Chat page" }));
    await screen.findByRole("region", { name: "Chat" });

    expect(within(page()).getByRole("radio", { name: "JSON" })).toBeChecked();
    expect(requestBody()).toHaveValue('{"state": "s",');
    expect(within(page()).getByRole("status")).toHaveTextContent("The JSON doesn't parse.");
    expect(pageAsk()).toBeDisabled();
  });

  it("saves, loads and deletes a named set", async () => {
    const storage = new MemoryStorage();
    const decisions = fakeDecisionClient();
    const user = await openPage(decisions.client, storage);

    let menu = await openSets(user);
    expect(menu).toHaveTextContent("No saved sets in this browser.");
    await user.type(within(menu).getByRole("textbox", { name: "Set name" }), "  Triage  {Enter}");
    expect(setNames(menu)).toEqual(["Triage"]);
    expect(within(menu).getByRole("textbox", { name: "Set name" })).toHaveValue("");
    expect(createDecisionStore(storage).readSets()).toEqual([
      { name: "Triage", body: formToBody(supportTriage) },
    ]);
    await user.keyboard("{Escape}");
    expect(within(page()).getByRole("button", { name: /^Saved sets/ })).toHaveTextContent(
      "Saved sets1",
    );

    await user.selectOptions(
      within(page()).getByRole("combobox", { name: "Load example" }),
      "Security alert",
    );
    menu = await openSets(user);
    await user.click(within(menu).getByRole("button", { name: "Triage" }));

    expect(screen.queryByRole("dialog", { name: "Saved sets" })).toBeNull();
    expect(within(page()).getByRole("textbox", { name: "State" })).toHaveValue(supportTriage.state);
    await user.click(pageAsk());
    expect(decisions.requests.map((request) => request.body)).toEqual([formToBody(supportTriage)]);

    menu = await openSets(user);
    await user.click(within(menu).getByRole("button", { name: "Delete Triage" }));
    expect(menu).toHaveTextContent("No saved sets in this browser.");
    expect(createDecisionStore(storage).readSets()).toEqual([]);
  });

  it("lists sets saved from another tab and opens a set the form can't show as JSON", async () => {
    const storage = new MemoryStorage();
    const decisions = fakeDecisionClient();
    const user = await openPage(decisions.client, storage);
    const jsonOnly = '{"state": {"ticket": 7}, "questions": {"10": {"type": "noul"}, "2": {}}}';

    createDecisionStore(storage).saveSet({ name: "Ticket", body: jsonOnly });
    const menu = await openSets(user);
    expect(setNames(menu)).toEqual(["Ticket"]);
    await user.click(within(menu).getByRole("button", { name: "Ticket" }));

    expect(within(page()).getByRole("radio", { name: "JSON" })).toBeChecked();
    expect(requestBody()).toHaveValue(jsonOnly);
    await user.click(pageAsk());
    expect(decisions.requests.map((request) => request.body)).toEqual([jsonOnly]);
  });

  it("disables Save with the reason while the draft can't build a request", async () => {
    const decisions = fakeDecisionClient();
    const user = await openPage(decisions.client);
    await user.clear(within(page()).getAllByRole("textbox", { name: "Question name" })[0]);

    const menu = await openSets(user);
    await user.type(within(menu).getByRole("textbox", { name: "Set name" }), "Broken{Enter}");

    const save = within(menu).getByRole("button", { name: "Save" });
    expect(save).toBeDisabled();
    expect(save).toHaveAccessibleDescription("Every question needs a name.");
    expect(setNames(menu)).toEqual([]);
  });

  it("keeps the chat working when browser storage fails", async () => {
    const decisions = fakeDecisionClient();
    const user = await openDecisions(decisions.client, new FailingStorage());

    await user.type(within(panel()).getByRole("textbox", { name: "State" }), " Thanks.");
    await user.click(askButton());
    await decisions.answer(answered);

    expect(decisions.requests.map((request) => request.body)).toEqual([
      formToBody({ ...supportTriage, state: `${supportTriage.state} Thanks.` }),
    ]);
    expect(answerLines("refund")).toEqual(["Yes | 80% probability of yes", "yes | 80%"]);
  });
});
