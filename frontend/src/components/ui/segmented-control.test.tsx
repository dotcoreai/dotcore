import { render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it } from "vitest";

import { setupUser } from "@/test/user";

import { SegmentedControl } from "./segmented-control";

describe("SegmentedControl", () => {
  it("is a radio group whose arrow keys move the selection", async () => {
    const user = setupUser();
    render(<Picker />);

    const group = screen.getByRole("radiogroup", { name: "History range" });
    expect(group).toBeVisible();
    expect(screen.getByRole("radio", { name: "15m" })).toBeChecked();

    await user.click(screen.getByRole("radio", { name: "6h" }));
    expect(screen.getByRole("radio", { name: "6h" })).toBeChecked();
    expect(screen.getByText("Selected 6h")).toBeVisible();

    await user.keyboard("{ArrowRight}");
    expect(screen.getByRole("radio", { name: "48h" })).toBeChecked();
    expect(screen.getByText("Selected 48h")).toBeVisible();
  });
});

function Picker() {
  const [value, setValue] = useState<"15m" | "6h" | "48h">("15m");

  return (
    <>
      <SegmentedControl
        aria-label="History range"
        onValueChange={setValue}
        options={[
          { label: "15m", value: "15m" },
          { label: "6h", value: "6h" },
          { label: "48h", value: "48h" },
        ]}
        value={value}
      />
      <p>Selected {value}</p>
    </>
  );
}
