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
- Reference actions by full version tag (`actions/checkout@v7.0.1`). `.github/workflows/actions.lock`
  records the commit behind each tag, so a moved tag changes nothing until someone relocks.
- Never edit the lockfile by hand. After changing a `uses:` line, in a workflow or a local action, or
  adding a workflow, run `just ci::lock` and commit the lockfile with the change.
- Reference the local action as `$/.github/actions/ci-task`, not `./`, so the lockfile covers the actions
  it uses. `.github/actionlint.yaml` silences actionlint's complaint about that syntax; delete it once a
  release accepts `$/`.
- The `lockfile` job in `CI` does what `just ci::lock-verify` does. A stale lockfile otherwise makes a
  workflow fail to start, with no check on the PR. Its extension version is a manual bump point: raise it
  when you relock.
- Don't add label events to `CI`: each label starts a run, and a skipped run hides the real result
  on the PR. The `ci:macos` label belongs to `CI macOS`.
