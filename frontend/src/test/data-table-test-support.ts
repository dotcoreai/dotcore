import type { DataTableDefinition } from "@/components/data-table/data-table-model";
import { createDataTableViewCodec } from "@/components/data-table/data-table-view";

export interface Album {
  id: string;
  name: string;
  tracks: Track[];
}

export interface Track {
  featured?: boolean;
  genres: string[];
  id: string;
  minutes?: number;
  moods: string[];
  name: string;
}

export type AlbumColumn = "title" | "minutes" | "genre";

export type AlbumField = "genre" | "mood";

/**
 * A synthetic table of albums expanding into tracks, for the generic data table tests: a pinned
 * name-sorted column, a value-sorted column whose sort key is its column key, an unsortable column
 * sharing its key with a field, an open "any" field, and a closed "all" field.
 */
export const albumTable: DataTableDefinition<Album, Track, AlbumColumn, AlbumField> = {
  children: (album) => album.tracks,
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
      id: "minutes",
      key: "min",
      label: "Minutes",
      sort: { kind: "value", value: (track) => track.minutes },
    },
    { id: "genre", key: "genre", label: "Genre" },
  ],
  // Featured tracks first, then by name.
  compareChildren: (left, right) =>
    Number(Boolean(right.featured)) - Number(Boolean(left.featured)) ||
    left.name.localeCompare(right.name) ||
    left.id.localeCompare(right.id),
  defaultSort: { column: "title", direction: "asc" },
  fields: [
    {
      domain: { kind: "open", label: (value) => value.toUpperCase() },
      id: "genre",
      key: "genre",
      label: "Genre",
      match: "any",
      values: (track) => track.genres,
    },
    {
      domain: {
        kind: "closed",
        values: [
          { label: "Calm", value: "calm" },
          { label: "Loud", value: "loud" },
          { label: "Sad", value: "sad" },
        ],
      },
      id: "mood",
      key: "mood",
      label: "Mood",
      match: "all",
      values: (track) => track.moods,
    },
  ],
  rowId: (album) => album.id,
  rowName: (album) => album.name,
  searchText: (album) => [album.name, album.id],
};

export const albumCodec = createDataTableViewCodec(albumTable);

/**
 * Albums covering a childless album, a lowercase name, a tie on minutes, a track without minutes,
 * and a featured track.
 */
export function syntheticAlbums(): Album[] {
  return [
    album("kind-of-blue", "Kind of Blue", [
      track("so-what", "So What", { genres: ["jazz"], minutes: 9, moods: ["calm"] }),
      track("blue-in-green", "Blue in Green", {
        featured: true,
        genres: ["jazz"],
        minutes: 5,
        moods: ["calm", "sad"],
      }),
    ]),
    album("paranoid", "Paranoid", [
      track("war-pigs", "War Pigs", { genres: ["rock", "metal"], minutes: 8, moods: ["loud"] }),
      track("iron-man", "Iron Man", {
        featured: true,
        genres: ["metal"],
        minutes: 6,
        moods: ["loud", "sad"],
      }),
      track("electric-funeral", "Electric Funeral", {
        genres: ["metal"],
        minutes: 6,
        moods: ["loud"],
      }),
      track("planet-caravan", "Planet Caravan", { genres: ["rock"], moods: ["calm"] }),
    ]),
    album("empty", "empty", []),
    album("abbey", "Abbey Road", [
      track("something", "Something", { genres: ["rock"], minutes: 3, moods: ["calm"] }),
    ]),
    album("zebra", "Zebra", [
      track("stripes", "Stripes", { genres: ["pop"], minutes: 3, moods: ["loud"] }),
    ]),
  ];
}

function album(id: string, name: string, tracks: Track[]): Album {
  return { id, name, tracks };
}

function track(id: string, name: string, values: Omit<Track, "id" | "name">): Track {
  return { id, name, ...values };
}
