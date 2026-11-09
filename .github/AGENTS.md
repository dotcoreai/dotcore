# GitHub agentic instructions

`.github/` holds the CI workflows and the local action they share; the recipes they run live in
[`ci/`](../ci).

## Workflows

- After editing a workflow or a local action, run `just ci::lint`.
- Start each workflow with a one-line comment saying what it runs and when; workflows have no
  description field.
- Required status checks on `main`:
  - every `CI` job
  - `check commit prefixes`, in `PR commits`
- Renaming a required job, or moving it into a reusable workflow (which prefixes job names),
  blocks every PR until the branch rule is updated.
- The cross-platform task list is in both `ci.yml` and `ci-macos.yml`. Keep them in step with each
  other and with `just ci::check`.
- Pin third-party actions to a commit, with the version as a comment.
- Don't add label events to `CI`: each label starts a run, and a skipped run hides the real result
  on the PR. The `ci:macos` label belongs to `CI macOS`.
