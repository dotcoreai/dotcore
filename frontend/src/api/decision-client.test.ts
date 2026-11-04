import { describe, expect, it, vi } from "vitest";

import { createDecisionClient } from "@/api/decision-client";

type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;

const body = '{"state":"s","questions":{"q":{"type":"noul","instructions":"i"}}}';

/** A clock returning each given time once, in order. */
function fakeClock(...times: number[]) {
  return vi.fn(() => times.shift() ?? Number.NaN);
}

function clientFor(response: () => Response, now = fakeClock(100, 142.5)) {
  const fetchMock = vi.fn<FetchLike>(() => Promise.resolve(response()));
  return { client: createDecisionClient("http://host:1/", fetchMock, now), fetchMock };
}

function jsonResponse(status: number, statusText: string, payload: unknown) {
  return new Response(JSON.stringify(payload), {
    status,
    statusText,
    headers: { "Content-Type": "application/json" },
  });
}

function errorEnvelope(message: string, type: string, param: string | null, code: string | null) {
  return { error: { message, type, param, code } };
}

describe("decision client", () => {
  it("parses all three answer types with usage and the measured latency", async () => {
    const responseText = JSON.stringify({
      model: "clef",
      answers: {
        topic: {
          type: "choice",
          choice: "billing",
          probabilities: { billing: 0.75, "2": 0.25 },
          confidence: 0.5,
        },
        urgency: {
          type: "score",
          score: 1.25,
          legend: { "0": "low", "1": "mid", "2": "high" },
          probabilities: { "0": 0.1, "1": 0.55, "2": 0.35 },
          confidence: 0.4,
        },
        refund: { type: "noul", noul: 0.8 },
      },
      usage: { input_tokens: 321, output_tokens: 0 },
    });
    const { client, fetchMock } = clientFor(() => new Response(responseText, { status: 200 }));

    const result = await client.ask({ model: "clef", body });

    expect(fetchMock.mock.calls[0][0]).toBe("http://host:1/v1/systemone");
    expect(fetchMock.mock.calls[0][1]?.method).toBe("POST");
    expect(result).toEqual({
      kind: "answered",
      responseText,
      answers: new Map([
        [
          "topic",
          {
            type: "choice",
            choice: "billing",
            probabilities: new Map([
              ["2", 0.25],
              ["billing", 0.75],
            ]),
            confidence: 0.5,
          },
        ],
        [
          "urgency",
          { type: "score", score: 1.25, probabilities: [0.1, 0.55, 0.35], confidence: 0.4 },
        ],
        ["refund", { type: "noul", yesProbability: 0.8 }],
      ]),
      usage: { inputTokens: 321 },
      latencyMs: 42.5,
    });
  });

  it("sends the body byte for byte with the model spliced in first", async () => {
    const written =
      '  {"questions": {"2": {"type":"noul","instructions":"a"},' +
      ' "1": {"type":"noul","instructions":"b"}, "2": {"type":"noul","instructions":"c"}},\n' +
      ' "state": "s"}';
    const { client, fetchMock } = clientFor(() =>
      jsonResponse(400, "Bad Request", errorEnvelope("x", "invalid_request_error", null, null)),
    );

    await client.ask({ model: 'route "a"', body: written });

    expect(fetchMock.mock.calls[0][1]?.body).toBe(
      '{"model":"route \\"a\\"","questions": {"2": {"type":"noul","instructions":"a"},' +
        ' "1": {"type":"noul","instructions":"b"}, "2": {"type":"noul","instructions":"c"}},\n' +
        ' "state": "s"}',
    );
  });

  it("splices the model into an empty object without a trailing comma", async () => {
    const { client, fetchMock } = clientFor(() =>
      jsonResponse(400, "Bad Request", errorEnvelope("x", "invalid_request_error", null, null)),
    );

    await client.ask({ model: "r", body: "{ }" });

    expect(fetchMock.mock.calls[0][1]?.body).toBe('{"model":"r" }');
  });

  it("refuses a body that isn't a JSON object or sets the model, without sending it", async () => {
    const { client, fetchMock } = clientFor(() => new Response("{}"));

    expect(await client.ask({ model: "r", body: '{"state":' })).toEqual({
      kind: "invalid-body",
      message: "The request body is not valid JSON.",
    });
    expect(await client.ask({ model: "r", body: "[]" })).toEqual({
      kind: "invalid-body",
      message: "The request body must be a JSON object.",
    });
    expect(await client.ask({ model: "r", body: '{"model":"other","state":"s"}' })).toEqual({
      kind: "invalid-body",
      message: "The request body must not set model.",
    });
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it.each([
    {
      name: "an invalid request",
      status: 400,
      statusText: "Bad Request",
      envelope: errorEnvelope(
        "Request field criteria is invalid.",
        "invalid_request_error",
        "criteria",
        "invalid_value",
      ),
      error: {
        status: 400,
        type: "invalid_request_error",
        code: "invalid_value",
        param: "criteria",
        message: "Request field criteria is invalid.",
      },
    },
    {
      name: "a context that is too large",
      status: 400,
      statusText: "Bad Request",
      envelope: errorEnvelope(
        "The request is larger than the deployment's context.",
        "invalid_request_error",
        null,
        "context_length_exceeded",
      ),
      error: {
        status: 400,
        type: "invalid_request_error",
        code: "context_length_exceeded",
        message: "The request is larger than the deployment's context.",
      },
    },
    {
      name: "an unknown route",
      status: 404,
      statusText: "Not Found",
      envelope: errorEnvelope(
        "The requested model route does not exist.",
        "invalid_request_error",
        "model",
        "model_not_found",
      ),
      error: {
        status: 404,
        type: "invalid_request_error",
        code: "model_not_found",
        param: "model",
        message: "The requested model route does not exist.",
      },
    },
    {
      name: "a request over a size limit",
      status: 413,
      statusText: "Payload Too Large",
      envelope: errorEnvelope(
        "The request exceeds a size limit.",
        "invalid_request_error",
        null,
        "request_too_large",
      ),
      error: {
        status: 413,
        type: "invalid_request_error",
        code: "request_too_large",
        message: "The request exceeds a size limit.",
      },
    },
    {
      name: "an unavailable route",
      status: 503,
      statusText: "Service Unavailable",
      envelope: errorEnvelope(
        "The requested model route has no ready serving target.",
        "server_error",
        null,
        "model_not_ready",
      ),
      error: {
        status: 503,
        type: "server_error",
        code: "model_not_ready",
        message: "The requested model route has no ready serving target.",
      },
    },
  ])("reports the error envelope of $name", async ({ status, statusText, envelope, error }) => {
    const responseText = JSON.stringify(envelope);
    const { client } = clientFor(() => new Response(responseText, { status, statusText }));

    expect(await client.ask({ model: "r", body })).toEqual({
      kind: "failed",
      responseText,
      error,
    });
  });

  it("falls back to the status for a non-JSON error body", async () => {
    const { client } = clientFor(
      () => new Response("<html>bad gateway</html>", { status: 502, statusText: "Bad Gateway" }),
    );

    expect(await client.ask({ model: "r", body })).toEqual({
      kind: "failed",
      responseText: "<html>bad gateway</html>",
      error: { status: 502, message: "Request failed: 502 Bad Gateway" },
    });
  });

  it.each([
    { name: "a non-JSON body", responseText: "<html>ok</html>" },
    { name: "no answers object", responseText: '{"model":"r","answers":[]}' },
    {
      name: "an unknown answer type",
      responseText: '{"answers":{"q":{"type":"rank","rank":1}}}',
    },
    {
      name: "a choice without its choice",
      responseText: '{"answers":{"q":{"type":"choice","probabilities":{"a":1}}}}',
    },
    {
      name: "score levels with a gap",
      responseText:
        '{"answers":{"q":{"type":"score","score":1,"probabilities":{"0":0.5,"2":0.5}}}}',
    },
    {
      name: "a non-numeric yes probability",
      responseText: '{"answers":{"q":{"type":"noul","noul":"0.5"}}}',
    },
    {
      name: "usage without input tokens",
      responseText: '{"answers":{"q":{"type":"noul","noul":0.5}},"usage":{}}',
    },
  ])("reports a 200 with $name as unreadable, keeping the raw text", async ({ responseText }) => {
    const { client } = clientFor(() => new Response(responseText, { status: 200 }));

    expect(await client.ask({ model: "r", body })).toEqual({
      kind: "unreadable",
      responseText,
      message: "The inference API sent an unreadable response.",
    });
  });

  it("resolves with nothing when aborted in flight", async () => {
    const controller = new AbortController();
    const fetchMock = vi.fn<FetchLike>(
      (_input, init) =>
        new Promise((_resolve, reject) => {
          init?.signal?.addEventListener("abort", () =>
            reject(new DOMException("Aborted", "AbortError")),
          );
        }),
    );
    const client = createDecisionClient("http://host:1", fetchMock, fakeClock(0, 1));

    const pending = client.ask({ model: "r", body, signal: controller.signal });
    controller.abort();

    expect(await pending).toBeUndefined();
    expect(fetchMock.mock.calls[0][1]?.signal).toBe(controller.signal);
  });

  it("drops a response that arrives after the abort", async () => {
    const controller = new AbortController();
    const client = createDecisionClient(
      "http://host:1",
      () => {
        controller.abort();
        return Promise.resolve(new Response('{"answers":{}}', { status: 200 }));
      },
      fakeClock(0, 1),
    );

    expect(await client.ask({ model: "r", body, signal: controller.signal })).toBeUndefined();
  });

  it("keeps the status of a response whose body fails to arrive", async () => {
    const failingBody = new ReadableStream({
      start: (controller) => controller.error(new TypeError("network error")),
    });
    const { client } = clientFor(
      () => new Response(failingBody, { status: 200, statusText: "OK" }),
    );

    expect(await client.ask({ model: "r", body })).toEqual({
      kind: "failed",
      responseText: "",
      error: { status: 200, message: "The response was cut off before it arrived." },
    });
  });

  it("reports an unreachable API", async () => {
    const client = createDecisionClient(
      "http://host:1",
      () => Promise.reject(new TypeError("fetch failed")),
      fakeClock(0, 1),
    );

    expect(await client.ask({ model: "r", body })).toEqual({
      kind: "unreachable",
      message: "Could not reach the inference API: fetch failed",
    });
  });
});
