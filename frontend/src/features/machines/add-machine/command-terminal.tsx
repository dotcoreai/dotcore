import { CodeCopyControl } from "@/components/ui/copy-control";
import { cn } from "@/lib/utils";

/**
 * The join command in a terminal-styled pane titled for the machine it runs on. The command shows
 * in at most three lines that fade out; Copy takes the whole command. An expired command dims and
 * loses its Copy button.
 */
export function CommandTerminal({ command, dimmed }: { command: string; dimmed?: boolean }) {
  return (
    <div className="rounded-xl border border-console-terminal-foreground/15 bg-console-terminal text-console-terminal-foreground">
      <div className="flex items-center gap-1.5 border-b border-console-terminal-foreground/15 px-3 py-2 text-[11.5px] text-console-terminal-foreground/65">
        <span aria-hidden="true" className="flex gap-1.5">
          <i className="size-2.5 rounded-full bg-console-terminal-foreground/25" />
          <i className="size-2.5 rounded-full bg-console-terminal-foreground/25" />
          <i className="size-2.5 rounded-full bg-console-terminal-foreground/25" />
        </span>
        <span className="mr-auto ml-1.5 min-w-0 truncate">on the new machine</span>
        {dimmed ? null : (
          <CodeCopyControl
            className="border-console-terminal-foreground/25 bg-transparent text-console-terminal-foreground hover:bg-console-terminal-foreground/10 hover:text-console-terminal-foreground dark:border-console-terminal-foreground/25 dark:bg-transparent dark:hover:bg-console-terminal-foreground/10"
            label="join command"
            value={command}
          />
        )}
      </div>
      <div
        className={cn(
          "relative max-h-[calc(3*1.55*12.5px+1.9rem)] overflow-hidden rounded-b-xl px-4 pt-3.5 pb-4 font-mono text-[12.5px] leading-[1.55] break-all",
          "after:absolute after:inset-x-0 after:bottom-0 after:h-7 after:bg-linear-to-b after:from-transparent after:to-console-terminal after:content-['']",
          dimmed && "opacity-40",
        )}
      >
        <span aria-hidden="true" className="mr-1.5 text-console-pattern-green">
          $
        </span>
        <code>{command}</code>
      </div>
    </div>
  );
}
