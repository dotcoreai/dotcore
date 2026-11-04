import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { Sparkline } from "./sparkline";

describe("Sparkline", () => {
  it("is hidden from assistive technology", () => {
    render(<Sparkline values={[1, 2]} />);

    expect(document.querySelector('[data-slot="sparkline"]')).toHaveAttribute(
      "aria-hidden",
      "true",
    );
  });

  it("draws one bar per non-empty step, scaled to the peak, leaving gaps empty", () => {
    render(<Sparkline values={[2, null, 0, 4]} variant="bars" />);

    const bars = document.querySelectorAll('[data-slot="sparkline-bar"]');
    expect(Array.from(bars, (bar) => bar.getAttribute("height"))).toEqual(["10", "20"]);
    expect(Array.from(bars, (bar) => bar.getAttribute("x"))).toEqual(["2.5", "77.5"]);
  });

  it("breaks the line at a gap into separate runs", () => {
    render(<Sparkline values={[1, 2, null, 2, 1]} variant="line" />);

    const paths = document.querySelectorAll('[data-slot="sparkline-line"]');
    expect(Array.from(paths, (path) => path.getAttribute("d"))).toEqual([
      "M0.00 10.00 L25.00 1.00",
      "M75.00 1.00 L100.00 10.00",
    ]);
  });

  it("draws a reading alone between gaps as a dot", () => {
    render(<Sparkline values={[null, 2, null, 1, 1]} variant="line" />);

    const points = document.querySelectorAll('[data-slot="sparkline-point"]');
    expect(Array.from(points, (point) => point.getAttribute("d"))).toEqual(["M25.00 1.00 l0 0"]);
    expect(points[0]).toHaveAttribute("stroke-linecap", "round");
    expect(
      Array.from(document.querySelectorAll('[data-slot="sparkline-line"]'), (path) =>
        path.getAttribute("d"),
      ),
    ).toEqual(["M75.00 10.00 L100.00 10.00"]);
  });

  it("draws nothing for an all-gap series", () => {
    render(<Sparkline values={[null, null]} />);

    expect(document.querySelector('[data-slot="sparkline-line"]')).toBeNull();
  });
});
