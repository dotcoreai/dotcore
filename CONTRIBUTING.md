# Contributing to Dotcore

Thanks for helping. Dotcore is in alpha and moves fast, so start a conversation before you write a large change.

## Start in Discussions

Issues are open to maintainers only. Everything starts in [Discussions](https://github.com/dotcoreai/dotcore/discussions):

- **Questions:** ask in Q&A.
- **Bugs:** open a Bug triage discussion. Maintainers turn confirmed bugs into issues.
- **Ideas and features:** post in Ideas first, before you write the code.

Security problems never go to Discussions. Follow [SECURITY.md](SECURITY.md) instead.

## Send a pull request

- Every pull request links the issue it fixes (`Fixes #123`). Issues come from a discussion a maintainer accepted, so start there.
- Small fixes (typos, broken links, docs) don't need an issue.
- Keep one change per pull request, with tests for the behavior it changes.
- Run `just check` before you push. It runs the repository-wide checks.
- Follow the commit format in the [root `AGENTS.md`](AGENTS.md#version-control).
- By sending a contribution, you agree to license it under [Apache-2.0](LICENSE), the project's license.

[Developing Dotcore](docs/development/README.md) explains how to set up the tools, run the stack and test.

## AI tools

You can use AI tools, but you own the result:

- Say in the pull request whether you used AI, and which tool.
- You must understand the change and be able to explain it in review without the tool.
- Discussions and pull request descriptions drafted with AI must be read and edited by you before posting.

Pull requests that look generated and unreviewed get closed.

## Code of conduct

Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).
