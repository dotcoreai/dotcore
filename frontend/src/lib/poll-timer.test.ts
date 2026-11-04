import { afterEach, describe, expect, it, vi } from "vitest";

import { createPollTimer } from "@/lib/poll-timer";

afterEach(() => {
  vi.useRealTimers();
  setDocumentVisibility("visible");
});

function setDocumentVisibility(visibilityState: DocumentVisibilityState) {
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: visibilityState,
  });
}

function fireEvent(visibilityState: DocumentVisibilityState) {
  setDocumentVisibility(visibilityState);
  document.dispatchEvent(new Event("visibilitychange"));
}

describe("createPollTimer", () => {
  it("runs the fire callback once after the armed delay", () => {
    vi.useFakeTimers();
    const fire = vi.fn();
    const timer = createPollTimer(fire);

    timer.arm(100);
    expect(fire).not.toHaveBeenCalled();
    vi.advanceTimersByTime(99);
    expect(fire).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(fire).toHaveBeenCalledTimes(1);
    timer.dispose();
  });

  it("replaces the pending timer when re-armed", () => {
    vi.useFakeTimers();
    const fire = vi.fn();
    const timer = createPollTimer(fire);

    timer.arm(100);
    timer.arm(40);
    vi.advanceTimersByTime(100);
    expect(fire).toHaveBeenCalledTimes(1);
    timer.dispose();
  });

  it("does not fire a disarmed timer", () => {
    vi.useFakeTimers();
    const fire = vi.fn();
    const timer = createPollTimer(fire);

    timer.arm(100);
    timer.disarm();
    vi.advanceTimersByTime(200);
    expect(fire).not.toHaveBeenCalled();
    timer.dispose();
  });

  it("refuses to arm while the document is hidden and fires after the document returns", () => {
    vi.useFakeTimers();
    setDocumentVisibility("hidden");
    const fire = vi.fn();
    const timer = createPollTimer(fire);

    timer.arm(100);
    vi.advanceTimersByTime(100);
    expect(fire).not.toHaveBeenCalled();

    fireEvent("visible");
    timer.arm(100);
    vi.advanceTimersByTime(100);
    expect(fire).toHaveBeenCalledTimes(1);
    timer.dispose();
  });

  it("clears the pending timer when the document hides and reports both transitions", () => {
    vi.useFakeTimers();
    const fire = vi.fn();
    const onHidden = vi.fn();
    const onVisible = vi.fn();
    const timer = createPollTimer(fire, { onHidden, onVisible });

    timer.arm(100);
    fireEvent("hidden");
    expect(onHidden).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(100);
    expect(fire).not.toHaveBeenCalled();

    fireEvent("visible");
    expect(onVisible).toHaveBeenCalledTimes(1);
    timer.arm(50);
    vi.advanceTimersByTime(50);
    expect(fire).toHaveBeenCalledTimes(1);
    timer.dispose();
  });

  it("keeps the pending timer running while hidden when pauseWhileHidden is off", () => {
    vi.useFakeTimers();
    const fire = vi.fn();
    const onVisible = vi.fn();
    const timer = createPollTimer(fire, { pauseWhileHidden: false, onVisible });

    timer.arm(100);
    setDocumentVisibility("hidden");
    vi.advanceTimersByTime(100);
    expect(fire).toHaveBeenCalledTimes(1);
    fireEvent("visible");
    expect(onVisible).toHaveBeenCalledTimes(1);
    timer.dispose();
  });

  it("stops all activity after dispose, including visibility transitions", () => {
    vi.useFakeTimers();
    const fire = vi.fn();
    const onVisible = vi.fn();
    const timer = createPollTimer(fire, { onVisible });

    timer.arm(100);
    timer.dispose();
    vi.advanceTimersByTime(100);
    expect(fire).not.toHaveBeenCalled();

    fireEvent("hidden");
    fireEvent("visible");
    expect(onVisible).not.toHaveBeenCalled();
    timer.arm(50);
    vi.advanceTimersByTime(50);
    expect(fire).not.toHaveBeenCalled();
  });
});
