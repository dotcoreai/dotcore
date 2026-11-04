import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { Notice } from "@/components/ui/notice";

describe("Notice", () => {
  it.each([
    ["info", "border-primary/30", "bg-info", "text-primary"],
    ["destructive", "border-destructive/30", "bg-destructive/10", "text-destructive"],
  ] as const)(
    "renders the %s tone with its border, chip, and ink classes",
    (tone, border, bg, fg) => {
      render(<Notice tone={tone}>Message</Notice>);

      const notice = screen.getByText("Message");
      expect(notice).toHaveClass(border);
      expect(notice).toHaveClass(bg);
      expect(notice).toHaveClass(fg);
    },
  );

  it("shares the same geometry regardless of tone", () => {
    render(<Notice tone="destructive">Message</Notice>);

    const notice = screen.getByText("Message");
    expect(notice).toHaveClass("rounded-lg");
    expect(notice).toHaveClass("px-[15px]");
    expect(notice).toHaveClass("py-[13px]");
    expect(notice).toHaveClass("text-[12.5px]");
  });

  it("defaults to the info tone and forwards arbitrary props", () => {
    render(<Notice role="status">Message</Notice>);

    const notice = screen.getByText("Message");
    expect(notice).toHaveClass("bg-info");
    expect(notice).toHaveAttribute("role", "status");
  });
});
