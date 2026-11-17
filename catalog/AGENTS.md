# Catalog agentic instructions

`catalog/` holds the master catalog: one YAML file per app, engine release, model, hardware class
and recommendation list. The [catalog spec](../docs/specs/catalog.md) says what each declaration
is, and `backend/catalog/` loads and validates them.

Use the `add-app` skill to add an app, the `add-model` skill to add a model and the `bump-engines`
skill to bump an engine. [`engines/AGENTS.md`](../engines/AGENTS.md) says where engine rules live.

## Declarations

- Leave out optional fields you cannot support.

## Parameters

- Every parameter bound (length bounds, ranges, accepted values; test fixtures carry no source
  comments) comes from the app, engine or model it describes and names its source in a short YAML
  comment: a constant or argument parser at the pinned version, its docs, or the model's own
  limits. Never pick a cap: an engine with no limit declares no maximum. The one exception is
  Dotcore's context floor, the smallest context minimum, which an engine YAML names as its source
  ([catalog spec](../docs/specs/catalog.md#invariants)).
- Group parameters that belong together under `parameterGroups`, and set each member's `groupId`;
  members are contiguous. A plain group has a display name and a description.
- Only apps declare conditions. For an optional set of parameters, make a switched group whose
  `switchParameterId` is its boolean first member: that parameter titles the group and enables the
  others. For one dependent parameter, set `enabledByParameterId`. A disabled parameter keeps no
  value and its bindings never apply.
- `proto/dotcore/catalog/v1/parameter.proto` owns the full rules for groups and conditions.
