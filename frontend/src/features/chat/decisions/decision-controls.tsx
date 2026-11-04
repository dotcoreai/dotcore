import { LoaderCircle, Maximize2, Plus, SendHorizontal } from "lucide-react";
import { useId, useState } from "react";

import type { DecisionResult } from "@/api/decision-client";
import { Button } from "@/components/ui/button";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";
import { Notice } from "@/components/ui/notice";
import { Textarea } from "@/components/ui/textarea";
import { useChatDecisions, useChatRouteSession } from "@/features/chat/chat-session-context";
import { DecisionQuestionAnswer } from "@/features/chat/decisions/decision-answer";
import {
  addQuestion,
  type DecisionForm,
  removeQuestion,
  replaceQuestion,
} from "@/features/chat/decisions/decision-draft";
import { decisionExamples } from "@/features/chat/decisions/decision-examples";
import { DecisionQuestionRow } from "@/features/chat/decisions/decision-question-row";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";

/**
 * The decision draft as the chat panel shows it: the state, the examples, and a compact question
 * list whose rows open to edit, each with its answer from the last Ask. A JSON body the form can't
 * show can't be shown as rows either, so it is noted instead, with `onOpenPage` leading to the
 * Chat page.
 */
export function DecisionEditor({ onOpenPage }: { onOpenPage?: () => void }) {
  const { draft, formReason, loadForm, run, updateForm } = useChatDecisions();
  const [openRows, setOpenRows] = useState<ReadonlySet<number>>(new Set());
  const stateId = useId();
  const placeholders = useDelayedLoading(run !== undefined && run.result === undefined);
  const { form } = draft;

  if (formReason !== undefined) {
    return (
      <div className="flex min-h-0 flex-1 flex-col p-3">
        <Notice className="flex items-center justify-between gap-2" role="status">
          <span>Edited as JSON</span>
          {onOpenPage ? (
            <Button onClick={onOpenPage} size="xs" type="button" variant="outline">
              <Maximize2 data-icon="inline-start" />
              Open the Chat page
            </Button>
          ) : null}
        </Notice>
      </div>
    );
  }

  const setRowOpen = (index: number, open: boolean) =>
    setOpenRows((rows) => {
      const next = new Set(rows);
      if (open) next.add(index);
      else next.delete(index);
      return next;
    });
  const add = () => {
    updateForm(addQuestion);
    setRowOpen(form.questions.length, true);
  };
  const remove = (index: number) => {
    updateForm((current) => removeQuestion(current, index));
    // Rows below the removed one move up, and so does their open state.
    setOpenRows(
      (rows) =>
        new Set(
          [...rows].filter((row) => row !== index).map((row) => (row > index ? row - 1 : row)),
        ),
    );
  };

  return (
    <div className="grid min-h-0 flex-1 auto-rows-max grid-cols-[minmax(0,1fr)] content-start gap-2.5 overflow-x-hidden overflow-y-auto p-3">
      <div className="flex items-center justify-between gap-2">
        <label className="text-xs font-semibold text-muted-foreground" htmlFor={stateId}>
          State
        </label>
        <DecisionExamplesPicker
          onLoad={(example) => {
            loadForm(example);
            setOpenRows(new Set());
          }}
          size="sm"
        />
      </div>
      <Textarea
        className="max-h-40"
        id={stateId}
        onChange={(event) => {
          const state = event.target.value;
          updateForm((current) => ({ ...current, state }));
        }}
        rows={3}
        value={form.state}
      />
      <div className="flex items-center justify-between gap-2">
        <span className="text-xs font-semibold text-muted-foreground">
          Questions ({form.questions.length})
        </span>
        <AddQuestionButton onClick={add} />
      </div>
      {form.questions.map((question, index) => (
        <DecisionQuestionRow
          answer={
            <DecisionQuestionAnswer
              index={index}
              placeholders={placeholders}
              question={question}
              run={run}
            />
          }
          key={index}
          onChange={(next) => updateForm((current) => replaceQuestion(current, index, next))}
          onOpenChange={(open) => setRowOpen(index, open)}
          onRemove={() => remove(index)}
          open={openRows.has(index)}
          question={question}
        />
      ))}
    </div>
  );
}

/**
 * The panel's decision footer: why the draft can't be asked, the last Ask's failure with Retry,
 * its latency and input tokens, and the Ask control.
 */
export function DecisionFooter() {
  return (
    <div className="grid gap-2 border-t p-3 max-md:pb-[max(--spacing(3),env(safe-area-inset-bottom))]">
      <DecisionNotices />
      <div className="flex items-center justify-between gap-2">
        <DecisionRunSummary />
        <DecisionAskButton />
      </div>
    </div>
  );
}

/** Why the draft can't be asked, and the last Ask's failure with Retry; nothing otherwise. */
export function DecisionNotices() {
  const { problem, retry, run } = useChatDecisions();
  const failure = describeFailure(run?.result);

  return (
    <>
      {problem ? (
        <Notice role="status" tone="warning">
          {problem}
        </Notice>
      ) : null}
      {failure ? (
        <Notice className="flex items-center justify-between gap-2" role="alert" tone="destructive">
          <span className="min-w-0 break-words">
            {failure.heading ? (
              <strong className="font-semibold">{failure.heading} · </strong>
            ) : null}
            {failure.message}
          </span>
          <Button onClick={retry} size="xs" type="button" variant="outline">
            Retry
          </Button>
        </Notice>
      ) : null}
    </>
  );
}

/** The last answered Ask's latency and input tokens; an empty slot otherwise. */
export function DecisionRunSummary() {
  const { run } = useChatDecisions();
  const answered = run?.result?.kind === "answered" ? run.result : undefined;
  if (!answered) return <span />;

  return (
    <p className="flex gap-3 text-xs text-muted-foreground">
      <span>
        <strong className="font-semibold text-foreground tabular-nums">
          {Math.round(answered.latencyMs)} ms
        </strong>
      </span>
      {answered.usage ? (
        <span>
          <strong className="font-semibold text-foreground tabular-nums">
            {answered.usage.inputTokens}
          </strong>{" "}
          input tokens
        </span>
      ) : null}
    </p>
  );
}

/** Sends the draft to the route; disabled without a route, while asking or with a problem. */
export function DecisionAskButton() {
  const { ask, problem, run } = useChatDecisions();
  const { route } = useChatRouteSession();
  const asking = run !== undefined && run.result === undefined;

  return (
    <Button
      disabled={!route || asking || problem !== undefined}
      onClick={ask}
      size="console"
      type="button"
    >
      {asking ? (
        <LoaderCircle
          className="animate-spin motion-reduce:animate-none"
          data-icon="inline-start"
        />
      ) : (
        <SendHorizontal data-icon="inline-start" />
      )}
      {asking ? "Asking…" : "Ask"}
    </Button>
  );
}

/** The built-in examples as a picker; picking one hands its form to `onLoad`. */
export function DecisionExamplesPicker({
  onLoad,
  size,
}: {
  onLoad: (form: DecisionForm) => void;
  size?: "sm";
}) {
  return (
    <NativeSelect
      aria-label="Load example"
      onChange={(event) => {
        const example = decisionExamples.find((entry) => entry.name === event.target.value);
        if (example) onLoad(example.form);
      }}
      size={size}
      value=""
    >
      <NativeSelectOption disabled value="">
        Examples
      </NativeSelectOption>
      {decisionExamples.map((example) => (
        <NativeSelectOption key={example.name} value={example.name}>
          {example.name}
        </NativeSelectOption>
      ))}
    </NativeSelect>
  );
}

/** The control adding a question at the end of the form. */
export function AddQuestionButton({ onClick }: { onClick: () => void }) {
  return (
    <Button onClick={onClick} size="xs" type="button" variant="ghost">
      <Plus data-icon="inline-start" />
      Add question
    </Button>
  );
}

/** What a failed Ask shows: its status and code when the inference API answered, and why. */
function describeFailure(
  result: DecisionResult | undefined,
): { heading?: string; message: string } | undefined {
  switch (result?.kind) {
    case "failed": {
      const { code, message, status, type } = result.error;
      return { heading: `${status} ${code ?? type ?? ""}`.trim(), message };
    }
    case "unreadable":
    case "unreachable":
    case "invalid-body":
      return { message: result.message };
    default:
      return undefined;
  }
}
