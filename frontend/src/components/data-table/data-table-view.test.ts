import { describe, expect, it } from "vitest";

import {
  albumCodec,
  type AlbumColumn,
  type AlbumField,
  albumTable,
} from "@/test/data-table-test-support";

import {
  createDataTableViewCodec,
  type DataTableView,
  type DataTableViewDefinition,
} from "./data-table-view";

type AlbumView = DataTableView<AlbumColumn, AlbumField>;

const [titleColumn, minutesColumn] = albumTable.columns;
const moodField = albumTable.fields[1];

const fullView: AlbumView = {
  columns: ["title", "minutes", "genre"],
  predicates: [
    { field: "mood", values: ["loud", "calm"] },
    { field: "genre", values: ["rock", "jazz"] },
  ],
  search: "blue train",
  sort: { column: "minutes", direction: "desc" },
};

describe("data table view codec", () => {
  it("serializes the default view to an empty string and parses an empty search to it", () => {
    expect(albumCodec.defaultView).toEqual({
      columns: ["title", "minutes"],
      predicates: [],
      search: "",
      sort: { column: "title", direction: "asc" },
    });
    expect(albumCodec.serialize(albumCodec.defaultView)).toBe("");
    expect(albumCodec.isDefault(albumCodec.defaultView)).toBe(true);
    expect(albumCodec.parse("")).toEqual(albumCodec.defaultView);
  });

  it("round-trips every part of the view", () => {
    const search = albumCodec.serialize(fullView);

    expect(search).toBe(
      "?q=blue+train&f=mood%2Cgenre&mood=loud&mood=calm&genre=rock&genre=jazz&sort=-min&cols=title%2Cmin%2Cgenre",
    );
    expect(albumCodec.parse(search)).toEqual(fullView);
    expect(albumCodec.isDefault(fullView)).toBe(false);
  });

  it.each([
    ["descending name", { column: "title", direction: "desc" }, "?sort=-name"],
    ["ascending value", { column: "minutes", direction: "asc" }, "?sort=min"],
  ] as const)("round-trips the %s sort", (_, sort, search) => {
    const view: AlbumView = { ...albumCodec.defaultView, sort };

    expect(albumCodec.serialize(view)).toBe(search);
    expect(albumCodec.parse(search)).toEqual(view);
  });

  it("keeps a shown-but-empty predicate, in display order", () => {
    const view: AlbumView = {
      ...albumCodec.defaultView,
      predicates: [
        { field: "mood", values: [] },
        { field: "genre", values: ["rock"] },
      ],
    };

    expect(albumCodec.serialize(view)).toBe("?f=mood%2Cgenre&genre=rock");
    expect(albumCodec.parse("?f=mood%2Cgenre&genre=rock")).toEqual(view);
  });

  it("treats a value key without its field in f as a shown predicate", () => {
    expect(albumCodec.parse("?genre=polka").predicates).toEqual([
      { field: "genre", values: ["polka"] },
    ]);
  });

  it("ignores unknown fields, closed-domain values, columns, and sorts", () => {
    const view = albumCodec.parse(
      "?f=genre%2Cbogus&genre=rock&genre=rock&genre=&mood=calm&mood=angry&cols=title%2Cbogus&sort=-bogus&unrelated=1",
    );

    expect(view).toEqual({
      columns: ["title"],
      predicates: [
        { field: "genre", values: ["rock"] },
        { field: "mood", values: ["calm"] },
      ],
      search: "",
      sort: { column: "title", direction: "asc" },
    });
  });

  it("forces the pinned column visible and omits cols for the default set", () => {
    const genre = albumCodec.setColumns(albumCodec.defaultView, ["genre"]);

    expect(albumCodec.parse("?cols=").columns).toEqual(["title"]);
    expect(genre.columns).toEqual(["title", "genre"]);
    expect(albumCodec.serialize(genre)).toBe("?cols=title%2Cgenre");
    expect(albumCodec.serialize(albumCodec.setColumns(genre, ["minutes", "title"]))).toBe("");
  });

  it("resets the sort when the sorted column is hidden", () => {
    const sorted = albumCodec.parse("?sort=-min");
    expect(sorted.sort).toEqual({ column: "minutes", direction: "desc" });

    const hidden = albumCodec.setColumns(sorted, ["title"]);
    expect(hidden.sort).toEqual({ column: "title", direction: "asc" });
    expect(albumCodec.parse("?sort=-min&cols=title").sort).toEqual(hidden.sort);
  });

  describe("default chips and a default sort on a shown column", () => {
    const codec = createDataTableViewCodec({
      ...albumTable,
      defaultPredicates: [{ field: "mood", values: ["calm"] }],
      defaultSort: { column: "minutes", direction: "desc" },
    });

    it("starts from the default chips and writes nothing for the default view", () => {
      expect(codec.defaultView.predicates).toEqual([{ field: "mood", values: ["calm"] }]);
      expect(codec.defaultView.sort).toEqual({ column: "minutes", direction: "desc" });
      expect(codec.serialize(codec.defaultView)).toBe("");
      expect(codec.parse("").predicates).toEqual(codec.defaultView.predicates);
      expect(codec.isDefault(codec.parse(""))).toBe(true);
    });

    it("records a removed default chip as an empty f and reads it back", () => {
      const removed = { ...codec.defaultView, predicates: [] };

      expect(codec.serialize(removed)).toBe("?f=");
      expect(codec.parse("?f=").predicates).toEqual([]);
      expect(codec.isDefault(removed)).toBe(false);
      expect(codec.apply("?foo=1&mood=calm", removed)).toBe("?foo=1&f=");
    });

    it("keeps a changed default chip value and other chips, replacing the defaults", () => {
      const changed = {
        ...codec.defaultView,
        predicates: [
          { field: "mood" as const, values: ["loud"] },
          { field: "genre" as const, values: ["jazz"] },
        ],
      };

      expect(codec.serialize(changed)).toBe("?f=mood%2Cgenre&mood=loud&genre=jazz");
      expect(codec.parse(codec.serialize(changed)).predicates).toEqual(changed.predicates);
      expect(codec.parse("?genre=jazz").predicates).toEqual([{ field: "genre", values: ["jazz"] }]);
    });

    it("sorts by the default column and falls back to the pinned sort while it is hidden", () => {
      const hidden = codec.setColumns(codec.defaultView, ["title"]);

      expect(codec.parse("?sort=name").sort).toEqual({ column: "title", direction: "asc" });
      expect(hidden.sort).toEqual({ column: "title", direction: "asc" });
      expect(codec.parse("?cols=title").sort).toEqual({ column: "title", direction: "asc" });
      expect(codec.serialize(hidden)).toBe("?cols=title");
      expect(codec.parse(codec.serialize(hidden))).toEqual(hidden);
    });

    it("follows the default sort as its column hides and returns, keeping a chosen sort", () => {
      const hidden = codec.setColumns(codec.defaultView, ["title"]);
      const shown = codec.setColumns(hidden, ["title", "minutes"]);

      expect(shown).toEqual(codec.defaultView);
      expect(codec.isDefault(shown)).toBe(true);

      const chosen = {
        ...codec.defaultView,
        sort: { column: "title" as const, direction: "desc" as const },
      };
      expect(
        codec.setColumns(codec.setColumns(chosen, ["title"]), ["title", "minutes"]).sort,
      ).toEqual(chosen.sort);
    });

    it("leaves a definition without defaults serializing an empty chip list to nothing", () => {
      expect(albumCodec.serialize({ ...albumCodec.defaultView, predicates: [] })).toBe("");
    });
  });

  it("merges the view into a search, keeping other parameters and writing the view before panel", () => {
    const search = "?foo=1&sort=-name&genre=old&panel=apps%2Fcatalog%3Aalpha";

    expect(albumCodec.apply(search, fullView)).toBe(
      "?foo=1&q=blue+train&f=mood%2Cgenre&mood=loud&mood=calm&genre=rock&genre=jazz&sort=-min&cols=title%2Cmin%2Cgenre&panel=apps%2Fcatalog%3Aalpha",
    );
    expect(albumCodec.apply(search, albumCodec.defaultView)).toBe(
      "?foo=1&panel=apps%2Fcatalog%3Aalpha",
    );
    expect(albumCodec.apply("?panel=a&panel=b&q=x", albumCodec.defaultView)).toBe(
      "?panel=a&panel=b",
    );
    expect(albumCodec.apply("?q=x", albumCodec.defaultView)).toBe("");
  });

  it("accepts a field and a column sharing a key, since columns only travel inside cols", () => {
    expect(albumTable.fields.find(({ id }) => id === "genre")?.key).toBe("genre");
    expect(albumTable.columns.find(({ id }) => id === "genre")?.key).toBe("genre");
    expect(albumCodec.serialize({ ...fullView, sort: albumCodec.defaultView.sort })).toBe(
      "?q=blue+train&f=mood%2Cgenre&mood=loud&mood=calm&genre=rock&genre=jazz&cols=title%2Cmin%2Cgenre",
    );
  });

  it.each([
    [
      "a field key reserved for the search",
      { fields: [{ ...moodField, key: "q" }] },
      'Data table field key "q" collides with a reserved view key.',
    ],
    [
      "a field key reserved for the panel stack",
      { fields: [{ ...moodField, key: "panel" }] },
      'Data table field key "panel" collides with a reserved view key.',
    ],
    [
      "two fields sharing a key",
      { fields: [moodField, { ...moodField, id: "genre" as const }] },
      'Data table field key "mood" is used more than once.',
    ],
    [
      "two columns sharing a key",
      { columns: [titleColumn, { ...minutesColumn, key: "title" }] },
      'Data table column key "title" is used more than once.',
    ],
    [
      "two columns sharing a sort key",
      { columns: [titleColumn, { ...minutesColumn, sort: { key: "name" } }] },
      'Data table sort key "name" is used more than once.',
    ],
    [
      "a key that can't travel in a URL list",
      { fields: [{ ...moodField, key: "mood,genre" }] },
      'Data table key "mood,genre" is not a plain URL key.',
    ],
    [
      "a default sort on a column hidden by default",
      {
        columns: [titleColumn, { ...minutesColumn, defaultVisible: false }],
        defaultSort: { column: "minutes" as const, direction: "asc" as const },
      },
      'Data table default sort "minutes" is not a sortable column shown by default.',
    ],
    [
      "a default sort on an unsortable column",
      { defaultSort: { column: "genre" as const, direction: "asc" as const } },
      'Data table default sort "genre" is not a sortable column shown by default.',
    ],
    [
      "a table without a pinned sortable column to fall back to",
      {
        columns: [{ ...titleColumn, pinned: false, defaultVisible: true }, minutesColumn],
      },
      "Data table has no pinned sortable column to fall back to.",
    ],
  ])("rejects %s", (_, overrides, message) => {
    const definition: DataTableViewDefinition<AlbumColumn, AlbumField> = {
      columns: [titleColumn, minutesColumn],
      defaultSort: { column: "title", direction: "asc" },
      fields: [moodField],
      ...overrides,
    };

    expect(() => createDataTableViewCodec(definition)).toThrow(message);
  });
});
