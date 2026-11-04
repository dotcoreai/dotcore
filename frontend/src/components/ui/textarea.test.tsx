import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { Textarea } from "@/components/ui/textarea";

describe("Textarea", () => {
  it("keeps 16px text on touch devices so iOS does not zoom on focus", () => {
    render(<Textarea aria-label="Message" />);

    const classes = screen.getByLabelText("Message").className.split(" ");

    expect(classes).toContain("pointer-coarse:text-base");
    expect(classes.indexOf("pointer-coarse:text-base")).toBeGreaterThan(
      classes.indexOf("md:text-sm"),
    );
  });

  it("takes its minimum height from the control density scale", () => {
    render(<Textarea aria-label="Message" />);

    expect(screen.getByLabelText("Message").className.split(" ")).toContain("min-h-control-md");
  });
});
