# Testing

## Running tests

- `just test` runs every test suite, and `just check` runs them with formatting and lint. Run `just check` before sending a change.
- Each area has its own recipes, such as `just backend::test` and `just frontend::test`. `just --list <area>` shows them.
- `just backend::test` takes nextest filters to run some tests only, for example `just backend::test -E 'test(/^server::/)'`.
- `just backend::test-diagnostics` runs the backend tests without retries and writes a JUnit report, to investigate flaky tests.

## CI

- CI runs the same gates as `just check`, but it fails instead of reformatting or regenerating files. Each area's checks run in their own job, and the backend and frontend tests run in two parts. `just ci::check` runs them all locally, to reproduce a CI failure.
- Pull requests and `main` run the backend and development checks on x86_64 Linux, arm64 Linux and arm64 macOS. The protobuf, frontend and installer checks don't depend on the platform, so they run on x86_64 Linux only.
- CI skips the hardware tests below.

## Hardware tests

Some backend tests need real hardware. `just test` and `just check` skip them. Each kind has its own recipe:

- `just backend::test-docker` needs Docker running natively on the machine's architecture.
- `just backend::test-nvidia` needs Linux with an NVIDIA GPU and the NVIDIA Container Toolkit.
- `just backend::test-nvidia-5090` needs an RTX 5090 that nothing else uses.
- `just backend::test-apple` needs a Mac with Apple Silicon.

Extra arguments narrow the selection, such as `just backend::test-nvidia vllm`.

On a Mac, run `just dev::mac-loopback` first. It adds the loopback addresses some tests need, until the next reboot.

To run them on hardware you don't have locally, use a [remote host](remote-hosts.md).
