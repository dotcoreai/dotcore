import { create } from "@bufbuild/protobuf";
import { beforeEach, describe, expect, expectTypeOf, it, vi } from "vitest";

import { ListConfiguredExecutionTargetsResponseSchema } from "@/gen/dotcore/site/v1/service_pb";

import type { SiteClient } from "./site-client";

const { createClient, createGrpcWebTransport, generatedListConfiguredExecutionTargets } =
  vi.hoisted(() => {
    const generatedListConfiguredExecutionTargets = vi.fn();
    const createGrpcWebTransport = vi.fn(() => ({}) as never);
    const createClient = vi.fn((...args: unknown[]) => {
      void args;
      return {
        listConfiguredExecutionTargets: generatedListConfiguredExecutionTargets,
      } as never;
    });

    return {
      createClient,
      createGrpcWebTransport,
      generatedListConfiguredExecutionTargets,
    };
  });

vi.mock("@connectrpc/connect-web", () => ({ createGrpcWebTransport }));
vi.mock("@connectrpc/connect", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@connectrpc/connect")>()),
  createClient,
}));

describe("Site client endpoint resolution", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    vi.unstubAllEnvs();
    generatedListConfiguredExecutionTargets.mockResolvedValue(
      create(ListConfiguredExecutionTargetsResponseSchema),
    );
  });

  it("uses the loopback test fallback when no test management origin is configured", async () => {
    delete process.env.VITE_DOTCORE_MANAGEMENT_ORIGIN;

    const { createSiteClient } = await import("./site-client");
    const siteClient = createSiteClient();

    expect(siteClient).toBeDefined();
    expect(createGrpcWebTransport).toHaveBeenCalledWith({
      baseUrl: "http://127.0.0.1:50051",
      interceptors: [expect.any(Function)],
    });
    expect(createClient).toHaveBeenCalledWith(
      expect.objectContaining({ typeName: "dotcore.site.v1.SiteService" }),
      expect.anything(),
    );
  });

  it("keeps explicit client URL injection available", async () => {
    const { createSiteClient } = await import("./site-client");

    createSiteClient("http://127.0.0.1:50052");

    expect(createGrpcWebTransport).toHaveBeenLastCalledWith({
      baseUrl: "http://127.0.0.1:50052",
      interceptors: [expect.any(Function)],
    });
  });

  it("exposes only public site management methods to browser callers", async () => {
    expectTypeOf<keyof SiteClient>().toEqualTypeOf<"listConfiguredExecutionTargets">();

    const { createSiteClient } = await import("./site-client");
    const siteClient = createSiteClient();

    expect(siteClient).not.toHaveProperty("getMachine");
  });

  it("forwards configured-pool discovery without model context", async () => {
    const { createSiteClient } = await import("./site-client");
    const siteClient = createSiteClient();
    await siteClient.listConfiguredExecutionTargets({});
    expect(generatedListConfiguredExecutionTargets).toHaveBeenCalledWith({}, undefined);
  });
});
