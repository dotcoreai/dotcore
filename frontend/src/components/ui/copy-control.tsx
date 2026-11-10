import { CopyIcon } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";

import { Button } from "@/components/ui/button";
import { writeClipboard } from "@/lib/clipboard";
import { timing } from "@/lib/timing";
import { cn } from "@/lib/utils";

type CopyControlProps = {
  label: string;
  value: string;
};

type InlineCopyControlProps = CopyControlProps & {
  children: ReactNode;
  className?: string;
};

type CopyFeedback = "failure" | "success" | undefined;

/**
 * Makes an inline value and its copy affordance one accessible, clickable control.
 * Its content remains ordinary text, so browser selection and context-menu copy behavior are preserved.
 */
function InlineCopyControl({ children, className, label, value }: InlineCopyControlProps) {
  const { copy, feedback } = useCopyFeedback(value);

  return (
    <span className={cn("relative inline-flex", className)}>
      <Feedback feedback={feedback} />
      {/* Inline values stack in rows about 0.5rem apart, so the tap area stops at the gap rather
          than covering the neighbouring value. */}
      <Button
        aria-label={`Copy ${label}`}
        className="relative h-auto min-h-0 cursor-pointer gap-1 p-0 text-inherit font-inherit select-text touch-target after:max-h-[calc(100%+0.5rem)]"
        onClick={() => void copy()}
        title={`Copy ${label}`}
        type="button"
        variant="link"
      >
        {children}
        <CopyIcon aria-hidden="true" className="size-3.5" />
      </Button>
    </span>
  );
}

/**
 * Provides the separate copy action used beside a code example without turning the code itself into
 * a button, preserving code selection and context-menu copy behavior.
 */
function CodeCopyControl({ label, value }: CopyControlProps) {
  const { copy, feedback } = useCopyFeedback(value);

  return (
    <span className="relative inline-flex">
      <Feedback feedback={feedback} />
      <Button
        aria-label={`Copy ${label}`}
        className="cursor-pointer"
        onClick={() => void copy()}
        size="sm"
        title={`Copy ${label}`}
        type="button"
        variant="outline"
      >
        <CopyIcon aria-hidden="true" />
        Copy
      </Button>
    </span>
  );
}

function Feedback({ feedback }: { feedback: CopyFeedback }) {
  if (!feedback) return null;

  return (
    <span className="absolute bottom-full left-1/2 z-10 mb-2 -translate-x-1/2 whitespace-nowrap rounded-md bg-foreground px-2 py-1 text-xs text-background shadow-md">
      <span aria-live="polite" role="status">
        {feedback === "success" ? "Copied to clipboard" : "Could not copy to clipboard. Try again."}
      </span>
    </span>
  );
}

function useCopyFeedback(value: string) {
  const [feedback, setFeedback] = useState<CopyFeedback>();
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  useEffect(() => {
    return () => clearTimeout(timer.current);
  }, []);

  const copy = async () => {
    clearTimeout(timer.current);

    try {
      await writeClipboard(value);
      setFeedback("success");
    } catch {
      setFeedback("failure");
    }

    timer.current = setTimeout(() => setFeedback(undefined), timing().copyFeedbackMs);
  };

  return { copy, feedback };
}

export { CodeCopyControl, InlineCopyControl };
export type { CopyControlProps, InlineCopyControlProps };
