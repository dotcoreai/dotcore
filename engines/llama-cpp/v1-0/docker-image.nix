# llama.cpp v1.0.0's CUDA 13 server image, pinned by digest and kept for Clef Flash only.
# CHECK(https://github.com/ggml-org/llama.cpp/pull/32754): a llama.cpp release sizes clef's decision head
#   by its spans again, not the micro-batch maximum -> move Clef Flash to the current llama.cpp releases
#   and delete this folder and its catalog releases.
{ }:

{
  registry.lock = ./docker-image.lock.json;
}
