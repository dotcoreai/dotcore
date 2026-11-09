import { beforeEach, describe, expect, it, vi } from "vitest";

const { createGrpcWebTransport, createClient, watchService } = vi.hoisted(() => {
  const watchService = { watchResources: vi.fn() };

  return {
    createGrpcWebTransport: vi.fn(() => ({}) as never),
    createClient: vi.fn(() => watchService),
    watchService,
  };
});

vi.mock("@connectrpc/connect-web", () => ({ createGrpcWebTransport }));
vi.mock("@connectrpc/connect", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@connectrpc/connect")>()),
  createClient,
}));

describe("Resource watch client", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
  });

  it("creates a ResourceWatchService client over the shared console transport", async () => {
    const { ResourceWatchService } = await import("@/gen/dotcore/watch/v1/service_pb");
    const { createResourceWatchClient } = await import("./resource-watch-client");
    const client = createResourceWatchClient("http://100.64.0.7:6207");
    const request = { subscriptions: [{ key: "machines" }] };
    watchService.watchResources.mockReturnValue("stream");

    expect(client.watchResources(request as never)).toBe("stream");

    expect(createGrpcWebTransport).toHaveBeenCalledWith({
      baseUrl: "http://100.64.0.7:6207",
      interceptors: [expect.any(Function)],
    });
    expect(createClient).toHaveBeenCalledWith(ResourceWatchService, expect.anything());
    expect(watchService.watchResources).toHaveBeenCalledWith(request);
  });
});
