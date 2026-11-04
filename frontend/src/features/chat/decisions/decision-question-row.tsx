import { ChevronRight, Plus, Trash2, XIcon } from "lucide-react";
import { type ReactNode, useId } from "react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";
import {
  type DecisionQuestionDraft,
  type DecisionQuestionType,
  decisionQuestionTypes,
  SCORE_LEVELS,
} from "@/features/chat/decisions/decision-draft";
import { cn } from "@/lib/utils";

/**
 * One question of the panel's compact list: a summary line with its name, type and instructions
 * that opens to edit the question, and its answer below.
 */
export function DecisionQuestionRow({
  answer,
  onChange,
  onOpenChange,
  onRemove,
  open,
  question,
}: {
  answer?: ReactNode;
  onChange: (question: DecisionQuestionDraft) => void;
  onOpenChange: (open: boolean) => void;
  onRemove: () => void;
  open: boolean;
  question: DecisionQuestionDraft;
}) {
  const bodyId = useId();

  return (
    <div className="rounded-lg border bg-card" data-open={open}>
      <div className="flex min-w-0 items-center gap-1 pr-1">
        <button
          aria-controls={open ? bodyId : undefined}
          aria-expanded={open}
          aria-label={`${question.name || "unnamed"} (${typeLabel(question.type)})`}
          className="flex min-h-control-md min-w-0 flex-1 items-center gap-1.5 rounded-lg px-2 text-left outline-none focus-visible:ring-3 focus-visible:ring-ring/50"
          onClick={() => onOpenChange(!open)}
          type="button"
        >
          <ChevronRight
            aria-hidden="true"
            className={cn(
              "size-4 shrink-0 text-console-subtle transition-transform motion-reduce:transition-none",
              open && "rotate-90",
            )}
          />
          <span className="shrink-0 font-mono text-[12.5px] font-semibold">
            {question.name || "unnamed"}
          </span>
          <DecisionQuestionTypeBadge type={question.type} />
          <span className="min-w-0 flex-1 truncate text-[12.5px] text-muted-foreground">
            {question.instructions}
          </span>
        </button>
        <RemoveQuestion name={question.name} onRemove={onRemove} />
      </div>
      {open ? (
        <div className="grid gap-2 px-2 pb-2" id={bodyId}>
          <DecisionQuestionFields onChange={onChange} question={question} />
        </div>
      ) : null}
      <div className="px-2 pb-2 empty:hidden">{answer}</div>
    </div>
  );
}

/**
 * The fields of one question: name, type, instructions, and its options or levels. A remove control
 * sits beside the type when `onRemove` is given.
 */
export function DecisionQuestionFields({
  onChange,
  onRemove,
  question,
}: {
  onChange: (question: DecisionQuestionDraft) => void;
  onRemove?: () => void;
  question: DecisionQuestionDraft;
}) {
  const changeType = (type: DecisionQuestionType) => {
    // A type without entries yet starts with the two its minimum needs, ready to fill in.
    const options =
      type === "choice" && question.options.length === 0
        ? [blankOption, blankOption]
        : question.options;
    const levels = type === "score" && question.levels.length === 0 ? ["", ""] : question.levels;
    onChange({ ...question, levels, options, type });
  };

  return (
    <>
      <div
        className={cn(
          "grid gap-1.5",
          onRemove ? "grid-cols-[minmax(0,1fr)_7.5rem_auto]" : "grid-cols-[minmax(0,1fr)_7.5rem]",
        )}
      >
        <Input
          aria-label="Question name"
          className="font-mono font-semibold"
          onChange={(event) => onChange({ ...question, name: event.target.value })}
          value={question.name}
        />
        <NativeSelect
          aria-label="Question type"
          className="w-full"
          onChange={(event) => changeType(event.target.value as DecisionQuestionType)}
          value={question.type}
        >
          {decisionQuestionTypes.map((type) => (
            <NativeSelectOption key={type.value} value={type.value}>
              {type.label}
            </NativeSelectOption>
          ))}
        </NativeSelect>
        {onRemove ? <RemoveQuestion name={question.name} onRemove={onRemove} /> : null}
      </div>
      <Input
        aria-label="Instructions"
        onChange={(event) => onChange({ ...question, instructions: event.target.value })}
        placeholder="Instructions"
        value={question.instructions}
      />
      {question.type === "choice" ? (
        <EntryList
          addLabel="Add option"
          onAdd={() => onChange({ ...question, options: [...question.options, blankOption] })}
        >
          {question.options.map((option, index) => (
            <div
              className="grid grid-cols-[minmax(0,0.7fr)_minmax(0,1fr)_auto] gap-1.5"
              key={index}
            >
              <Input
                aria-label="Option name"
                onChange={(event) =>
                  onChange({
                    ...question,
                    options: replaceAt(question.options, index, {
                      ...option,
                      name: event.target.value,
                    }),
                  })
                }
                placeholder="Option"
                value={option.name}
              />
              <Input
                aria-label="Option description"
                onChange={(event) =>
                  onChange({
                    ...question,
                    options: replaceAt(question.options, index, {
                      ...option,
                      description: event.target.value,
                    }),
                  })
                }
                placeholder="Description (optional)"
                value={option.description}
              />
              <RemoveEntry
                label={option.name ? `Remove ${option.name}` : "Remove option"}
                onRemove={() =>
                  onChange({ ...question, options: removeAt(question.options, index) })
                }
              />
            </div>
          ))}
        </EntryList>
      ) : null}
      {question.type === "score" ? (
        <EntryList
          addLabel="Add level (highest last)"
          disabled={question.levels.length >= SCORE_LEVELS.max}
          onAdd={() => onChange({ ...question, levels: [...question.levels, ""] })}
        >
          {question.levels.map((level, index) => (
            <div
              className="grid grid-cols-[1.5rem_minmax(0,1fr)_auto] items-center gap-1.5"
              key={index}
            >
              <span className="text-right text-[11.5px] text-console-subtle tabular-nums">
                {index}
              </span>
              <Input
                aria-label={`Level ${index}`}
                onChange={(event) =>
                  onChange({
                    ...question,
                    levels: replaceAt(question.levels, index, event.target.value),
                  })
                }
                placeholder="Level"
                value={level}
              />
              <RemoveEntry
                label={`Remove level ${index}`}
                onRemove={() => onChange({ ...question, levels: removeAt(question.levels, index) })}
              />
            </div>
          ))}
        </EntryList>
      ) : null}
    </>
  );
}

/** A question's type, as a fixed property of the question. */
export function DecisionQuestionTypeBadge({ type }: { type: DecisionQuestionType }) {
  return <Badge variant="neutral">{typeLabel(type)}</Badge>;
}

function EntryList({
  addLabel,
  children,
  disabled,
  onAdd,
}: {
  addLabel: string;
  children: ReactNode;
  disabled?: boolean;
  onAdd: () => void;
}) {
  return (
    <div className="grid gap-1.5">
      {children}
      <Button
        className="justify-self-start"
        disabled={disabled}
        onClick={onAdd}
        size="xs"
        type="button"
        variant="ghost"
      >
        <Plus data-icon="inline-start" />
        {addLabel}
      </Button>
    </div>
  );
}

function RemoveQuestion({ name, onRemove }: { name: string; onRemove: () => void }) {
  return (
    <Button
      aria-label={name ? `Remove ${name}` : "Remove question"}
      onClick={onRemove}
      size="icon"
      type="button"
      variant="ghost"
    >
      <Trash2 />
    </Button>
  );
}

function RemoveEntry({ label, onRemove }: { label: string; onRemove: () => void }) {
  return (
    <Button aria-label={label} onClick={onRemove} size="icon" type="button" variant="ghost">
      <XIcon />
    </Button>
  );
}

const blankOption = { description: "", name: "" };

function typeLabel(type: DecisionQuestionType): string {
  return decisionQuestionTypes.find((entry) => entry.value === type)?.label ?? type;
}

function replaceAt<Item>(items: readonly Item[], index: number, item: Item): Item[] {
  return items.map((current, position) => (position === index ? item : current));
}

function removeAt<Item>(items: readonly Item[], index: number): Item[] {
  return items.filter((_, position) => position !== index);
}
