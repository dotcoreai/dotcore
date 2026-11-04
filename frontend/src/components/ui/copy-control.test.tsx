import { act, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { CodeCopyControl, InlineCopyControl } from "@/components/ui/copy-control";

const writeClipboard = vi.fn();
vi.mock("@/lib/clipboard", () => ({
  writeClipboard: (value: string) => writeClipboard(value) as Promise<void>,
}));

afterEach(() => {
  vi.useRealTimers();
});

beforeEach(() => {
  writeClipboard.mockReset();
});

describe("copy controls", () => {
  it("copies an inline value from its full clickable surface without moving focus", async () => {
    const user = userEvent.setup();
    writeClipboard.mockResolvedValue(undefined);
    render(
      <InlineCopyControl label="Inference API origin" value="http://localhost:8080">
        <span>http://localhost:8080</span>
      </InlineCopyControl>,
    );

    const control = screen.getByRole("button", { name: "Copy Inference API origin" });
    await user.click(control);

    expect(writeClipboard).toHaveBeenCalledExactlyOnceWith("http://localhost:8080");
    expect(control).toHaveFocus();
    expect(screen.getByRole("status")).toHaveTextContent("Copied to clipboard");
    expect(screen.getByRole("status").parentElement).toHaveClass("absolute");
  });

  it("uses a distinct code-copy action and resets its exact 1.5-second feedback timer", async () => {
    vi.useFakeTimers();
    writeClipboard.mockResolvedValue(undefined);
    render(<CodeCopyControl label="curl example" value={"curl http://localhost:8080/v1/models"} />);

    const control = screen.getByRole("button", { name: "Copy curl example" });
    act(() => {
      fireEvent.click(control);
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });
    act(() => {
      fireEvent.click(control);
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });

    expect(screen.getByRole("status")).toHaveTextContent("Copied to clipboard");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(500);
    });
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
  });

  it("keeps failure feedback anchored and actionable", async () => {
    const user = userEvent.setup();
    writeClipboard.mockRejectedValue(new Error("clipboard unavailable"));
    render(<CodeCopyControl label="curl example" value="curl http://localhost:8080/v1/models" />);

    await user.click(screen.getByRole("button", { name: "Copy curl example" }));

    expect(screen.getByRole("status")).toHaveTextContent("Could not copy to clipboard. Try again.");
    expect(screen.getByRole("status").parentElement).toHaveClass("absolute");
  });
});
