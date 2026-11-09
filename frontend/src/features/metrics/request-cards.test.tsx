import type { MessageInitShape } from "@bufbuild/protobuf";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import type { MetricsClient } from "@/api/metrics-client";
import type { MetricQuerySchema } from "@/gen/dotcore/metrics/v1/metric_pb";
import { WatchPauseContext } from "@/hooks/use-watch-pause";

import {
  counterPoint,
  distributionPoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "./metric-test-support";
import { deploymentRequestCards } from "./request-card-specs";
import { RequestCards } from "./request-cards";

vi.mock("uplot", () => ({
  default: class {
    setData = vi.fn();
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

const specs = deploymentRequestCards("deployment-a");

describe("RequestCards", () => {
  it("shows six cards with their values over the range and no chart until one is selected", async () => {
    const user = userEvent.setup();
    renderCards(metricsClientWith(staticWatch(busyAnswer)));

    const section = screen.getByRole("region", { name: "Requests" });
    expect(await cardTexts(section)).toEqual([
      "Requests | 3",
      "Processed tokens | 100 | 40% cached",
      "Generated tokens | 30 | 10 per request",
      "Prompt processing | 500 tokens/s",
      "Time to first token | 250 ms",
      "Token generation | 50 tokens/s",
    ]);
    expect(within(section).queryByRole("img")).not.toBeInTheDocument();

    await user.click(within(section).getByRole("button", { name: /^Time to first token/ }));
    expect(
      await within(section).findByRole("img", { name: "Time to first token history plot" }),
    ).toBeVisible();

    await user.click(within(section).getByRole("button", { name: /^Time to first token/ }));
    expect(within(section).queryByRole("img")).not.toBeInTheDocument();
  });

  it("keeps the cards at their size while pending, with dashes for values", () => {
    renderCards(metricsClientWith(staticWatch(busyAnswer)));

    const cards = screen.getAllByRole("button");
    expect(
      cards.map((card) => card.querySelector('[data-slot="metric-card-value"]')?.textContent),
    ).toEqual(Array.from({ length: 6 }, () => "—"));
  });

  it("reads zero counts, empty lines below them and dashes for speeds when the range was idle", async () => {
    renderCards(
      metricsClientWith(
        staticWatch(() => queryResult({ series: [], stepCount: 60, stepSeconds: 15 })),
      ),
    );

    const section = screen.getByRole("region", { name: "Requests" });
    expect(await cardTexts(section)).toEqual([
      "Requests | 0",
      "Processed tokens | 0",
      "Generated tokens | 0",
      "Prompt processing | —",
      "Time to first token | —",
      "Token generation | —",
    ]);
  });

  it("says where history starts when the store started inside the range", async () => {
    renderCards(
      metricsClientWith(staticWatch((query) => busyAnswer(query, { startedAtOffset: 300 }))),
    );

    const section = screen.getByRole("region", { name: "Requests" });
    await cardTexts(section);
    const requests = within(section).getByRole("button", { name: /^Requests/ });
    expect(requests.querySelector('[data-slot="metric-card-line"]')?.textContent).toMatch(
      /^since \d{2}:\d{2}$/,
    );
  });

  it("reopens at the picked range and reads its values", async () => {
    const user = userEvent.setup();
    const watchMetrics = vi.fn(staticWatch(busyAnswer));
    renderCards(metricsClientWith(watchMetrics));
    const section = screen.getByRole("region", { name: "Requests" });
    await cardTexts(section);

    await user.click(within(section).getByRole("radio", { name: "6h" }));

    await waitFor(() => expect(watchMetrics).toHaveBeenCalledTimes(2));
    expect(watchMetrics.mock.calls[1][0].queries?.[0].query?.range).toMatchObject({
      case: "last",
      value: { seconds: 21_600n },
    });
  });

  it("keeps the last values and says so when the stream fails", async () => {
    let failing = false;
    const watchMetrics = vi.fn<MetricsClient["metrics"]["watchMetrics"]>((request, options) => {
      if (failing) return failedWatch();
      return staticWatch(busyAnswer)(request, options);
    });
    const client = metricsClientWith(watchMetrics);
    const { rerender } = render(<RequestCards client={client} headingId="h" specs={specs} />);
    const section = screen.getByRole("region", { name: "Requests" });
    await cardTexts(section);

    failing = true;
    rerender(<RequestCards client={client} headingId="h" paused specs={specs} />);
    rerender(<RequestCards client={client} headingId="h" paused={false} specs={specs} />);

    expect(await screen.findByRole("status")).toHaveTextContent(
      "Request readings stopped updating; showing the last values read.",
    );
    expect(within(section).getByRole("button", { name: /^Time to first token/ })).toHaveTextContent(
      "Time to first token250 ms",
    );
  });

  it("holds no stream while a log viewer pauses metrics, and shows nothing read", async () => {
    const watchMetrics = vi.fn(staticWatch(busyAnswer));
    renderCards(metricsClientWith(watchMetrics), true);

    await screen.findByRole("radio", { name: "6h" });
    expect(watchMetrics).not.toHaveBeenCalled();
  });
});

function renderCards(client: MetricsClient, pausedByLogs = false) {
  return render(
    <WatchPauseContext.Provider value={pausedByLogs}>
      <RequestCards client={client} headingId="requests-heading" specs={specs} />
    </WatchPauseContext.Provider>,
  );
}

/** Each card's text, once the section has read its values: the first card is never a dash. */
async function cardTexts(section: HTMLElement): Promise<string[]> {
  const cards = () => within(section).getAllByRole("button");
  await waitFor(() => expect(cards()[0]).not.toHaveTextContent("—"));

  const slot = (card: HTMLElement, name: string) =>
    card.querySelector(`[data-slot="metric-card-${name}"]`)?.textContent ?? "";

  return cards().map((card) =>
    [slot(card, "label"), slot(card, "value"), slot(card, "line")].filter(Boolean).join(" | "),
  );
}

function failedWatch(): ReturnType<MetricsClient["metrics"]["watchMetrics"]> {
  return (async function* () {
    await Promise.resolve();
    throw new Error("stream failed");
    yield* [];
  })();
}

/** A deployment that served three successful attempts in the first step of the range. */
function busyAnswer(
  query: MessageInitShape<typeof MetricQuerySchema> | undefined,
  { startedAtOffset = 0 } = {},
) {
  const counter = (delta: number) => [metricSeries({}, [counterPoint(0, 0, false, delta)])];
  const distribution = (count: number, sum: number) => [
    metricSeries({}, [distributionPoint(0, { 0.95: sum }, false, { count, sum })]),
  ];
  const series = (() => {
    switch (query?.metric) {
      case "router.attempts":
        return counter(3);
      case "router.tokens.prompt":
        return counter(100);
      case "router.tokens.cached_prompt":
        return counter(40);
      case "router.tokens.completion":
        return counter(30);
      case "router.attempt.prefill_rate":
        return distribution(2, 1000);
      case "router.attempt.decode_rate":
        return distribution(4, 200);
      case "router.attempt.ttft":
        return distribution(2, 0.5);
      default:
        return [];
    }
  })();

  return queryResult({ series, startedAtOffset, stepCount: 60, stepSeconds: 15 });
}
