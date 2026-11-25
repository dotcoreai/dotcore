---
name: fix-bumps
description: Fix failing dependabot dependency bumps one at a time and report which were bumped, fixed or left out. Use when asked to fix, unblock or triage dependabot PRs that fail CI.
---

# Fix bumps

Each bump is a small experiment: apply it, see what breaks, fix it if the fix is small, and drop it if not. The owning area's `AGENTS.md` says how to change its dependencies and which gates to run; read it before touching that area.

1. List the open dependabot PRs and read each failing CI job's log. Note the first real error per PR, not the log's noise. Order the work by blast radius, smallest first.

2. Take one bump at a time, in this workspace, as its own change. Apply the same update the PR makes, following the area's dependency rules, then reproduce the failure locally with the area's gates. If the bump changes the toolchain or dev shell, run the gates inside the refreshed shell, not the old one.

3. Find the root cause before fixing. Check whether the new version, its peers or its plugins support each other before patching around them. Fix the code, test or config at its cause; never weaken a test or force a dependency resolution to get green.
   * A failure that appears only with other tests, or only in CI, is a signal to isolate it (run it alone, then with its neighbours) before changing anything.
   * A fix that several places need goes in the shared helper, not in each copy.

4. Stop when it is a rabbit hole: the fix is not small and local, the cause stays unknown after a handful of attempts, or it needs an upstream release. Revert the bump completely, restore the installed dependencies, record what was tried and what would unblock it, and move on. Never leave a half-applied bump behind.

5. Validate each kept bump with the area's full gates, and say which platforms or builds were not run here. Commit the bump and each fix as separate changes by area, with semantic messages.

6. Report per bump: bumped, fixed (what and why), or not bumped (cause and what would unblock it), plus anything unverified. Don't merge, push or close PRs unless asked.
