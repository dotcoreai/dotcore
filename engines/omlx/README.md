# oMLX engine

[oMLX](https://github.com/jundot/omlx), an MLX-based engine for Apple Silicon Macs. Chat models
only.

## Good to know

* It needs macOS 26 or newer on an Apple Silicon Mac. Models run directly on the GPU, outside
  Docker.
* It manages its own memory, so it can use more than the estimate. Dotcore gives it a memory
  ceiling it respects, and it refuses prompts that would cross it.

Maintaining it: [`AGENTS.md`](AGENTS.md).
