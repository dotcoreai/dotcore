import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { ParameterView } from "./parameter-view";
import { ParametersDetails, ParametersForm } from "./parameters";

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

  it("shows a secret's generated-at-deploy notice as a muted italic placeholder, not a value", () => {
    const parameters: ParameterView[] = [
      { id: "pass", label: "Password", type: { kind: "secret", minimumLength: 1 } },
      { id: "user", label: "Username", type: { kind: "text", maximumLength: 8, minimumLength: 0 } },
    ];
    render(
      <ParametersForm
        errors={new Map()}
        idPrefix="test"
        inputRefs={{ current: new Map() }}
        inputs={new Map()}
        onBlur={() => {}}
        onChange={() => {}}
        parameters={parameters}
      />,
    );

    const secret = screen.getByRole("textbox", { name: "Password" });
    expect(secret).toHaveAttribute("placeholder", "Generated at deploy");
    expect(secret).toHaveValue("");
    expect(secret).toHaveClass("placeholder:italic", "placeholder:text-muted-foreground");
    expect(screen.getByRole("textbox", { name: "Username" })).not.toHaveAttribute("placeholder");
  });
});

describe("ParametersDetails", () => {
  const parameters: ParameterView[] = [
    { id: "user", label: "Username", type: { kind: "text", maximumLength: 8, minimumLength: 0 } },
    { id: "note", label: "Note", type: { kind: "text", maximumLength: 8, minimumLength: 0 } },
    { id: "pass", label: "Password", type: { kind: "secret", minimumLength: 12 } },
  ];

  it("shows values without tags, empty text as Empty and a masked secret", () => {
    render(
      <dl>
        <ParametersDetails
          parameters={parameters}
          values={
            new Map([
              ["user", "admin"],
              ["note", ""],
              ["pass", "s3cretvalue-abc"],
            ])
          }
        />
      </dl>,
    );

    const rows = screen
      .getAllByRole("term")
      .map((term) => [term.textContent, term.nextElementSibling?.textContent]);
    expect(rows).toEqual([
      ["Username", "admin"],
      ["Note", "Empty"],
      ["Password", "••••••••"],
    ]);
  });
});
