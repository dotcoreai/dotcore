# dotc

Dotcore runs AI models and apps on hardware people own, and serves the models behind one
OpenAI-compatible endpoint. `dotc` is its command-line tool: it manages a site (one or more
machines), its nodes, and its model and application deployments.

## Conventions

- Output: a command prints its result on stdout for a person. Failures go to stderr with a
  non-zero exit code.
- Scripts and agents: add `--yaml` to get exactly one YAML document on stdout. A command with
  `--yaml` never prompts; pass `--yes` where it would ask for a confirmation.
- `--yaml` is the API's proto JSON mapping: keys are lowerCamelCase, 64-bit numbers are quoted
  strings, absent fields mean zero, false or empty, and enums use their full names.
- A deployment, node or route reference is its ID, its name, a unique part of its ID (`3cfmn`) or
  a unique word or prefix of its name (`27b`). An ambiguous reference fails and lists the
  candidates; pick one by ID. Deployment IDs look like `<slug>-<5 chars>`.
