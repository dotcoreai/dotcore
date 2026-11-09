# Engines

The inference engines Dotcore runs models with. Each model in the catalog lists the engines it can
use, and you pick one when you deploy it.

| Engine                  | Runs on                                  | Good to know                                       |
| ----------------------- | ---------------------------------------- | -------------------------------------------------- |
| [llama.cpp](llama-cpp/) | NVIDIA GPUs on Linux, Apple Silicon Macs | The only engine on a Mac                           |
| [vLLM](vllm/)           | NVIDIA GPUs on Linux                     | Shares prompt prefixes between requests            |
| [ninfer](ninfer/)       | RTX 5090, RTX PRO 6000 on x86_64 Linux   | Shares prompt prefixes; first deployment builds it |

## Shared settings

Every engine takes the same three settings, which you pick when you deploy a model:

* **Total context** (`total-context`): tokens of KV cache the engine allocates at start, shared by
  all active requests.
* **Context per request** (`context-per-request`): the most tokens one request may use. When you
  leave it empty, it is the smaller of the model's maximum context and the total context.
* **Parallel requests** (`parallel-requests`): how many requests the engine serves at once. More
  wait in a queue.

Memory estimates assume requests share no prompt prefix. Engines that share prefixes (vLLM, ninfer)
can fit more than the estimate.

Maintaining the engines: [`AGENTS.md`](AGENTS.md).
