import type { RefObject } from "react";

import { Notice } from "@/components/ui/notice";
import { Textarea } from "@/components/ui/textarea";
import {
  DeploymentForm,
  DeploymentStepHeading,
} from "@/features/deployments/deployment-form-layout";
import { quoteArgument } from "@/features/models/runtime/argument-lines";
import { desiredArgv } from "@/features/models/runtime/desired-argv";
import { cn } from "@/lib/utils";

import type { PreviewResult } from "./use-model-deploy-engine";

interface ModelDeployEngineStepProps {
  /** The earlier step whose errors hold back the command, if any. */
  blockedBy?: string;
  /** The custom arguments' tokens, which end the command. */
  customArgumentTokens: readonly string[];
  disabled: boolean;
  inputRef: RefObject<HTMLTextAreaElement | null>;
  onTextChange: (text: string) => void;
  /** The latest command preview, current or not. */
  preview?: PreviewResult;
  text: string;
  /** Whether the shown command is older than the values. */
  updating: boolean;
}

/**
 * The Command step of the model deploy dialog, shown in advanced mode: the launch command the
 * server resolves for the dialog's values, then the custom arguments added at its end, one per
 * line or as a pasted command line. A rejected command shows the server's reason in its place.
 */
export function ModelDeployEngineStep({
  blockedBy,
  customArgumentTokens,
  disabled,
  inputRef,
  onTextChange,
  preview,
  text,
  updating,
}: ModelDeployEngineStepProps) {
  return (
    <DeploymentForm className="gap-5">
      <DeploymentStepHeading>Command</DeploymentStepHeading>
      <section className="grid min-w-0 gap-1.5">
        <div className="flex items-center justify-between gap-3">
          <h3 className="text-sm font-medium" id="launch-command-heading">
            Launch command
          </h3>
          {updating && !blockedBy ? (
            <span className="text-xs text-muted-foreground" role="status">
              Updating…
            </span>
          ) : null}
        </div>
        {blockedBy ? (
          <p className="text-xs text-muted-foreground" role="status">
            Fix the errors on the {blockedBy} step to see the command.
          </p>
        ) : preview?.error ? (
          <Notice role="alert" tone="destructive">
            {`The command was rejected: ${preview.error}`}
          </Notice>
        ) : preview?.snapshot ? (
          <LaunchCommand
            customArgumentTokens={customArgumentTokens}
            stale={updating}
            tokens={desiredArgv(preview.snapshot)}
          />
        ) : (
          <p className="text-xs text-muted-foreground" role="status">
            Resolving command…
          </p>
        )}
      </section>
      <div className="grid min-w-0 gap-1.5 text-sm">
        <label className="font-medium" htmlFor="custom-arguments">
          Custom arguments
        </label>
        <Textarea
          className="max-h-40 min-h-20 font-mono text-xs md:text-xs"
          disabled={disabled}
          id="custom-arguments"
          onChange={(event) => onTextChange(event.target.value)}
          placeholder={"--temp 0.7\n--top-k 40"}
          ref={inputRef}
          spellCheck={false}
          value={text}
        />
      </div>
    </DeploymentForm>
  );
}

/**
 * A resolved command as shell words, one flag per line with its values: the executable and what
 * precedes the first flag make the first line. Values the runtime fills in at launch are muted,
 * and the custom arguments that end it are highlighted. The block scrolls instead of wrapping.
 */
function LaunchCommand({
  customArgumentTokens,
  stale,
  tokens,
}: {
  customArgumentTokens: readonly string[];
  stale: boolean;
  tokens: ReturnType<typeof desiredArgv>;
}) {
  const tail = tokens.slice(tokens.length - customArgumentTokens.length);
  const customFrom =
    customArgumentTokens.length &&
    tail.every(
      ({ placeholder, text }, index) => !placeholder && text === customArgumentTokens[index],
    )
      ? tokens.length - customArgumentTokens.length
      : tokens.length;
  const lines = commandLines(tokens);

  return (
    <pre
      aria-labelledby="launch-command-heading"
      className={cn(
        // The dialog is at most 44rem tall, and the rest of the step takes about 29rem of it, so
        // the block gets 15rem and shrinks in shorter windows to keep the dialog body still.
        "max-h-[min(15rem,max(10rem,calc(100dvh-33rem)))] min-w-0 overflow-auto rounded-md outline-none focus-visible:ring-3 focus-visible:ring-ring/50 border border-border bg-muted/40 p-3 font-mono text-xs leading-relaxed text-muted-foreground",
        stale && "opacity-60",
      )}
      role="region"
      // A scrolling block must take focus so the keyboard can scroll it.
      // eslint-disable-next-line jsx-a11y/no-noninteractive-tabindex
      tabIndex={0}
    >
      {lines.map((line, lineIndex) => (
        <div className="w-max min-w-full" key={lineIndex}>
          {line.map(({ index, token }) => (
            <span key={index}>
              {index !== line[0].index ? " " : null}
              <span
                className={cn(
                  token.placeholder && "italic",
                  index >= customFrom && "font-semibold text-primary",
                  !token.placeholder && index < customFrom && "text-foreground",
                )}
              >
                {token.placeholder ? token.text : quoteArgument(token.text)}
              </span>
            </span>
          ))}
          {lineIndex < lines.length - 1 ? " \\" : null}
        </div>
      ))}
    </pre>
  );
}

// A token starting with a dash starts a line; the tokens after it, up to the next such token, are
// its values.
function commandLines(tokens: ReturnType<typeof desiredArgv>) {
  const lines: { index: number; token: ReturnType<typeof desiredArgv>[number] }[][] = [];
  tokens.forEach((token, index) => {
    // A negative number, such as -1, is a value that stays on its flag's line.
    if (!lines.length || (!token.placeholder && /^-\D/.test(token.text))) lines.push([]);
    lines[lines.length - 1].push({ index, token });
  });
  return lines;
}
