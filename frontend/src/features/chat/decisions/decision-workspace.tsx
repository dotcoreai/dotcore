import { useId, useState } from "react";

import { SegmentedControl, type SegmentedControlOption } from "@/components/ui/segmented-control";
import { Textarea } from "@/components/ui/textarea";
import { useChatDecisions } from "@/features/chat/chat-session-context";
import { DecisionQuestionAnswer } from "@/features/chat/decisions/decision-answer";
import {
  AddQuestionButton,
  DecisionAskButton,
  DecisionExamplesPicker,
  DecisionNotices,
  DecisionRunSummary,
} from "@/features/chat/decisions/decision-controls";
import {
  addQuestion,
  type DecisionDraft,
  removeQuestion,
  replaceQuestion,
} from "@/features/chat/decisions/decision-draft";
import { DecisionJsonEditor } from "@/features/chat/decisions/decision-json-editor";
import { DecisionQuestionFields } from "@/features/chat/decisions/decision-question-row";
import { DecisionSetsMenu } from "@/features/chat/decisions/decision-sets-menu";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";

/**
 * The decision draft as the Chat page shows it: a toolbar with the Form / JSON switch, the
 * examples, the saved question sets and Ask, then the form with the state beside the questions,
 * or the request body as JSON beside the response. A body the form can't show stays in JSON, with
 * the reason shown once the operator asks for the form.
 */
export function DecisionWorkspace() {
  const { draft, formReason, loadForm, switchView } = useChatDecisions();
  const [formAsked, setFormAsked] = useState(false);

  const changeView = (view: DecisionDraft["view"]) => {
    setFormAsked(view === "form" && formReason !== undefined);
    switchView(view);
  };

  return (
    <div className="grid min-h-0 flex-1 auto-rows-max grid-cols-[minmax(0,1fr)] content-start gap-4 overflow-x-hidden overflow-y-auto p-4">
      <div className="flex flex-wrap items-center gap-2">
        <SegmentedControl
          aria-label="Editor"
          onValueChange={changeView}
          options={editorOptions}
          value={draft.view}
        />
        <DecisionExamplesPicker onLoad={loadForm} />
        <DecisionSetsMenu />
        <span className="flex-1" />
        <DecisionRunSummary />
        <DecisionAskButton />
      </div>
      <DecisionNotices />
      {draft.view === "form" ? (
        <DecisionFormEditor />
      ) : (
        <DecisionJsonEditor formReason={formAsked ? formReason : undefined} />
      )}
    </div>
  );
}

const editorOptions: readonly SegmentedControlOption<DecisionDraft["view"]>[] = [
  { label: "Form", value: "form" },
  { label: "JSON", value: "json" },
];

/** The state, kept in view, beside every question open to edit with its answer below. */
function DecisionFormEditor() {
  const { draft, run, updateForm } = useChatDecisions();
  const placeholders = useDelayedLoading(run !== undefined && run.result === undefined);
  const stateId = useId();
  const { form } = draft;

  return (
    <div className="grid items-start gap-4 lg:grid-cols-[minmax(0,0.9fr)_minmax(0,1.1fr)]">
      <div className="grid gap-2 rounded-lg border bg-card p-3 lg:sticky lg:top-0">
        <label className="text-sm font-semibold" htmlFor={stateId}>
          State
        </label>
        <Textarea
          className="min-h-48"
          id={stateId}
          onChange={(event) => {
            const state = event.target.value;
            updateForm((current) => ({ ...current, state }));
          }}
          value={form.state}
        />
      </div>
      <div className="grid gap-3 rounded-lg border bg-card p-3">
        <div className="flex items-center justify-between gap-2">
          <span className="text-sm font-semibold">Questions ({form.questions.length})</span>
          <AddQuestionButton onClick={() => updateForm(addQuestion)} />
        </div>
        {form.questions.map((question, index) => (
          <div
            className="grid gap-2 rounded-lg border p-2.5"
            data-slot="decision-question"
            key={index}
          >
            <DecisionQuestionFields
              onChange={(next) => updateForm((current) => replaceQuestion(current, index, next))}
              onRemove={() => updateForm((current) => removeQuestion(current, index))}
              question={question}
            />
            <DecisionQuestionAnswer
              index={index}
              placeholders={placeholders}
              question={question}
              run={run}
            />
          </div>
        ))}
      </div>
    </div>
  );
}
