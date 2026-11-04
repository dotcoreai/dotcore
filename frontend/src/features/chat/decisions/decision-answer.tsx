import type { ReactNode } from "react";

import type { DecisionAnswer } from "@/api/decision-client";
import { Skeleton } from "@/components/ui/skeleton";
import type {
  AskedQuestion,
  DecisionQuestionDraft,
} from "@/features/chat/decisions/decision-draft";
import type { DecisionRun } from "@/features/chat/decisions/use-decisions";
import { cn } from "@/lib/utils";

/**
 * The answer under one question of the form: placeholder bars while the last Ask is in flight,
 * then its answer when the Ask asked this question at this position with this type. Nothing shows
 * for a question the Ask didn't ask.
 */
export function DecisionQuestionAnswer({
  index,
  placeholders,
  question,
  run,
}: {
  index: number;
  /** Whether a slow Ask in flight shows placeholder bars yet. */
  placeholders: boolean;
  question: DecisionQuestionDraft;
  run: DecisionRun | undefined;
}) {
  if (!run) return null;
  if (run.result === undefined) return placeholders ? <DecisionAnswerPlaceholder /> : null;
  if (run.result.kind !== "answered") return null;

  const asked = run.request.questions[index];
  if (asked?.name !== question.name || asked.type !== question.type) return null;
  return <DecisionAnswerView answer={run.result.answers.get(asked.name)} question={asked} />;
}

/**
 * One question's answer: the selected option, expected level or yes or no, with its confidence
 * when given, over a probability bar per option or level in the asked order. Labels come from the
 * question as it was asked, so the answer never mixes with later edits.
 */
export function DecisionAnswerView({
  answer,
  question,
}: {
  answer: DecisionAnswer | undefined;
  question: AskedQuestion;
}) {
  if (!answer) {
    return (
      <AnswerFrame>
        <p className="text-xs text-muted-foreground">No answer</p>
      </AnswerFrame>
    );
  }

  const confidence =
    answer.confidence === undefined ? [] : [`${percent(answer.confidence)} confidence`];

  switch (answer.type) {
    case "noul":
      return (
        <AnswerFrame>
          <Verdict
            detail={[`${percent(answer.yesProbability)} probability of yes`, ...confidence]}
            value={answer.yesProbability >= 0.5 ? "Yes" : "No"}
          />
          <ProbabilityBar label="yes" probability={answer.yesProbability} top />
        </AnswerFrame>
      );

    case "score": {
      const probabilities = answer.probabilities ?? [];
      const top = probabilities.indexOf(Math.max(...probabilities));
      return (
        <AnswerFrame>
          <Verdict
            detail={[
              `level ${answer.score.toFixed(2)} of ${Math.max(question.labels.length - 1, 0)}`,
              ...confidence,
            ]}
            value={levelLabel(question, Math.round(answer.score))}
          />
          {probabilities.length > 0
            ? question.labels.map((_, level) => (
                <ProbabilityBar
                  key={level}
                  label={levelLabel(question, level)}
                  probability={probabilities[level] ?? 0}
                  top={level === top}
                />
              ))
            : null}
        </AnswerFrame>
      );
    }

    case "choice":
      return (
        <AnswerFrame>
          <Verdict detail={confidence} value={answer.choice} />
          {answer.probabilities
            ? question.labels.map((option, index) => (
                <ProbabilityBar
                  key={index}
                  label={option}
                  probability={answer.probabilities?.get(option) ?? 0}
                  top={option === answer.choice}
                />
              ))
            : null}
        </AnswerFrame>
      );
  }
}

/** Placeholder bars standing in for an answer while the request is in flight. */
export function DecisionAnswerPlaceholder() {
  return (
    <AnswerFrame>
      <Skeleton className="h-2 w-2/5 rounded-full" />
      <Skeleton className="h-2 rounded-full" />
      <Skeleton className="h-2 w-3/4 rounded-full" />
    </AnswerFrame>
  );
}

function AnswerFrame({ children }: { children: ReactNode }) {
  return (
    <div className="grid gap-1 border-t border-dashed pt-2" data-slot="decision-answer">
      {children}
    </div>
  );
}

function Verdict({ detail, value }: { detail: readonly string[]; value: string }) {
  return (
    <p className="mb-0.5 flex flex-wrap items-baseline gap-x-2 text-[12.5px]">
      <strong className="text-[13.5px] font-semibold">{value}</strong>
      {detail.length > 0 ? (
        <span className="text-muted-foreground">{detail.join(" · ")}</span>
      ) : null}
    </p>
  );
}

/** One `label | bar | percent` line; the percent is the reading and the bar only shows it. */
function ProbabilityBar({
  label,
  probability,
  top,
}: {
  label: string;
  probability: number;
  top: boolean;
}) {
  return (
    <div
      className="grid grid-cols-[minmax(3.5rem,8rem)_minmax(0,1fr)_2.75rem] items-center gap-2 text-[12.5px]"
      data-top={top || undefined}
      data-slot="decision-probability"
    >
      <span className={cn("truncate", top && "font-semibold")} title={label}>
        {label}
      </span>
      <span aria-hidden="true" className="h-2 overflow-hidden rounded-full bg-muted">
        <span
          className={cn(
            "block h-full rounded-full transition-[width] duration-300 motion-reduce:transition-none",
            top ? "bg-primary" : "bg-input",
          )}
          style={{ width: `${(probability * 100).toFixed(1)}%` }}
        />
      </span>
      <span className="text-right text-muted-foreground tabular-nums">{percent(probability)}</span>
    </div>
  );
}

function levelLabel(question: AskedQuestion, level: number): string {
  return question.labels[level] || `Level ${level}`;
}

function percent(probability: number): string {
  return `${Math.round(probability * 100)}%`;
}
