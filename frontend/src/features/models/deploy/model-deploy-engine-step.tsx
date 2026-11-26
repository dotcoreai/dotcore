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
 * The Engine step of the model deploy dialog, shown in advanced mode: the launch command the
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
  const argumentsHelpId = "custom-arguments-help";

  return (
    <DeploymentForm className="gap-5">
      <DeploymentStepHeading>Engine</DeploymentStepHeading>
      <section aria-labelledby="launch-command-heading" className="grid min-w-0 gap-1.5">
        <div className="flex items-center justify-between gap-3">
          <h3 className="text-sm font-medium" id="launch-command-heading">
            Command
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
        <p className="text-xs text-muted-foreground" id={argumentsHelpId}>
          One argument per line, or paste a command line. Added at the end of the command.
        </p>
        <Textarea
          aria-describedby={argumentsHelpId}
          className="min-h-32 font-mono text-xs md:text-xs"
          disabled={disabled}
          id="custom-arguments"
          onChange={(event) => onTextChange(event.target.value)}
          ref={inputRef}
          spellCheck={false}
          value={text}
        />
      </div>
    </DeploymentForm>
  );
}

/**
 * A resolved command as one line of shell words: values the runtime fills in at launch are muted,
 * and the custom arguments that end it are highlighted.
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

  return (
    <pre
      aria-label="Launch command"
      className={cn(
        "max-h-60 min-w-0 overflow-y-auto rounded-md border border-border bg-muted/40 p-3 font-mono text-xs leading-relaxed break-all whitespace-pre-wrap text-muted-foreground",
        stale && "opacity-60",
      )}
    >
      {tokens.map((token, index) => (
        <span key={index}>
          {index ? " " : null}
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
    </pre>
  );
}
