import { Code, ConnectError } from "@connectrpc/connect";
import { describe, expect, it, vi } from "vitest";

import type { LogTimestamp } from "@/api/log-types";

import { createDeploymentLogOperations } from "./deployment-log-client";

const timestamp: LogTimestamp = { seconds: 42n, nanos: 7 };

describe("deployment log adapters", () => {
  it("maps normalized requests and bounded history replies", async () => {
    const read = vi.fn().mockResolvedValue({
      sources: [{ sourceName: "worker", sourceLabel: "Worker", state: 1 }],
      entries: [
        {
          sourceName: "worker",
          sourceLabel: "Worker",
          stream: 2,
          timestamp,
          content: new TextEncoder().encode("failed"),
          truncated: true,
        },
      ],
      nextBeforeTimestamp: timestamp,
      historyState: 1,
      responseTruncated: true,
      latestPositions: [{ sourceName: "worker", latestTimestamp: timestamp }],
    });
    const signal = new AbortController().signal;
    const client = createDeploymentLogOperations(read, vi.fn(), (request) => request, vi.fn());

    await expect(
      client.read({ deploymentId: "deployment", pageSize: 5, beforeTimestamp: timestamp }, signal),
    ).resolves.toEqual({
      sources: [{ sourceName: "worker", sourceLabel: "Worker", state: "available" }],
      entries: [
        {
          sourceName: "worker",
          sourceLabel: "Worker",
          category: "stderr",
          timestamp,
          text: "failed",
          truncated: true,
        },
      ],
      nextBeforeTimestamp: timestamp,
      historyState: "older-output-may-remain",
      responseTruncated: true,
      latestPositions: [{ sourceName: "worker", latestTimestamp: timestamp }],
    });
    expect(read).toHaveBeenCalledWith(
      { deploymentId: "deployment", pageSize: 5, beforeTimestamp: timestamp },
      { signal },
    );
  });

  it("forwards follow events and abort signal", async () => {
    const follow = vi.fn().mockReturnValue(
      (function* () {
        yield {
          event: {
            case: "sourceEvent",
            value: { sourceName: "worker", sourceLabel: "Worker", state: 3 },
          },
        };
        yield {
          event: {
            case: "entry",
            value: {
              sourceName: "worker",
              sourceLabel: "Worker",
              stream: 1,
              content: new TextEncoder().encode("ok"),
              truncated: false,
            },
          },
        };
      })(),
    );
    const signal = new AbortController().signal;
    const client = createDeploymentLogOperations(vi.fn(), follow, vi.fn(), (request) => request);

    await expect(
      awaitToArray(client.follow({ deploymentId: "deployment" }, signal)),
    ).resolves.toEqual([
      {
        event: {
          kind: "source",
          source: { sourceName: "worker", sourceLabel: "Worker", state: "ended" },
        },
      },
      {
        event: {
          kind: "entry",
          entry: {
            sourceName: "worker",
            sourceLabel: "Worker",
            category: "stdout",
            text: "ok",
            truncated: false,
          },
        },
      },
    ]);
    expect(follow).toHaveBeenCalledWith({ deploymentId: "deployment" }, { signal });
  });

  it("maps synchronous follow failures without exposing transport error objects", async () => {
    const error = new ConnectError("disconnected", Code.Unavailable);
    const follow = vi.fn(() => {
      throw error;
    });
    const client = createDeploymentLogOperations(vi.fn(), follow, vi.fn(), (request) => request);

    await expect(awaitToArray(client.follow({ deploymentId: "deployment" }))).rejects.toMatchObject(
      {
        name: "DeploymentLogError",
        message: "disconnected",
        code: Code.Unavailable,
      },
    );
  });

  it("maps follow request construction failures", async () => {
    const error = new ConnectError("invalid request", Code.InvalidArgument);
    const client = createDeploymentLogOperations(
      vi.fn(),
      vi.fn(),
      (request) => request,
      () => {
        throw error;
      },
    );

    await expect(awaitToArray(client.follow({ deploymentId: "deployment" }))).rejects.toMatchObject(
      {
        name: "DeploymentLogError",
        message: "invalid request",
        code: Code.InvalidArgument,
      },
    );
  });

  it("maps transport failures without exposing transport error objects", async () => {
    const error = new ConnectError("unavailable", Code.Unavailable);
    const client = createDeploymentLogOperations(
      vi.fn().mockRejectedValue(error),
      vi.fn(),
      (request) => request,
      vi.fn(),
    );

    await expect(client.read({ deploymentId: "deployment", pageSize: 1 })).rejects.toMatchObject({
      name: "DeploymentLogError",
      message: "unavailable",
      code: Code.Unavailable,
    });
  });
});

async function awaitToArray<T>(stream: AsyncIterable<T>): Promise<T[]> {
  const values: T[] = [];
  for await (const value of stream) values.push(value);
  return values;
}
