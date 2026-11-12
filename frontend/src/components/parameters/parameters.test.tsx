import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { ParameterView } from "./parameter-view";
import { ParametersForm } from "./parameters";

describe("ParametersForm", () => {
  it("shows a number field's unit after its input, and nothing for a unit-less one", () => {
    const parameters: ParameterView[] = [
      {
        id: "cache",
        label: "Prompt cache memory",
        type: { kind: "unsignedInteger", maximum: 65536n, minimum: 0n, unit: "MiB" },
      },
      {
        id: "slots",
        label: "Slots",
        type: { kind: "unsignedInteger", maximum: 8n, minimum: 1n, unit: "" },
      },
    ];
    render(
      <ParametersForm
        errors={new Map()}
        idPrefix="test"
        inputRefs={{ current: new Map() }}
        inputs={
          new Map([
            ["cache", "2048"],
            ["slots", "2"],
          ])
        }
        onBlur={() => {}}
        onChange={() => {}}
        parameters={parameters}
      />,
    );

    const cache = screen.getByRole("textbox", { name: "Prompt cache memory" });
    expect(cache).toHaveValue("2048");
    expect(cache.parentElement?.textContent).toBe("MiB");
    expect(cache).not.toHaveAccessibleDescription();
    const slots = screen.getByRole("textbox", { name: "Slots" });
    expect(slots.parentElement?.textContent).toBe("Slots");
    expect(slots).not.toHaveAccessibleDescription();
  });
});
