import { afterEach, describe, expect, it, vi } from "vitest";

import { writeClipboard } from "./clipboard";

describe("writeClipboard", () => {
  afterEach(() => {
    delete (navigator as { clipboard?: unknown }).clipboard;
    delete (document as { execCommand?: unknown }).execCommand;
  });

  it("writes through the async Clipboard API when it is available", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });

    await writeClipboard("secure-context-value");

    expect(writeText).toHaveBeenCalledWith("secure-context-value");
  });

  it("falls back to document.execCommand when the Clipboard API is unavailable", async () => {
    // Simulates the console's actual insecure-origin development environment, where
    // `navigator.clipboard` is undefined and the hidden-textarea fallback is the path that runs.
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: undefined,
    });
    const execCommand = vi.fn().mockReturnValue(true);
    Object.defineProperty(document, "execCommand", {
      configurable: true,
      value: execCommand,
    });

    await writeClipboard("insecure-origin-value");

    expect(execCommand).toHaveBeenCalledWith("copy");
  });

  it("falls back to document.execCommand when the Clipboard API rejects", async () => {
    const writeText = vi.fn().mockRejectedValue(new Error("clipboard unavailable"));
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });
    const execCommand = vi.fn().mockReturnValue(true);
    Object.defineProperty(document, "execCommand", {
      configurable: true,
      value: execCommand,
    });

    await writeClipboard("rejected-value");

    expect(writeText).toHaveBeenCalledOnce();
    expect(execCommand).toHaveBeenCalledWith("copy");
  });

  it("throws when both the Clipboard API and the fallback are unavailable", async () => {
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: undefined,
    });
    Object.defineProperty(document, "execCommand", {
      configurable: true,
      value: undefined,
    });

    const error: unknown = await writeClipboard("unreachable-value").catch(
      (caught: unknown) => caught,
    );

    expect(error).toBeInstanceOf(Error);
    expect((error as Error).message).toBe("Clipboard access is not available.");
    const cause = (error as Error).cause as { clipboardError: Error; fallbackError: Error };
    expect(cause.clipboardError.message).toBe("Clipboard API is not available.");
    expect(cause.fallbackError.message).toBe(
      'document.execCommand("copy") did not report success.',
    );
  });

  it("throws when the fallback's execCommand reports failure", async () => {
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: undefined,
    });
    Object.defineProperty(document, "execCommand", {
      configurable: true,
      value: vi.fn().mockReturnValue(false),
    });

    const error: unknown = await writeClipboard("rejected-copy-value").catch(
      (caught: unknown) => caught,
    );

    expect(error).toBeInstanceOf(Error);
    expect((error as Error).message).toBe("Clipboard access is not available.");
    const cause = (error as Error).cause as { clipboardError: Error; fallbackError: Error };
    expect(cause.clipboardError.message).toBe("Clipboard API is not available.");
    expect(cause.fallbackError.message).toBe(
      'document.execCommand("copy") did not report success.',
    );
  });
});
