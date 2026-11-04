# Protocol agentic instructions

`proto/` is the only source of the API contracts between the backend, the console and `dotc`.
`just --list proto` lists the recipes; they wrap Buf, so don't run Buf directly.

## Generated code

- `just proto::generate` regenerates the console's TypeScript in `frontend/src/gen/` (committed),
  then the Rust bindings, which the backend build writes to Cargo's `OUT_DIR` (never committed).
  Never edit either by hand.
- The generators come from the Nix toolchain. To change a generator version, change its Nix pin,
  regenerate, and review the TypeScript diff.

## Writing contracts

- Document each message in one or two lines and each field in one line.
- In each file, put services first, then messages from the ones callers use down to their
  dependencies, with enums and helper messages after the messages that use them.
- Group fields by meaning, keep field numbers stable, and put one empty line between fields.
- Dotcore has no deployments to stay compatible with: don't keep compatibility aliases or
  `reserved` names, numbers or ranges.
- Keep fast API calls separate from slow or expensive detail. Give an expensive call its own clear
  cost and owner, and state freshness and unavailability explicitly. When a call can partly succeed
  or streams, define what that means in the contract.

## Checks

- Run `just proto::check` (format, lint, build) after changing a contract, then the gates of the
  areas that use it.
