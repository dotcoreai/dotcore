import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import * as clipboard from "@/lib/clipboard";

import { CopyableValue } from "./copyable-value";

const writeClipboard = vi.fn<typeof clipboard.writeClipboard>();
beforeEach(() => {
  vi.spyOn(clipboard, "writeClipboard").mockImplementation(writeClipboard);
});

// A 40-character value, the length of a Hugging Face revision SHA.
const longValue = "a1b2c3d4e5a1b2c3d4e5a1b2c3d4e5a1b2c3d4e5";

describe("CopyableValue", () => {
  beforeEach(() => {
    writeClipboard.mockReset();
  });

  it("renders a button", () => {
    render(<CopyableValue value={longValue} />);

    expect(screen.getByRole("button")).toBeInTheDocument();
  });

  it("middle-truncates a known 40-character value to the default 7-and-6 split", () => {
    render(<CopyableValue value={longValue} />);

    expect(screen.getByRole("button")).toHaveTextContent("a1b2c3d…c3d4e5");
  });

  it("keeps the ellipsis out of the accessible name, so it is never announced", () => {
    render(<CopyableValue value={longValue} />);

    expect(screen.getByRole("button")).toHaveAccessibleName(`Copy ${longValue}`);
    expect(screen.getByRole("button")).not.toHaveAccessibleName(/…/);
  });

  it("renders a value of length head + tail + 1 or shorter whole, with no ellipsis", () => {
    const shortValue = "0123456789abc"; // 13 chars: exactly 7 + 6 + 0, under the +1 threshold
    render(<CopyableValue value={shortValue} />);

    const button = screen.getByRole("button");
    expect(button).toHaveTextContent(shortValue);
    expect(button.textContent).not.toContain("…");
  });

  it("honors custom head and tail props", () => {
    render(<CopyableValue head={2} tail={2} value={longValue} />);

    expect(screen.getByRole("button")).toHaveTextContent("a1…e5");
  });

  it("renders only the head guard when tail is zero, with nothing after the ellipsis", () => {
    render(<CopyableValue head={7} tail={0} value={longValue} />);

    expect(screen.getByRole("button")).toHaveTextContent(/^a1b2c3d…$/);
  });

  it("renders only the tail guard when head is zero, with nothing before the ellipsis", () => {
    render(<CopyableValue head={0} tail={6} value={longValue} />);

    expect(screen.getByRole("button")).toHaveTextContent(/^…c3d4e5$/);
  });

  it("renders only the ellipsis when both head and tail are zero", () => {
    render(<CopyableValue head={0} tail={0} value={longValue} />);

    expect(screen.getByRole("button")).toHaveTextContent(/^…$/);
  });

  it("normalizes a negative tail to zero", () => {
    render(<CopyableValue head={7} tail={-4} value={longValue} />);

    expect(screen.getByRole("button")).toHaveTextContent(/^a1b2c3d…$/);
  });

  it("normalizes a negative head to zero", () => {
    render(<CopyableValue head={-4} tail={6} value={longValue} />);

    expect(screen.getByRole("button")).toHaveTextContent(/^…c3d4e5$/);
  });

  it("exposes the full value on hover via title", () => {
    render(<CopyableValue value={longValue} />);

    expect(screen.getByTitle(longValue)).toBeInTheDocument();
  });

  it("names the copy action alongside the full value, matching the console's other copy buttons", () => {
    render(<CopyableValue value={longValue} />);

    expect(screen.getByRole("button", { name: `Copy ${longValue}` })).toBeInTheDocument();
  });

  it("copies the full untruncated value on click and shows success feedback", async () => {
    const user = userEvent.setup();
    writeClipboard.mockResolvedValue(undefined);
    render(<CopyableValue value={longValue} />);

    await user.click(screen.getByRole("button", { name: `Copy ${longValue}` }));

    expect(writeClipboard).toHaveBeenCalledExactlyOnceWith(longValue);
    expect(screen.getByRole("status")).toHaveTextContent("Copied to clipboard");
  });

  it("shows a distinct message when the copy fails", async () => {
    const user = userEvent.setup();
    writeClipboard.mockRejectedValue(new Error("clipboard unavailable"));
    render(<CopyableValue value={longValue} />);

    await user.click(screen.getByRole("button", { name: `Copy ${longValue}` }));

    expect(screen.getByRole("status")).toHaveTextContent("Could not copy to clipboard. Try again.");
  });
});
