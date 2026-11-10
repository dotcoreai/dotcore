import { render, screen, within } from "@testing-library/react";
import { ListMusicIcon } from "lucide-react";
import { createRef, type RefObject, useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { buildDataTable, type DataTableDefinition } from "@/components/data-table/data-table-model";
import {
  createDataTableViewCodec,
  type DataTableView,
} from "@/components/data-table/data-table-view";
import { type Album, syntheticAlbums } from "@/test/data-table-test-support";
import { setupUser } from "@/test/user";

import { DataTable, type DataTableUiDefinition } from "./data-table";
import { Metric } from "./data-table-cells";
import { DataTableToolbar } from "./data-table-toolbar";

type FlatColumn = "title" | "tracks";

type FlatField = "genre";

// A flat table: each album is its own matching unit, so rows never expand.
const flatAlbums: DataTableDefinition<Album, Album, FlatColumn, FlatField> = {
  columns: [
    {
      id: "title",
      key: "title",
      label: "Title",
      pinned: true,
      sort: { key: "name", kind: "name" },
    },
    {
      defaultVisible: true,
      id: "tracks",
      key: "tracks",
      label: "Tracks",
      sort: { kind: "value", value: (album) => album.tracks.length },
    },
  ],
  defaultSort: { column: "title", direction: "asc" },
  fields: [
    {
      domain: { kind: "open" },
      id: "genre",
      key: "genre",
      label: "Genre",
      match: "any",
      values: (album) => album.tracks.flatMap((track) => track.genres),
    },
  ],
  rowId: (album) => album.id,
  rowName: (album) => album.name,
  searchText: (album) => [album.name],
};

const flatUi: DataTableUiDefinition<Album, Album, FlatColumn, FlatField> = {
  codec: createDataTableViewCodec(flatAlbums),
  columns: {
    title: { cell: ({ row }) => row.name },
    tracks: { cell: ({ row }) => <Metric value={row.tracks.length} />, showFrom: "40rem" },
  },
  definition: flatAlbums,
  fields: { genre: { icon: ListMusicIcon } },
  label: "Albums",
  noun: { one: "album", other: "albums" },
};

function FlatTable({
  albums,
  initialView,
  onOpenRow,
}: {
  albums: Album[];
  initialView: DataTableView<FlatColumn, FlatField>;
  onOpenRow: (albumId: string, opener: HTMLElement) => void;
}) {
  const [view, setView] = useState(initialView);
  return (
    <DataTable
      onOpenRow={({ row }, opener) => onOpenRow(row.id, opener)}
      onViewChange={setView}
      table={buildDataTable(flatAlbums, albums, view)}
      ui={flatUi}
      view={view}
    />
  );
}

function renderTable({
  albums = syntheticAlbums(),
  search = "",
}: { albums?: Album[]; search?: string } = {}) {
  const onOpenRow = vi.fn<(albumId: string, opener: HTMLElement) => void>();
  render(
    <FlatTable
      albums={albums}
      initialView={{ ...flatUi.codec.defaultView, search }}
      onOpenRow={onOpenRow}
    />,
  );
  return { onOpenRow, user: setupUser() };
}

function headers(): string[] {
  return within(screen.getByRole("table", { name: "Albums" }))
    .getAllByRole("columnheader")
    .map((header) => header.textContent);
}

function bodyRows(): string[] {
  const [, body] = within(screen.getByRole("table", { name: "Albums" })).getAllByRole("rowgroup");
  return within(body)
    .getAllByRole("row")
    .map((row) => row.getAttribute("aria-label") ?? row.textContent);
}

describe("DataTable", () => {
  it("shows a flat table's rows by name without expand controls, and counts them", () => {
    renderTable();

    expect(headers()).toEqual(["Title", "Tracks"]);
    expect(bodyRows()).toEqual(["Abbey Road", "empty", "Kind of Blue", "Paranoid", "Zebra"]);
    expect(screen.queryByRole("button", { name: /^(Show|Hide) / })).not.toBeInTheDocument();
    expect(screen.getByText("5 of 5 albums")).toBeInTheDocument();
  });

  it("limits a column to tables of its width, in the header and every row", () => {
    renderTable();

    const table = screen.getByRole("table", { name: "Albums" });
    const [titleHeader, tracksHeader] = within(table).getAllByRole("columnheader");
    const [, body] = within(table).getAllByRole("rowgroup");
    const tracksCells = within(body)
      .getAllByRole("row")
      .map((row) => within(row).getAllByRole("cell").at(-1));

    expect(titleHeader).not.toHaveClass("hidden");
    expect(tracksHeader).toHaveClass("hidden", "@min-[40rem]:table-cell");
    expect(tracksCells).toHaveLength(5);
    for (const cell of tracksCells) expect(cell).toHaveClass("hidden", "@min-[40rem]:table-cell");
  });

  it("sorts from a header menu and hides a column, but never the pinned one", async () => {
    const { user } = renderTable();

    await user.click(screen.getByRole("button", { name: "Tracks column options" }));
    await user.click(await screen.findByRole("menuitem", { name: "High → low" }));

    expect(bodyRows()).toEqual(["Paranoid", "Kind of Blue", "Abbey Road", "Zebra", "empty"]);
    expect(screen.getByRole("columnheader", { name: /Tracks/ })).toHaveAttribute(
      "aria-sort",
      "descending",
    );

    await user.click(screen.getByRole("button", { name: "Title column options" }));
    expect(await screen.findByRole("menuitem", { name: "A → Z" })).toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: "Hide column" })).not.toBeInTheDocument();
    await user.keyboard("{Escape}");

    await user.click(screen.getByRole("button", { name: "Tracks column options" }));
    await user.click(await screen.findByRole("menuitem", { name: "Hide column" }));

    expect(headers()).toEqual(["Title"]);
    // Hiding the sorted column falls back to the name sort.
    expect(bodyRows()).toEqual(["Abbey Road", "empty", "Kind of Blue", "Paranoid", "Zebra"]);
  });

  it("opens a row by click, Enter, and Space", async () => {
    const { onOpenRow, user } = renderTable();
    const paranoid = screen.getByRole("row", { name: "Paranoid" });
    const zebra = screen.getByRole("row", { name: "Zebra" });

    await user.click(paranoid);
    zebra.focus();
    await user.keyboard("{Enter}");
    await user.keyboard(" ");

    expect(onOpenRow.mock.calls).toEqual([
      ["paranoid", paranoid],
      ["zebra", zebra],
      ["zebra", zebra],
    ]);
  });

  it("shows a no-match row whose reset clears the search", async () => {
    const { user } = renderTable({ search: "nothing" });

    expect(bodyRows()).toEqual(["No albums match. Reset filters"]);
    expect(screen.getByText("0 of 5 albums")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Reset filters" }));

    expect(bodyRows()).toHaveLength(5);
    expect(screen.getByText("5 of 5 albums")).toBeInTheDocument();
  });

  it("counts a single row with the singular noun", () => {
    renderTable({ albums: syntheticAlbums().slice(0, 1) });

    expect(screen.getByText("1 of 1 album")).toBeInTheDocument();
  });
});

describe("DataTable selection", () => {
  it("selects one row at a time from a row click or its radio", async () => {
    const { onSelect, user } = renderSelectable({ selectedId: "kind-of-blue" });

    expect(headers()).toEqual(["Selected", "Title", "Tracks"]);
    expect(checkedRadios()).toEqual(["Kind of Blue"]);
    expect(screen.getByRole("row", { name: "Kind of Blue" })).toHaveAttribute(
      "data-selected",
      "true",
    );

    await user.click(screen.getByRole("row", { name: "Paranoid" }));
    expect(checkedRadios()).toEqual(["Paranoid"]);
    expect(screen.getByRole("row", { name: "Paranoid" })).toHaveAttribute("data-selected", "true");
    expect(screen.getByRole("row", { name: "Kind of Blue" })).toHaveAttribute(
      "data-selected",
      "false",
    );

    await user.click(screen.getByRole("radio", { name: "Zebra" }));
    // Clicking the selected row again selects nothing new.
    await user.click(screen.getByRole("row", { name: "Zebra" }));
    expect(checkedRadios()).toEqual(["Zebra"]);
    expect(onSelect.mock.calls).toEqual([["paranoid"], ["zebra"]]);
  });

  it("lists a disabled row that can't be focused or selected", async () => {
    const { onSelect, user } = renderSelectable({ disabledIds: ["paranoid"] });
    const paranoid = screen.getByRole("row", { name: "Paranoid" });

    expect(paranoid).toHaveAttribute("aria-disabled", "true");
    expect(screen.getByRole("radio", { name: "Paranoid" })).toBeDisabled();
    expect(screen.getByRole("row", { name: "Zebra" })).not.toHaveAttribute("aria-disabled");

    await user.click(paranoid);
    expect(checkedRadios()).toEqual([]);
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("moves the selection with arrow keys, skipping disabled rows", async () => {
    const { onSelect, user } = renderSelectable({
      disabledIds: ["kind-of-blue"],
      selectedId: "empty",
    });

    screen.getByRole("radio", { name: "empty" }).focus();
    await user.keyboard("{ArrowDown}");

    expect(screen.getByRole("radio", { name: "Paranoid" })).toHaveFocus();
    expect(checkedRadios()).toEqual(["Paranoid"]);
    await user.keyboard("{ArrowUp}");
    expect(checkedRadios()).toEqual(["empty"]);
    expect(onSelect.mock.calls).toEqual([["paranoid"], ["empty"]]);
  });

  it("hands the selected radio, or the first enabled one, to the input ref", () => {
    const inputRef = createRef<HTMLInputElement>();
    const { unmount } = render(
      <SelectableTable disabledIds={["abbey"]} inputRef={inputRef} selectedId="zebra" />,
    );
    expect(inputRef.current).toBe(screen.getByRole("radio", { name: "Zebra" }));
    unmount();

    render(<SelectableTable disabledIds={["abbey"]} inputRef={inputRef} />);
    expect(inputRef.current).toBe(screen.getByRole("radio", { name: "empty" }));
  });

  it("offers neither the View menu nor column hiding for fixed columns", async () => {
    const user = setupUser();
    const ui = { ...flatUi, fixedColumns: true };
    render(
      <DataTableToolbar
        facets={buildDataTable(flatAlbums, syntheticAlbums(), ui.codec.defaultView).facets}
        onViewChange={vi.fn()}
        ui={ui}
        view={ui.codec.defaultView}
      />,
    );

    expect(screen.getByRole("searchbox", { name: "Filter albums" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Filter" })).toBeVisible();
    expect(screen.queryByRole("button", { name: "View" })).not.toBeInTheDocument();

    render(<SelectableTable ui={ui} />);
    await user.click(screen.getByRole("button", { name: "Tracks column options" }));
    expect(await screen.findByRole("menuitem", { name: "High → low" })).toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: "Hide column" })).not.toBeInTheDocument();
  });
});

describe("DataTableToolbar chip settings", () => {
  // A default view with a Genre chip, so a setting on it shows in the default view.
  const genreView = {
    ...flatUi.codec.defaultView,
    predicates: [{ field: "genre" as const, values: ["Jazz"] }],
  };
  const genreUi = {
    ...flatUi,
    codec: {
      ...flatUi.codec,
      defaultView: genreView,
      isDefault: (view: DataTableView<FlatColumn, FlatField>) =>
        flatUi.codec.serialize(view) === flatUi.codec.serialize(genreView),
    },
  };

  function renderToolbar(props: { onReset?: () => void; reset?: () => void }) {
    const onViewChange = vi.fn<(view: DataTableView<FlatColumn, FlatField>) => void>();
    render(
      <DataTableToolbar
        chipSettings={{ genre: { menu: null, reset: props.reset, summary: "Loud" } }}
        facets={buildDataTable(flatAlbums, syntheticAlbums(), genreView).facets}
        onReset={props.onReset}
        onViewChange={onViewChange}
        ui={genreUi}
        view={genreView}
      />,
    );
    return { onViewChange, user: setupUser() };
  }

  it("hides Reset while the view and its chip settings are at their defaults", () => {
    renderToolbar({});

    expect(screen.getByRole("button", { name: "Genre filter: Jazz, Loud" })).toBeVisible();
    expect(screen.queryByRole("button", { name: /^Reset/ })).not.toBeInTheDocument();
  });

  it("shows Reset for a changed chip setting, restoring the setting and the view", async () => {
    const reset = vi.fn();
    const { onViewChange, user } = renderToolbar({ reset });

    await user.click(screen.getByRole("button", { name: "Reset" }));

    expect(reset).toHaveBeenCalledTimes(1);
    expect(onViewChange.mock.calls).toEqual([[genreView]]);
  });

  it("hands Reset to the host's own reset when it has one", async () => {
    const onReset = vi.fn();
    const reset = vi.fn();
    const { onViewChange, user } = renderToolbar({ onReset, reset });

    await user.click(screen.getByRole("button", { name: "Reset" }));

    expect(onReset).toHaveBeenCalledTimes(1);
    expect(reset).not.toHaveBeenCalled();
    expect(onViewChange).not.toHaveBeenCalled();
  });
});

describe("DataTableToolbar chip value menu", () => {
  function StatefulToolbar({ replaceOnSelect }: { replaceOnSelect: boolean }) {
    const [view, setView] = useState<DataTableView<FlatColumn, FlatField>>({
      ...flatUi.codec.defaultView,
      predicates: [{ field: "genre", values: ["jazz"] }],
    });
    return (
      <DataTableToolbar
        facets={buildDataTable(flatAlbums, syntheticAlbums(), view).facets}
        onViewChange={setView}
        ui={{ ...flatUi, fields: { genre: { icon: ListMusicIcon, replaceOnSelect } } }}
        view={view}
      />
    );
  }

  async function openGenreMenu(replaceOnSelect: boolean) {
    const user = setupUser();
    render(<StatefulToolbar replaceOnSelect={replaceOnSelect} />);
    await user.click(screen.getByRole("button", { name: /^Genre filter/ }));
    return user;
  }

  const chipSummary = () => screen.getByRole("button", { name: /^Genre filter/ }).textContent;

  it("adds a clicked value to the selection by default", async () => {
    const user = await openGenreMenu(false);

    await user.click(await screen.findByRole("menuitemcheckbox", { name: /^rock/ }));

    expect(chipSummary()).toBe("Genrejazz, rock");
  });

  it("replaces the selection with a clicked value when the field replaces on select", async () => {
    const user = await openGenreMenu(true);

    await user.click(await screen.findByRole("menuitemcheckbox", { name: /^rock/ }));

    expect(chipSummary()).toBe("Genrerock");
  });

  it("adds a value on shift-click and removes it on a second shift-click", async () => {
    const user = await openGenreMenu(true);
    const rock = await screen.findByRole("menuitemcheckbox", { name: /^rock/ });

    await user.keyboard("{Shift>}");
    await user.click(rock);
    expect(chipSummary()).toBe("Genrejazz, rock");

    await user.click(rock);
    await user.keyboard("{/Shift}");
    expect(chipSummary()).toBe("Genrejazz");
  });

  it("hints at shift-click only for a field that replaces on select", async () => {
    await openGenreMenu(true);
    expect(await screen.findByText("Shift-click to select more than one")).toBeVisible();
  });

  it("shows no shift-click hint for an additive field", async () => {
    await openGenreMenu(false);
    await screen.findByRole("menuitemcheckbox", { name: /^rock/ });
    expect(screen.queryByText("Shift-click to select more than one")).not.toBeInTheDocument();
  });

  it("clears the chip when the only selected value is clicked", async () => {
    const user = await openGenreMenu(true);

    await user.click(await screen.findByRole("menuitemcheckbox", { name: /^jazz/ }));

    expect(chipSummary()).toBe("Genreany");
  });
});

function SelectableTable({
  disabledIds = [],
  inputRef,
  onSelect,
  selectedId: initialSelectedId,
  ui = flatUi,
}: {
  disabledIds?: string[];
  inputRef?: RefObject<HTMLInputElement | null>;
  onSelect?: (albumId: string) => void;
  selectedId?: string;
  ui?: DataTableUiDefinition<Album, Album, FlatColumn, FlatField>;
}) {
  const [view, setView] = useState(ui.codec.defaultView);
  const [selectedId, setSelectedId] = useState(initialSelectedId);
  return (
    <DataTable
      onViewChange={setView}
      select={{
        disabled: ({ row }) => disabledIds.includes(row.id),
        inputRef,
        onSelect: ({ row }) => {
          setSelectedId(row.id);
          onSelect?.(row.id);
        },
        selectedId,
      }}
      table={buildDataTable(flatAlbums, syntheticAlbums(), view)}
      ui={ui}
      view={view}
    />
  );
}

function renderSelectable(props: { disabledIds?: string[]; selectedId?: string } = {}) {
  const onSelect = vi.fn<(albumId: string) => void>();
  render(<SelectableTable {...props} onSelect={onSelect} />);
  return { onSelect, user: setupUser() };
}

function checkedRadios(): string[] {
  return screen
    .getAllByRole("radio")
    .filter((radio) => (radio as HTMLInputElement).checked)
    .map((radio) => radio.getAttribute("aria-label") ?? "");
}
