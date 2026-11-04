import { useId, useState } from "react";

import { type DecisionAnswer, decisionRequestText } from "@/api/decision-client";
import { CodeCopyControl } from "@/components/ui/copy-control";
import { Notice } from "@/components/ui/notice";
import { SegmentedControl, type SegmentedControlOption } from "@/components/ui/segmented-control";
import { Textarea } from "@/components/ui/textarea";
import { useChatDecisions, useChatRouteSession } from "@/features/chat/chat-session-context";
import {
  DecisionAnswerPlaceholder,
  DecisionAnswerView,
} from "@/features/chat/decisions/decision-answer";
import type { AskedQuestion } from "@/features/chat/decisions/decision-draft";
import { DecisionQuestionTypeBadge } from "@/features/chat/decisions/decision-question-row";
import { parseJson, writeJson } from "@/features/chat/decisions/ordered-json";
import type { DecisionRun } from "@/features/chat/decisions/use-decisions";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";

/**
 * The request body edited as JSON beside the last Ask's response, shown as answers or as the
 * response JSON, each with a copy control. `formReason` says why the form can't show the body.
 */
export function DecisionJsonEditor({ formReason }: { formReason?: string }) {
  const { draft, editJson, problem, run } = useChatDecisions();
  const { route } = useChatRouteSession();
  const [responseView, setResponseView] = useState<ResponseView>("answers");
  const bodyId = useId();
  const body = draft.jsonText ?? "";
  const responseText =
    run?.result && "responseText" in run.result
      ? formatResponse(run.result.responseText)
      : undefined;

  return (
    <div className="grid items-start gap-4 lg:grid-cols-2">
      <div className="grid gap-2 rounded-lg border bg-card p-3">
        <div className="flex items-center justify-between gap-2">
          <label className="font-mono text-sm font-semibold" htmlFor={bodyId}>
            POST /v1/systemone
          </label>
          <CodeCopyControl
            label="request"
            value={route && !problem ? decisionRequestText(route, body) : body}
          />
        </div>
        <p className="text-xs text-muted-foreground">
          The route picker sets <code className="font-mono">model</code>.
        </p>
        <Textarea
          className="min-h-80 font-mono text-xs md:text-xs pointer-coarse:text-xs"
          id={bodyId}
          onChange={(event) => editJson(event.target.value)}
          spellCheck={false}
          value={body}
        />
        {formReason ? (
          <Notice role="status" tone="warning">
            {formReason}
          </Notice>
        ) : null}
      </div>
      <div className="grid gap-3 rounded-lg border bg-card p-3">
        <div className="flex items-center justify-between gap-2">
          <SegmentedControl
            aria-label="Response view"
            onValueChange={setResponseView}
            options={responseViewOptions}
            value={responseView}
          />
          {responseText !== undefined ? (
            <CodeCopyControl label="response" value={responseText} />
          ) : null}
        </div>
        {responseView === "answers" ? (
          <ResponseAnswers run={run} />
        ) : responseText !== undefined ? (
          <pre
            className="max-h-[28rem] overflow-auto rounded-lg border bg-muted p-3 font-mono text-xs break-all whitespace-pre-wrap"
            data-slot="decision-response"
          >
            {responseText}
          </pre>
        ) : run?.result ? (
          <EmptyResponse text="No response" />
        ) : (
          <ResponsePending run={run} />
        )}
      </div>
    </div>
  );
}

type ResponseView = "answers" | "raw";

const responseViewOptions: readonly SegmentedControlOption<ResponseView>[] = [
  { label: "Answers", value: "answers" },
  { label: "Response JSON", value: "raw" },
];

/** Every answer of the last Ask, in the order its request asked them. */
function ResponseAnswers({ run }: { run: DecisionRun | undefined }) {
  if (!run?.result) return <ResponsePending run={run} />;
  if (run.result.kind !== "answered") return <EmptyResponse text="No answers" />;

  const { answers } = run.result;
  return (
    <div className="grid gap-2">
      {answeredQuestions(run.request.questions, answers).map((question, index) => (
        <div
          className="grid gap-2 rounded-lg border p-2.5"
          data-slot="decision-question"
          key={index}
        >
          <div className="flex items-center gap-1.5">
            <span className="font-mono text-[12.5px] font-semibold">{question.name}</span>
            <DecisionQuestionTypeBadge type={question.type} />
          </div>
          <DecisionAnswerView answer={answers.get(question.name)} question={question} />
        </div>
      ))}
    </div>
  );
}

/** Before the first Ask, a hint to ask; while one is in flight, placeholder bars once slow. */
function ResponsePending({ run }: { run: DecisionRun | undefined }) {
  const placeholders = useDelayedLoading(run !== undefined);
  if (!run) return <EmptyResponse text="Ask to see the response." />;
  return placeholders ? <DecisionAnswerPlaceholder /> : null;
}

function EmptyResponse({ text }: { text: string }) {
  return <p className="text-xs text-muted-foreground">{text}</p>;
}

/** A response indented with its members in the order received, or as it arrived when not JSON. */
function formatResponse(text: string): string {
  const value = parseJson(text);
  return value === undefined ? text : writeJson(value);
}

/**
 * The questions an Ask's answers belong to: the ones its request asked, in order, then one per
 * answer the request didn't name, with labels taken from the answer.
 */
function answeredQuestions(
  asked: readonly AskedQuestion[],
  answers: ReadonlyMap<string, DecisionAnswer>,
): readonly AskedQuestion[] {
  const named = new Set(asked.map((question) => question.name));
  const unnamed = [...answers]
    .filter(([name]) => !named.has(name))
    .map(([name, answer]) => ({
      name,
      type: answer.type,
      labels:
        answer.type === "choice"
          ? [...(answer.probabilities?.keys() ?? [])]
          : answer.type === "score"
            ? (answer.probabilities ?? []).map(() => "")
            : [],
    }));
  return [...asked, ...unnamed];
}
