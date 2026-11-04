/**
 * Generated inference-route descriptor contract tests.
 *
 * These tests pin management methods and the lean desired/availability view
 * consumed by future console features. Route-manager behavior is excluded.
 */
import { describe, expect, it } from "vitest";

import { file_dotcore_inference_v1_routes } from "@/gen/dotcore/inference/v1/routes_pb";

describe("Inference route management contract", () => {
  it("exposes full-resource mutations and lean availability", () => {
    expect(file_dotcore_inference_v1_routes.services[0]?.methods.map(({ name }) => name)).toEqual([
      "ListRoutes",
      "GetRoute",
      "CreateRoute",
      "ReplaceRoute",
      "DeleteRoute",
    ]);
    expect(file_dotcore_inference_v1_routes.messages.map(({ name }) => name)).toEqual([
      "ListRoutesRequest",
      "ListRoutesResponse",
      "GetRouteRequest",
      "GetRouteResponse",
      "CreateRouteRequest",
      "CreateRouteResponse",
      "ReplaceRouteRequest",
      "ReplaceRouteResponse",
      "DeleteRouteRequest",
      "DeleteRouteResponse",
      "InferenceRouteView",
      "InferenceRoute",
      "InferenceRouteAvailability",
      "FallbackGroupAvailability",
      "InferenceRouteMemberAvailability",
      "FallbackGroup",
    ]);
    expect(fieldNames("CreateRouteRequest")).toEqual(["name", "enabled", "fallback_groups"]);
    expect(fieldNames("ReplaceRouteRequest")).toEqual([
      "inference_route_id",
      "expected_generation",
      "name",
      "enabled",
      "fallback_groups",
    ]);
    expect(fieldNames("DeleteRouteRequest")).toEqual(["inference_route_id", "expected_generation"]);
    expect(fieldNames("InferenceRouteView")).toEqual(["route", "availability", "request_kinds"]);
    expect(fieldNames("InferenceRouteAvailability")).toEqual([
      "available",
      "available_input_modalities",
      "exclusion_reasons",
      "fallback_groups",
    ]);
    expect(fieldNames("FallbackGroupAvailability")).toEqual(["fallback_group_index", "members"]);
    expect(fieldNames("InferenceRouteMemberAvailability")).toEqual([
      "model_deployment_id",
      "eligible",
      "available_input_modalities",
      "exclusion_reasons",
    ]);

    const contract = JSON.stringify(file_dotcore_inference_v1_routes.proto).toLowerCase();
    for (const privateName of [
      "target_generation",
      "endpoint",
      "credential",
      "provider",
      "runtime",
      "counter",
      "history",
    ]) {
      expect(contract).not.toContain(privateName);
    }
  });
});

function fieldNames(name: string) {
  const message = file_dotcore_inference_v1_routes.messages.find(
    (candidate) => candidate.name === name,
  );
  expect(message, `missing ${name} message`).toBeDefined();
  return message!.proto.field.map(({ name: fieldName }) => fieldName);
}
