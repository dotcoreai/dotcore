import { SendHorizontal, Square } from "lucide-react";
import type { KeyboardEvent, Ref } from "react";
import { useState } from "react";

import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";

interface ChatComposerProps {
  /** Whether a message can be sent: a route is selected. */
  canSend: boolean;
  streaming: boolean;
  textareaRef?: Ref<HTMLTextAreaElement>;
  onSend: (text: string) => void;
  onStop: () => void;
}

/**
 * Message box with a Send control that becomes Stop while a reply streams. Enter sends, Shift+Enter
 * adds a line, and Enter does nothing during IME composition or while streaming.
 */
export function ChatComposer({
  canSend,
  onSend,
  onStop,
  streaming,
  textareaRef,
}: ChatComposerProps) {
  const [text, setText] = useState("");
  const sendable = canSend && !streaming && text.trim().length > 0;

  const submit = () => {
    if (!sendable) return;
    onSend(text);
    setText("");
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key !== "Enter" || event.shiftKey || event.nativeEvent.isComposing) return;
    event.preventDefault();
    submit();
  };

  return (
    <form
      className="flex items-end gap-2 border-t p-3 max-md:pb-[max(--spacing(3),env(safe-area-inset-bottom))]"
      onSubmit={(event) => {
        event.preventDefault();
        submit();
      }}
    >
      <Textarea
        aria-label="Message"
        className="max-h-40"
        onChange={(event) => setText(event.target.value)}
        onKeyDown={onKeyDown}
        ref={textareaRef}
        rows={1}
        value={text}
      />
      {streaming ? (
        <Button onClick={onStop} size="console" type="button" variant="outline">
          <Square data-icon="inline-start" />
          Stop
        </Button>
      ) : (
        <Button disabled={!sendable} size="console" type="submit">
          <SendHorizontal data-icon="inline-start" />
          Send
        </Button>
      )}
    </form>
  );
}
