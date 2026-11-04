import { cn } from "@/lib/utils";

const WIDTH = 100;
const HEIGHT = 20;

/**
 * A small decorative history of per-step values, drawn as bars for counts or as a line for the
 * rest. A missing value is a gap: no bar, and the line breaks there, with a reading alone between
 * gaps drawn as a dot. It is hidden from assistive technology, since the card it sits in states the
 * reading.
 */
export function Sparkline({
  className,
  values,
  variant = "line",
}: {
  className?: string;
  /** One value per step, oldest first; `null` marks a step with no data. */
  values: readonly (number | null)[];
  variant?: "bars" | "line";
}) {
  const max = Math.max(0, ...values.filter((value): value is number => value !== null));

  return (
    <svg
      aria-hidden="true"
      className={cn("block h-4.5 w-full text-chart-1", className)}
      data-slot="sparkline"
      data-variant={variant}
      focusable="false"
      preserveAspectRatio="none"
      viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
    >
      {variant === "bars" ? barShapes(values, max) : lineShapes(values, max)}
    </svg>
  );
}

function barShapes(values: readonly (number | null)[], max: number) {
  const step = WIDTH / Math.max(1, values.length);

  return values.map((value, index) => {
    // An empty step has no bar; a non-zero one keeps at least one unit of height.
    if (value === null || value <= 0 || max === 0) return null;
    const height = Math.max(1, (value / max) * HEIGHT);

    return (
      <rect
        data-slot="sparkline-bar"
        fill="currentColor"
        height={height}
        key={index}
        width={Math.max(0.5, step * 0.8)}
        x={index * step + step * 0.1}
        y={HEIGHT - height}
      />
    );
  });
}

function lineShapes(values: readonly (number | null)[], max: number) {
  const step = WIDTH / Math.max(1, values.length - 1);
  const runs: string[][] = [];
  let current: string[] = [];

  const flush = () => {
    if (current.length > 0) runs.push(current);
    current = [];
  };

  values.forEach((value, index) => {
    if (value === null) {
      flush();
      return;
    }
    const y = max === 0 ? HEIGHT - 1 : HEIGHT - 1 - (value / max) * (HEIGHT - 2);
    current.push(`${(index * step).toFixed(2)} ${y.toFixed(2)}`);
  });
  flush();

  return runs.map((points) => {
    const d = points.map((point, index) => `${index === 0 ? "M" : "L"}${point}`).join(" ");
    // A reading between gaps has no segment to stroke; a round cap on a zero-length one draws a dot.
    const single = points.length === 1;

    return (
      <path
        d={single ? `${d} l0 0` : d}
        data-slot={single ? "sparkline-point" : "sparkline-line"}
        fill="none"
        key={d}
        stroke="currentColor"
        strokeLinecap={single ? "round" : undefined}
        strokeLinejoin="round"
        strokeWidth={single ? 3 : 1.5}
        vectorEffect="non-scaling-stroke"
      />
    );
  });
}
