import { fireEvent, render, screen, within } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { setupUser } from "@/test/user";

import { SizeInput } from "./size-input";

describe("SizeInput with a byte unit", () => {
  it("shows the size in the largest exact unit and hands its owner MiB digits", async () => {
    const user = setupUser();
    const { onChange } = renderMemoryInput("12288");
    const box = screen.getByRole("textbox", { name: "Expert memory" });
    expect(box).toHaveValue("12 GiB");

    await user.clear(box);
    await user.type(box, "6g");
    expect(onChange).toHaveBeenLastCalledWith("6144", false);
    expect(box).toHaveValue("6g");
    await user.tab();
    expect(box).toHaveValue("6 GiB");

    // A size that isn't whole in MiB reaches the owner as typed, for it to flag.
    await user.clear(box);
    await user.type(box, "0.1g");
    expect(onChange).toHaveBeenLastCalledWith("0.1g", false);
    await user.tab();
    expect(box).toHaveValue("0.1g");
  });

  it("drags linearly in quarter GiB steps, offers powers of two GiB, then the largest fit", () => {
    const { onChange } = renderMemoryInput("12288");
    const slider = screen.getByRole("slider", { name: "Expert memory" });
    // 12288 MiB sits 12288/12595 along the linear scale from 0.
    expect(slider).toHaveValue("976");
    expect(slider).toHaveAttribute("aria-valuetext", "12 GiB");
    expect(chipLabels()).toEqual(["1 GiB", "2 GiB", "4 GiB", "8 GiB", "12.3 GiB"]);

    fireEvent.change(slider, { target: { value: "500" } });
    expect(onChange).toHaveBeenLastCalledWith("6400", true);
    expect(screen.getByRole("textbox", { name: "Expert memory" })).toHaveValue("6.25 GiB");
    fireEvent.change(slider, { target: { value: "1000" } });
    expect(screen.getByRole("textbox", { name: "Expert memory" })).toHaveValue("12595 MiB");
  });

  it("shows the sized value in an empty box as its placeholder, with the thumb there", () => {
    renderMemoryInput("", 8192);
    const box = screen.getByRole("textbox", { name: "Expert memory" });
    expect(box).toHaveValue("");
    expect(box).toHaveAttribute("placeholder", "8 GiB");
    expect(screen.getByRole("slider", { name: "Expert memory" })).toHaveValue("650");
  });

  it("presses a chip only after a click, until typing", async () => {
    const user = setupUser();
    renderMemoryInput("4096");
    expect(chip("4 GiB")).toHaveAttribute("aria-pressed", "false");

    await user.click(chip("4 GiB"));
    expect(chip("4 GiB")).toHaveAttribute("aria-pressed", "true");

    const box = screen.getByRole("textbox", { name: "Expert memory" });
    await user.clear(box);
    await user.type(box, "4g");
    expect(box).toHaveValue("4g");
    expect(chip("4 GiB")).toHaveAttribute("aria-pressed", "false");
  });

  it("shows only the box without a range", () => {
    render(
      <SizeInput
        id="memory"
        label="Expert memory"
        onBlur={vi.fn()}
        onChange={vi.fn()}
        unit="GiB"
        value="12"
      />,
    );
    expect(screen.getByRole("textbox")).toHaveValue("12 GiB");
    expect(screen.queryByRole("slider")).not.toBeInTheDocument();
    expect(screen.queryByRole("group")).not.toBeInTheDocument();
  });
});

// A MiB input whose owner keeps the value, over 0 MiB to a largest fit of 12595 MiB.
function renderMemoryInput(initial: string, emptyValue?: number) {
  const onChange = vi.fn();
  function Owner() {
    const [value, setValue] = useState(initial);
    return (
      <>
        <label htmlFor="memory">Expert memory</label>
        <SizeInput
          emptyValue={emptyValue}
          id="memory"
          label="Expert memory"
          largestFit={{ value: 12595 }}
          onBlur={vi.fn()}
          onChange={(input, settled) => {
            onChange(input, settled);
            setValue(input);
          }}
          range={{ maximum: 12595, minimum: 0 }}
          unit="MiB"
          value={value}
        />
      </>
    );
  }
  render(<Owner />);
  return { onChange };
}

function chip(name: string) {
  return within(screen.getByRole("group", { name: "Expert memory presets" })).getByRole("button", {
    name,
  });
}

function chipLabels() {
  return within(screen.getByRole("group", { name: "Expert memory presets" }))
    .getAllByRole("button")
    .map((button) => button.textContent);
}
