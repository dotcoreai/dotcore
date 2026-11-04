/**
 * Shared clipboard write for console copy affordances.
 */

/**
 * Writes `value` to the system clipboard. Prefers the async Clipboard API, falling back to a
 * hidden textarea and `document.execCommand("copy")` when it is unavailable, which is the case on
 * an insecure origin (plain HTTP off `localhost`).
 */
export async function writeClipboard(value: string): Promise<void> {
  try {
    const clipboard = globalThis.navigator?.clipboard;
    if (!clipboard?.writeText) {
      throw new Error("Clipboard API is not available.");
    }
    await clipboard.writeText(value);
    return;
  } catch (clipboardError) {
    try {
      copyWithDocumentCommand(value);
      return;
    } catch (fallbackError) {
      throw new Error("Clipboard access is not available.", {
        cause: { clipboardError, fallbackError },
      });
    }
  }
}

/**
 * Copies `value` through a hidden, focused, selected textarea and `document.execCommand("copy")`.
 * Throws describing which step failed: no document body to host the textarea, or
 * `execCommand("copy")` throwing or reporting failure.
 */
function copyWithDocumentCommand(value: string): void {
  if (typeof document === "undefined" || !document.body) {
    throw new Error("No document body is available for the copy fallback.");
  }

  const textarea = document.createElement("textarea");
  textarea.value = value;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.opacity = "0";
  textarea.style.pointerEvents = "none";
  document.body.appendChild(textarea);

  try {
    textarea.focus();
    textarea.select();
    if (document.execCommand?.("copy") !== true) {
      throw new Error('document.execCommand("copy") did not report success.');
    }
  } finally {
    textarea.remove();
  }
}
