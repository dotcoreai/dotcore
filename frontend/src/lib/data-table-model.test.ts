import { describe, expect, it } from "vitest";

import { albumCodec, albumTable, syntheticAlbums } from "../test/data-table-test-support";
import {
  buildDataTable,
  type DataTableDefinition,
  formatRange,
  valueRange,
} from "./data-table-model";
import { createDataTableViewCodec } from "./data-table-view";

const albums = syntheticAlbums();

function build(search = "") {
  return buildDataTable(albumTable, albums, albumCodec.parse(search));
}

function order(search = ""): string[] {
  return build(search).rows.map(({ row }) => row.id);
}

function tracks(search: string, albumId: string): string[] {
  return build(search)
    .rows.find(({ row }) => row.id === albumId)!
    .children.map(({ id }) => id);
}

describe("matching", () => {
  it("accepts any selected value of an any field", () => {
    const table = build("?genre=rock&genre=jazz");

    expect(
      table.rows.map(({ row, children, hiddenChildCount }) => [
        row.id,
        children.length,
        hiddenChildCount,
      ]),
    ).toEqual([
      ["abbey", 1, 0],
      ["kind-of-blue", 2, 0],
      ["paranoid", 2, 2],
    ]);
    expect(table.shownCount).toBe(3);
    expect(table.totalCount).toBe(5);
  });

  it("requires every selected value of an all field on the same child", () => {
    const table = build("?mood=calm&mood=sad");

    expect(table.rows.map(({ row }) => row.id)).toEqual(["kind-of-blue"]);
    expect(table.rows[0]).toMatchObject({ expandable: true, hiddenChildCount: 1 });
    expect(tracks("?mood=calm&mood=sad", "kind-of-blue")).toEqual(["blue-in-green"]);
  });

  it("combines fields per child", () => {
    expect(order("?genre=metal&mood=sad")).toEqual(["paranoid"]);
    expect(tracks("?genre=metal&mood=sad", "paranoid")).toEqual(["iron-man"]);
    expect(order("?genre=jazz&mood=loud")).toEqual([]);
  });

  it("treats a shown predicate without values as matching everything", () => {
    const table = build("?f=mood");

    expect(table.rows.map(({ row, hiddenChildCount }) => [row.id, hiddenChildCount])).toEqual([
      ["abbey", 0],
      ["empty", 0],
      ["kind-of-blue", 0],
      ["paranoid", 0],
      ["zebra", 0],
    ]);
  });

  it("shows a childless row only while no filter value is selected", () => {
    const [empty] = build("?q=empty").rows;

    expect(empty).toEqual({
      children: [],
      expandable: false,
      hiddenChildCount: 0,
      row: albums[2],
    });
    expect(order("?q=empty&genre=rock")).toEqual([]);
  });
});

describe("facet counts", () => {
  it("drops an any field's own selection and applies every other predicate", () => {
    const { facets } = build("?genre=rock");

    expect(facets.genre).toEqual([
      { count: 1, label: "JAZZ", value: "jazz" },
      { count: 1, label: "METAL", value: "metal" },
      { count: 1, label: "POP", value: "pop" },
      { count: 2, label: "ROCK", value: "rock" },
    ]);
    expect(facets.mood).toEqual([
      { count: 2, label: "Calm", value: "calm" },
      { count: 1, label: "Loud", value: "loud" },
      { count: 0, label: "Sad", value: "sad" },
    ]);
  });

  it("keeps an all field's other selected values when counting one of its values", () => {
    const { facets } = build("?mood=calm");

    expect(facets.mood).toEqual([
      { count: 3, label: "Calm", value: "calm" },
      { count: 0, label: "Loud", value: "loud" },
      { count: 1, label: "Sad", value: "sad" },
    ]);
  });

  it("applies the search to the counts", () => {
    const { facets } = build("?q=paranoid");

    expect(facets.genre.map(({ count, value }) => [value, count])).toEqual([
      ["jazz", 0],
      ["metal", 1],
      ["pop", 0],
      ["rock", 1],
    ]);
  });

  it("keeps a selected value missing from the data with a zero count", () => {
    const { facets } = build("?genre=polka");

    expect(facets.genre.at(-1)).toEqual({ count: 0, label: "POLKA", value: "polka" });
    expect(facets.genre).toHaveLength(5);
  });
});

describe("search", () => {
  it("matches every search text, ignoring case and surrounding spaces", () => {
    expect(order("?q=BLUE")).toEqual(["kind-of-blue"]);
    expect(order("?q=of-blue")).toEqual(["kind-of-blue"]);
    expect(order("?q=++paranoid+")).toEqual(["paranoid"]);
    expect(build("?q=nothing").rows).toEqual([]);
    expect(build("?q=nothing").totalCount).toBe(5);
  });
});

describe("sorting", () => {
  it("sorts by name in both directions, ignoring case", () => {
    expect(order()).toEqual(["abbey", "empty", "kind-of-blue", "paranoid", "zebra"]);
    expect(order("?sort=-name")).toEqual(["zebra", "paranoid", "kind-of-blue", "empty", "abbey"]);
  });

  it("sorts ascending by the lowest and descending by the highest value, missing last, ties on name", () => {
    expect(order("?sort=min")).toEqual(["abbey", "zebra", "kind-of-blue", "paranoid", "empty"]);
    expect(order("?sort=-min")).toEqual(["kind-of-blue", "paranoid", "abbey", "zebra", "empty"]);
  });

  it("sorts over the matching children only", () => {
    // Paranoid's only calm track has no length, so its 8-minute track doesn't count.
    expect(order("?mood=calm&sort=-min")).toEqual(["kind-of-blue", "abbey", "paranoid"]);
  });

  it("orders children by the comparator under the name sort, whatever its direction", () => {
    const named = ["iron-man", "electric-funeral", "planet-caravan", "war-pigs"];

    expect(tracks("", "paranoid")).toEqual(named);
    expect(tracks("?sort=-name", "paranoid")).toEqual(named);
    expect(tracks("", "kind-of-blue")).toEqual(["blue-in-green", "so-what"]);
  });

  it("orders children by value, missing last, then by the comparator", () => {
    expect(tracks("?sort=min", "paranoid")).toEqual([
      "iron-man",
      "electric-funeral",
      "war-pigs",
      "planet-caravan",
    ]);
    expect(tracks("?sort=-min", "paranoid")).toEqual([
      "war-pigs",
      "iron-man",
      "electric-funeral",
      "planet-caravan",
    ]);
  });
});

describe("rows with and without children", () => {
  it("expands a row with a matching child and counts hidden ones", () => {
    const rows = build("?genre=metal").rows.map(
      ({ children, expandable, hiddenChildCount, row }) => ({
        children: children.length,
        expandable,
        hiddenChildCount,
        id: row.id,
      }),
    );

    expect(rows).toEqual([{ children: 3, expandable: true, hiddenChildCount: 1, id: "paranoid" }]);
    expect(build("?q=abbey").rows[0]).toMatchObject({
      children: [expect.anything()],
      expandable: true,
      hiddenChildCount: 0,
    });
  });

  it("ranges over the matching children, collapsing to one value", () => {
    const minutes = (search: string, albumId: string) =>
      valueRange(
        build(search)
          .rows.find(({ row }) => row.id === albumId)!
          .children.map(({ minutes: value }) => value),
      );
    const format = (range: ReturnType<typeof minutes>) =>
      range ? formatRange(range, (value) => `${value} min`) : "—";

    expect(minutes("", "paranoid")).toEqual({ max: 8, min: 6 });
    expect(format(minutes("", "paranoid"))).toBe("6 min–8 min");
    expect(minutes("?mood=sad", "paranoid")).toEqual({ max: 6, min: 6 });
    expect(format(minutes("?mood=sad", "paranoid"))).toBe("6 min");
    expect(minutes("?mood=calm", "paranoid")).toBeUndefined();
    expect(minutes("", "empty")).toBeUndefined();
    expect(formatRange({ max: 1400, min: 1000 }, (value) => `${Math.round(value / 1000)}k`)).toBe(
      "1k",
    );
  });

  it("treats each row as its own matching unit in a table without children", () => {
    const table = buildDataTable(fruitTable, fruits, fruitCodec.parse("?color=red"));

    expect(table.rows).toEqual([
      { children: [], expandable: false, hiddenChildCount: 0, row: fruits[0] },
      { children: [], expandable: false, hiddenChildCount: 0, row: fruits[2] },
    ]);
    expect(table.facets.color).toEqual([
      { count: 1, label: "green", value: "green" },
      { count: 2, label: "red", value: "red" },
      { count: 1, label: "yellow", value: "yellow" },
    ]);
  });

  it("sorts rows of a table without children by their own value", () => {
    const sorted = (search: string) =>
      buildDataTable(fruitTable, fruits, fruitCodec.parse(search)).rows.map(({ row }) => row.id);

    expect(sorted("?sort=size")).toEqual(["cherry", "apple", "banana"]);
    expect(sorted("?sort=-size")).toEqual(["apple", "cherry", "banana"]);
  });
});

interface Fruit {
  colors: string[];
  id: string;
  name: string;
  size?: number;
}

const fruitTable: DataTableDefinition<Fruit, Fruit, "name" | "size", "color"> = {
  columns: [
    { id: "name", key: "name", label: "Name", pinned: true, sort: { kind: "name" } },
    {
      defaultVisible: true,
      id: "size",
      key: "size",
      label: "Size",
      sort: { kind: "value", value: (fruit) => fruit.size },
    },
  ],
  defaultSort: { column: "name", direction: "asc" },
  fields: [
    {
      domain: { kind: "open" },
      id: "color",
      key: "color",
      label: "Color",
      match: "any",
      values: (fruit) => fruit.colors,
    },
  ],
  rowId: (fruit) => fruit.id,
  rowName: (fruit) => fruit.name,
  searchText: (fruit) => [fruit.name],
};

const fruitCodec = createDataTableViewCodec(fruitTable);

const fruits: Fruit[] = [
  { colors: ["red", "green"], id: "apple", name: "Apple", size: 3 },
  { colors: ["yellow"], id: "banana", name: "Banana" },
  { colors: ["red"], id: "cherry", name: "cherry", size: 1 },
];
