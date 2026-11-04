import { create } from "@bufbuild/protobuf";
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  CreateRouteResponseSchema,
  DeleteRouteResponseSchema,
  GetRouteResponseSchema,
  ListRoutesResponseSchema,
  ReplaceRouteResponseSchema,
  RouteService,
} from "@/gen/dotcore/inference/v1/routes_pb";

import type { RouteClient } from "./route-client";
import { createRouteClient } from "./route-client";

const { createGrpcWebTransport, createClient, routeService } = vi.hoisted(() => {
  const routeService = {
    listRoutes: vi.fn(),
    getRoute: vi.fn(),
    createRoute: vi.fn(),
    replaceRoute: vi.fn(),
    deleteRoute: vi.fn(),
  };

  return {
    createGrpcWebTransport: vi.fn(() => ({}) as never),
    createClient: vi.fn(() => routeService),
    routeService,
  };
});

vi.mock("@connectrpc/connect-web", () => ({ createGrpcWebTransport }));
vi.mock("@connectrpc/connect", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@connectrpc/connect")>()),
  createClient,
}));

describe("Route client", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    vi.unstubAllEnvs();
  });

  it("creates a RouteService client against the configured management origin", () => {
    vi.stubEnv("VITE_DOTCORE_MANAGEMENT_ORIGIN", "http://100.64.0.7:6207");

    const client = createRouteClient("http://100.64.0.7:6207");

    expect(client).toBeDefined();
    expect(createGrpcWebTransport).toHaveBeenCalledWith({
      baseUrl: "http://100.64.0.7:6207",
      interceptors: [expect.any(Function)],
    });
    expect(createClient).toHaveBeenCalledWith(RouteService, expect.anything());
  });

  it("delegates every CRUD operation with its exact protobuf request and response", async () => {
    const listRequest = {};
    const getRequest = { inferenceRouteId: "route-42" };
    const createRequest = {
      name: "chat",
      enabled: true,
      fallbackGroups: [],
    };
    const replaceRequest = {
      inferenceRouteId: "route-42",
      expectedGeneration: 3n,
      name: "chat-renamed",
      enabled: false,
      fallbackGroups: [],
    };
    const deleteRequest = { inferenceRouteId: "route-42", expectedGeneration: 4n };
    const responses = {
      list: create(ListRoutesResponseSchema),
      get: create(GetRouteResponseSchema),
      create: create(CreateRouteResponseSchema),
      replace: create(ReplaceRouteResponseSchema),
      delete: create(DeleteRouteResponseSchema),
    };
    routeService.listRoutes.mockResolvedValue(responses.list);
    routeService.getRoute.mockResolvedValue(responses.get);
    routeService.createRoute.mockResolvedValue(responses.create);
    routeService.replaceRoute.mockResolvedValue(responses.replace);
    routeService.deleteRoute.mockResolvedValue(responses.delete);

    const client: RouteClient = createRouteClient("http://127.0.0.1:50051");

    await expect(client.route.listRoutes(listRequest)).resolves.toBe(responses.list);
    await expect(client.route.getRoute(getRequest)).resolves.toBe(responses.get);
    await expect(client.route.createRoute(createRequest)).resolves.toBe(responses.create);
    await expect(client.route.replaceRoute(replaceRequest)).resolves.toBe(responses.replace);
    await expect(client.route.deleteRoute(deleteRequest)).resolves.toBe(responses.delete);

    expect(routeService.listRoutes).toHaveBeenCalledWith(listRequest);
    expect(routeService.getRoute).toHaveBeenCalledWith(getRequest);
    expect(routeService.createRoute).toHaveBeenCalledWith(createRequest);
    expect(routeService.replaceRoute).toHaveBeenCalledWith(replaceRequest);
    expect(routeService.deleteRoute).toHaveBeenCalledWith(deleteRequest);
  });

  it("preserves service validation errors for the page to explain", async () => {
    const error = new Error("route name already exists");
    routeService.createRoute.mockRejectedValue(error);

    const client = createRouteClient("http://127.0.0.1:50051");

    await expect(client.route.createRoute({})).rejects.toBe(error);
  });
});
