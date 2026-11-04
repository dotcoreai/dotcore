import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useRef, useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { FieldError } from "@/components/ui/field-error";

function SubmitTimeValidationHarness({ onSave }: { onSave: () => void }) {
  const [name, setName] = useState("");
  const [error, setError] = useState<string>();
  const nameRef = useRef<HTMLInputElement>(null);

  const submit = () => {
    if (name.trim()) {
      onSave();
      return;
    }

    setError("Enter a route name before saving.");
    queueMicrotask(() => nameRef.current?.focus());
  };

  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        submit();
      }}
    >
      <label htmlFor="route-name">Route name</label>
      <input
        aria-describedby={error ? "route-name-error" : undefined}
        aria-invalid={error ? true : undefined}
        id="route-name"
        onChange={(event) => {
          setName(event.target.value);
          setError(undefined);
        }}
        ref={nameRef}
        value={name}
      />
      {error ? <FieldError id="route-name-error">{error}</FieldError> : null}
      <button type="submit">Save</button>
    </form>
  );
}

describe("FieldError", () => {
  it("renders the exact inline error with a stable field-owned ID", () => {
    render(<FieldError id="deployment-error">Choose a deployment before saving.</FieldError>);

    const error = screen.getByText("Choose a deployment before saving.");
    expect(error).toHaveAttribute("id", "deployment-error");
    expect(error).toHaveAttribute("role", "alert");
  });

  it("appears only after submit, focuses the invalid field, and clears on correction", async () => {
    const user = userEvent.setup();
    const onSave = vi.fn();
    render(<SubmitTimeValidationHarness onSave={onSave} />);

    const input = screen.getByRole("textbox", { name: "Route name" });
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(input).not.toHaveAttribute("aria-invalid");

    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(onSave).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toHaveTextContent("Enter a route name before saving.");
    expect(input).toHaveAttribute("aria-invalid", "true");
    expect(input).toHaveAttribute("aria-describedby", "route-name-error");
    expect(input).toHaveFocus();

    await user.type(input, "text-route");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(input).not.toHaveAttribute("aria-invalid");
  });
});
