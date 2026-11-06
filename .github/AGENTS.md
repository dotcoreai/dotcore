# GitHub agentic instructions

`.github/` holds the CI workflows and the local action they share; the recipes they run live in
[`ci/`](../ci).

## Workflows

- After editing a workflow or a local action, run `just ci::lint`.
- The `CI` job names are the required status checks on `main`. Renaming one, or moving jobs into
  a reusable workflow (which prefixes job names), blocks every PR until the branch rule is
  updated.
- The cross-platform task list is in both `ci.yml` and `ci-macos.yml`. Keep them in step with each
  other and with `just ci::check`.
- Pin third-party actions to a commit, with the version as a comment.
- Don't add label events to `CI`: each label starts a run, and a skipped run hides the real result
  on the PR. The `ci:macos` label belongs to `CI macOS`.
