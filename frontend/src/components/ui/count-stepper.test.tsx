import { render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { CountStepper } from "@/components/ui/count-stepper";
import { setupUser } from "@/test/user";

function Stepper({
  initial,
  onChangeSpy,
  maximum,
  minimum = 1,
  onBlur,
}: {
  initial: string;
  onChangeSpy?: (value: string, change?: { settled: boolean }) => void;
  maximum?: number;
  minimum?: number;
  onBlur?: () => void;
}) {
  const [value, setValue] = useState(initial);
  return (
    <CountStepper
      aria-label="Parallel requests"
      id="count"
      label="Parallel requests"
      maximum={maximum}
      minimum={minimum}
      onBlur={onBlur}
      onChange={(next, change) => {
        onChangeSpy?.(next, change);
        setValue(next);
      }}
      value={value}
    />
  );
}

const input = () => screen.getByRole("textbox", { name: "Parallel requests" });
const increase = () => screen.getByRole("button", { name: "Increase Parallel requests" });
const decrease = () => screen.getByRole("button", { name: "Decrease Parallel requests" });

describe("CountStepper", () => {
  it("steps by one with the buttons and stops at the bounds", async () => {
    const user = setupUser();
    render(<Stepper initial="2" maximum={3} />);

    await user.click(increase());
    expect(input()).toHaveValue("3");
    expect(increase()).toBeDisabled();

    await user.click(decrease());
    await user.click(decrease());
    expect(input()).toHaveValue("1");
    expect(decrease()).toBeDisabled();
    expect(increase()).toBeEnabled();
  });

  it("steps with the arrow keys and keeps typed values", async () => {
    const user = setupUser();
    render(<Stepper initial="4" maximum={6} />);

    await user.click(input());
    await user.keyboard("{ArrowUp}{ArrowUp}{ArrowUp}");
    expect(input()).toHaveValue("6");
    await user.keyboard("{ArrowDown}");
    expect(input()).toHaveValue("5");

    await user.clear(input());
    await user.type(input(), "12");
    expect(input()).toHaveValue("12");
  });

  it("starts from the minimum when empty, and cannot step down", async () => {
    const user = setupUser();
    render(<Stepper initial="" minimum={2} />);

    expect(decrease()).toBeDisabled();
    await user.click(increase());
    expect(input()).toHaveValue("2");
  });

  it("is unbounded above without a maximum, and reports blur", async () => {
    const onBlur = vi.fn();
    const user = setupUser();
    render(<Stepper initial="99" onBlur={onBlur} />);

    await user.click(increase());
    expect(input()).toHaveValue("100");
    expect(increase()).toBeEnabled();

    await user.click(input());
    expect(onBlur).not.toHaveBeenCalled();
    await user.tab();
    expect(onBlur).toHaveBeenCalledTimes(1);
  });

  it("lands a step inside the bounds from an out-of-range value, keeping typed values as typed", async () => {
    const user = setupUser();
    render(<Stepper initial="9" maximum={5} minimum={2} />);

    await user.click(decrease());
    expect(input()).toHaveValue("5");

    await user.clear(input());
    await user.type(input(), "0");
    expect(input()).toHaveValue("0");
    await user.click(increase());
    expect(input()).toHaveValue("2");
  });

  it("reports buttons and arrow keys as settled changes and typing as unsettled", async () => {
    const onChangeSpy = vi.fn();
    const user = setupUser();
    render(<Stepper initial="2" onChangeSpy={onChangeSpy} />);

    await user.click(increase());
    await user.click(input());
    await user.keyboard("{ArrowDown}");
    await user.type(input(), "7");
    expect(onChangeSpy.mock.calls).toEqual([
      ["3", { settled: true }],
      ["2", { settled: true }],
      ["27", undefined],
    ]);
  });
});
