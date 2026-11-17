import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { ParameterGroupView, ParameterView } from "@/domain/parameter-view";
import type { FieldErrors } from "@/lib/form-validation";
import { setupUser } from "@/test/user";

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
        groups={[]}
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
        groups={[]}
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

  it("boxes a group's members under its title and help, and reopens it on a revealed error", async () => {
    const user = setupUser();
    const parameters: ParameterView[] = [
      { id: "slots", label: "Slots", type: { kind: "unsignedInteger", minimum: 1n, unit: "" } },
      expertField("gpu", "Expert GPU memory"),
      expertField("host", "Expert host memory"),
    ];
    const groups: ParameterGroupView[] = [
      { description: "Where expert weights live.", id: "expert", label: "Expert memory" },
    ];
    const form = (revealedErrors: FieldErrors) => (
      <ParametersForm
        errors={revealedErrors}
        groups={groups}
        idPrefix="test"
        inputRefs={{ current: new Map() }}
        inputs={new Map()}
        onBlur={() => {}}
        onChange={() => {}}
        parameters={parameters}
        revealedErrors={revealedErrors}
      />
    );
    const { rerender } = render(form(new Map()));

    const group = screen.getByRole("group", { name: "Expert memory" });
    expect(group).not.toContainElement(screen.getByRole("textbox", { name: "Slots" }));
    const toggle = within(group).getByRole("button", { name: "Expert memory" });
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(toggle).toHaveAccessibleDescription("Where expert weights live.");
    expect(
      within(group)
        .getAllByRole("textbox")
        .map((input) => input.id),
    ).toEqual(["test-gpu", "test-host"]);

    await user.click(toggle);
    expect(within(group).queryByRole("textbox")).toBeNull();

    rerender(form(new Map([["host", "Expert host memory must be an integer."]])));
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(within(group).getByRole("textbox", { name: "Expert host memory" })).toBeInvalid();
  });

  it("shows a switched group's members and a switch's dependents only while on", () => {
    const parameters: ParameterView[] = [
      { groupId: "login", id: "login", label: "Require login", type: { kind: "boolean" } },
      {
        groupId: "login",
        id: "email",
        label: "Admin email",
        type: { kind: "text", minimumLength: 1 },
      },
      { id: "reports", label: "Send reports", type: { kind: "boolean" } },
      {
        enabledById: "reports",
        id: "url",
        label: "Report URL",
        type: { kind: "text", minimumLength: 1 },
      },
    ];
    const form = (inputs: [string, string][]) => (
      <ParametersForm
        errors={new Map()}
        groups={[{ id: "login", label: "", switchId: "login" }]}
        idPrefix="test"
        inputRefs={{ current: new Map() }}
        inputs={new Map(inputs)}
        onBlur={() => {}}
        onChange={() => {}}
        parameters={parameters}
      />
    );
    const { rerender } = render(
      form([
        ["login", "false"],
        ["reports", "false"],
      ]),
    );

    const login = screen.getByRole("group", { name: "Require login" });
    expect(within(login).getByRole("switch", { name: "Require login" })).not.toBeChecked();
    expect(within(login).queryByRole("button", { name: /^Require login/ })).toBeNull();
    expect(screen.queryByRole("textbox")).toBeNull();

    rerender(
      form([
        ["login", "true"],
        ["reports", "true"],
      ]),
    );
    expect(within(login).getByRole("textbox", { name: "Admin email" })).toBeInTheDocument();
    const url = screen.getByRole("textbox", { name: "Report URL" });
    expect(login).not.toContainElement(url);
    expect(url.closest("[data-slot=field]")?.parentElement).toHaveClass("border-l-2");
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

function expertField(id: string, label: string): ParameterView {
  return {
    groupId: "expert",
    id,
    label,
    type: { kind: "unsignedInteger", minimum: 0n, unit: "MiB" },
  };
}
