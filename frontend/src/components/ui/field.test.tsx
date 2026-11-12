import { render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { Field } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { setTiming, timing } from "@/lib/timing";
import { setupUser } from "@/test/user";

const help = "Requests generated at once; more wait in a queue.";

function ParallelRequestsField({ error }: { error?: string }) {
  return (
    <>
      <Field
        actions={<button type="button">Use largest fit</button>}
        controlId="parallel"
        error={error}
        help={help}
        label="Parallel requests"
      >
        {(control) => <Input {...control} />}
      </Field>
      <button type="button">Elsewhere</button>
    </>
  );
}

function helpButton() {
  return screen.getByRole("button", { name: "About Parallel requests" });
}

function helpPopover() {
  return screen.queryByRole("dialog", { name: "About Parallel requests" });
}

describe("Field", () => {
  it("names the control with its label and describes it with the help while the popover is closed", () => {
    render(<ParallelRequestsField />);

    const input = screen.getByRole("textbox", { name: "Parallel requests" });
    expect(helpPopover()).toBeNull();
    expect(input).toHaveAccessibleDescription(help);
    expect(input).not.toHaveAttribute("aria-invalid");
    expect(screen.getByText(help)).not.toBeVisible();
    expect(screen.getByRole("button", { name: "Use largest fit" })).toBeVisible();
  });

  it("describes an invalid control with its help, then its error", () => {
    render(<ParallelRequestsField error="Enter a whole number." />);

    const input = screen.getByRole("textbox", { name: "Parallel requests" });
    expect(input).toHaveAttribute("aria-invalid", "true");
    expect(input).toHaveAccessibleDescription(`${help} Enter a whole number.`);
    expect(screen.getByRole("alert")).toHaveTextContent("Enter a whole number.");
  });

  it("opens the help on hover and closes it when the pointer leaves", async () => {
    const user = setupUser();
    render(<ParallelRequestsField />);

    await user.hover(helpButton());
    expect(
      await screen.findByRole("dialog", { name: "About Parallel requests" }),
    ).toHaveTextContent(help);

    await user.unhover(helpButton());
    await waitFor(() => expect(helpPopover()).toBeNull());
  });

  it("opens the help on keyboard focus, keeps it on Enter and closes it on Tab away", async () => {
    const user = setupUser();
    render(<ParallelRequestsField />);

    await user.tab();
    expect(helpButton()).toHaveFocus();
    expect(
      await screen.findByRole("dialog", { name: "About Parallel requests" }),
    ).toHaveTextContent(help);

    await user.keyboard("{Enter}");
    expect(helpPopover()).not.toBeNull();
    expect(helpButton()).toHaveFocus();

    await user.tab();
    expect(screen.getByRole("textbox", { name: "Parallel requests" })).toHaveFocus();
    await waitFor(() => expect(helpPopover()).toBeNull());
  });

  it("opens the help on click and closes it on Escape", async () => {
    // A hover delay longer than the test keeps the click the only way the help opens.
    setTiming({ ...timing(), tooltipOpenMs: 60_000 });
    const user = setupUser();
    render(<ParallelRequestsField />);

    await user.click(helpButton());
    expect(
      await screen.findByRole("dialog", { name: "About Parallel requests" }),
    ).toHaveTextContent(help);

    await user.keyboard("{Escape}");
    await waitFor(() => expect(helpPopover()).toBeNull());
  });

  it("opens the help on tap and closes it on a tap outside", async () => {
    const user = setupUser();
    render(<ParallelRequestsField />);

    await user.pointer({ keys: "[TouchA]", target: helpButton() });
    expect(
      await screen.findByRole("dialog", { name: "About Parallel requests" }),
    ).toHaveTextContent(help);

    await user.pointer({
      keys: "[TouchA]",
      target: screen.getByRole("button", { name: "Elsewhere" }),
    });
    await waitFor(() => expect(helpPopover()).toBeNull());
  });
});
