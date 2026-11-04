import { describe, expect, it } from "vitest";

import {
  catalogPlainViewCodec,
  catalogViewCodec,
  type CatalogViewState,
} from "./catalog-view-state";

const { defaultView } = catalogViewCodec;

const fullView: CatalogViewState = {
  columns: ["model", "memory", "engine", "quantization"],
  predicates: [
    { field: "engine", values: ["vllm", "llama-cpp"] },
    { field: "quantization", values: ["NVFP4"] },
    { field: "capability", values: ["image"] },
  ],
  search: "qwen 27b",
  sort: { column: "memory", direction: "desc" },
};

// The generic codec behavior lives in `lib/data-table-view.test.ts`; these tests pin the Catalog's
// URL keys and values.
describe("catalog view state", () => {
  it("shows Model, Intelligence, Memory, Maximum context, and Total context by intelligence with Runs on · This site, writing nothing", () => {
    expect(defaultView).toEqual({
      columns: ["model", "intelligence", "memory", "maximumContext", "totalContext"],
      predicates: [{ field: "runsOn", values: ["site"] }],
      search: "",
      sort: { column: "intelligence", direction: "desc" },
    });
    expect(catalogViewCodec.serialize(defaultView)).toBe("");
    expect(catalogViewCodec.parse("")).toEqual(defaultView);
  });

  it("records a removed Runs on chip as an empty f, and keeps a machine value", () => {
    const removed = { ...defaultView, predicates: [] };
    const machine = {
      ...defaultView,
      predicates: [{ field: "runsOn" as const, values: ["node:a"] }],
    };

    expect(catalogViewCodec.serialize(removed)).toBe("?f=");
    expect(catalogViewCodec.parse("?f=")).toEqual(removed);
    expect(catalogViewCodec.serialize(machine)).toBe("?f=runs&runs=node%3Aa");
    expect(catalogViewCodec.parse("?f=runs&runs=node%3Aa")).toEqual(machine);
  });

  it("starts with no chip while Runs on is unknown, still reading its URL values", () => {
    expect(catalogPlainViewCodec.defaultView.predicates).toEqual([]);
    expect(catalogPlainViewCodec.defaultView.sort).toEqual(defaultView.sort);
    expect(catalogPlainViewCodec.serialize(catalogPlainViewCodec.defaultView)).toBe("");
    expect(catalogPlainViewCodec.parse("?f=runs&runs=node%3Aa").predicates).toEqual([
      { field: "runsOn", values: ["node:a"] },
    ]);
  });

  it("round-trips every field with the catalog's URL keys", () => {
    const search = catalogViewCodec.serialize(fullView);

    expect(search).toBe(
      "?q=qwen+27b&f=engine%2Cquant%2Ccap&engine=vllm&engine=llama-cpp&quant=NVFP4&cap=image&sort=-memory&cols=model%2Cmemory%2Cengine%2Cquant",
    );
    expect(catalogViewCodec.parse(search)).toEqual(fullView);
  });

  it.each([
    ["ascending memory", { column: "memory", direction: "asc" }, "?sort=memory"],
    [
      "descending maximum context",
      { column: "maximumContext", direction: "desc" },
      "?sort=-max-context",
    ],
    [
      "ascending total context",
      { column: "totalContext", direction: "asc" },
      "?sort=total-context",
    ],
    ["descending name", { column: "model", direction: "desc" }, "?sort=-name"],
    ["ascending name", { column: "model", direction: "asc" }, "?sort=name"],
    ["ascending intelligence", { column: "intelligence", direction: "asc" }, "?sort=intelligence"],
  ] as const)("round-trips the %s sort", (_, sort, search) => {
    const view: CatalogViewState = { ...defaultView, sort };

    expect(catalogViewCodec.serialize(view)).toBe(search);
    expect(catalogViewCodec.parse(search)).toEqual(view);
  });

  it("stores the visible columns in cols, keeping the Model column", () => {
    const hidden = catalogViewCodec.setColumns(defaultView, ["memory"]);

    expect(hidden.columns).toEqual(["model", "memory"]);
    expect(catalogViewCodec.serialize(hidden)).toBe("?cols=model%2Cmemory");
    expect(catalogViewCodec.parse("?cols=quant%2Cengine").columns).toEqual([
      "model",
      "engine",
      "quantization",
    ]);
  });

  it("writes cols for a view that hides Intelligence", () => {
    const hidden = catalogViewCodec.setColumns(defaultView, ["memory", "totalContext"]);

    expect(hidden.columns).toEqual(["model", "memory", "totalContext"]);
    expect(catalogViewCodec.serialize(hidden)).toBe("?cols=model%2Cmemory%2Ctotal-context");
    expect(catalogViewCodec.parse("?cols=model%2Cmemory%2Ctotal-context")).toEqual(hidden);
  });

  it("drops a Fits chip, keeping its Runs on machine", () => {
    expect(catalogViewCodec.parse("?f=runs%2Cfits&runs=node%3Aa&fits=location").predicates).toEqual(
      [{ field: "runsOn", values: ["node:a"] }],
    );
  });

  it("keeps only known capability values and any non-empty engine or quantization value", () => {
    expect(
      catalogViewCodec.parse(
        "?engine=ninfer&quant=Q0&quant=&cap=image&cap=telepathy&cap=context-128k",
      ).predicates,
    ).toEqual([
      { field: "engine", values: ["ninfer"] },
      { field: "quantization", values: ["Q0"] },
      { field: "capability", values: ["image"] },
    ]);
  });

  it("writes the view before the panel parameters", () => {
    expect(
      catalogViewCodec.apply("?foo=1&sort=-name&panel=model-catalog%2Fcatalog%3Aalpha", fullView),
    ).toBe(
      "?foo=1&q=qwen+27b&f=engine%2Cquant%2Ccap&engine=vllm&engine=llama-cpp&quant=NVFP4&cap=image&sort=-memory&cols=model%2Cmemory%2Cengine%2Cquant&panel=model-catalog%2Fcatalog%3Aalpha",
    );
  });
});
