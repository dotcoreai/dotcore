//! Reads the memory figures vLLM prints while starting, for example
//! `GPU KV cache size: 172,480 tokens` or `Model loading took 21.34 GiB`,
//! into typed facts for workload resource telemetry.
//!
//! Only fixed marker text is matched, so unrelated or reworded lines leave
//! the matching fact unset instead of failing. When a line repeats, the last
//! occurrence wins. GiB figures are printed with two decimals, so converted
//! byte values are accurate to about 5 MiB.

use dotcore_proto::dotcore::telemetry::v1::{
    VllmStartupFacts, workload_resource_observation::EngineFacts,
};

use crate::model::engine::startup_facts::{ExtractedStartupFacts, StartupFactExtractor};

/// Version of this extractor, recorded with every fact set it produces.
pub(crate) const EXTRACTOR_VERSION: u32 = 1;

const BYTES_PER_GIB: f64 = (1_u64 << 30) as f64;

/// Extracts vLLM startup memory facts from its engine output.
pub(crate) struct VllmStartupFactExtractor;

impl StartupFactExtractor for VllmStartupFactExtractor {
    fn extract(&self, output: &str) -> ExtractedStartupFacts {
        let facts = VllmStartupFacts {
            extractor_version: EXTRACTOR_VERSION,
            kv_cache_token_capacity: last_value(output, "GPU KV cache size: ", " tokens")
                .and_then(tokens),
            kv_cache_reserved_bytes: last_value(output, ", reserved ", " GiB memory for KV Cache")
                .or_else(|| last_value(output, "Available KV cache memory: ", " GiB"))
                .and_then(gib_bytes),
            model_loading_bytes: last_value(output, "Model loading took ", " GiB")
                .and_then(gib_bytes),
            cuda_graph_bytes: last_value(output, "Graph capturing finished in ", " GiB")
                .and_then(|value| value.rsplit(", took ").next())
                .and_then(gib_bytes),
            startup_cuda_free_bytes: last_value(output, "Initial free memory ", " GiB")
                .and_then(gib_bytes),
        };
        let missing = [
            ("kv_cache_token_capacity", facts.kv_cache_token_capacity),
            ("kv_cache_reserved_bytes", facts.kv_cache_reserved_bytes),
            ("model_loading_bytes", facts.model_loading_bytes),
            ("cuda_graph_bytes", facts.cuda_graph_bytes),
            ("startup_cuda_free_bytes", facts.startup_cuda_free_bytes),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.is_none().then_some(name))
        .collect();
        ExtractedStartupFacts {
            facts: EngineFacts::Vllm(facts),
            missing,
        }
    }
}

/// Returns the text between the last `prefix` and the following `suffix` on the same line.
fn last_value<'a>(output: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    output.lines().rev().find_map(|line| {
        let start = line.rfind(prefix)? + prefix.len();
        let rest = &line[start..];
        rest.find(suffix).map(|end| rest[..end].trim())
    })
}

fn tokens(value: &str) -> Option<u64> {
    value.replace(',', "").parse().ok()
}

fn gib_bytes(value: &str) -> Option<u64> {
    let gib = value.trim().parse::<f64>().ok()?;
    (gib.is_finite() && gib >= 0.0).then(|| (gib * BYTES_PER_GIB).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Startup lines cut from a real vLLM 0.1.dev19754 launch on an RTX 5090 (point V2).
    const V2_STARTUP: &str = "\
(EngineCore pid=364) INFO 09-25 16:03:41 [gpu_model_runner.py:5483] Model loading took 21.34 GiB memory and 12.040773 seconds
(EngineCore pid=364) INFO 09-25 16:05:36 [gpu_worker.py:492] Initial free memory 30.86 GiB, reserved 5.59 GiB memory for KV Cache as specified by kv_cache_memory_bytes config and skipped memory profiling. This does not respect the gpu_memory_utilization config.
(EngineCore pid=364) INFO 09-25 16:05:36 [kv_cache_utils.py:2044] Auto-fit max_model_len: reduced from 262144 to 172480 to fit in available GPU memory (5.59 GiB available for KV cache)
(EngineCore pid=364) INFO 09-25 16:05:36 [kv_cache_utils.py:1882] GPU KV cache size: 172,480 tokens, Maximum concurrency for 172,480 tokens per request: 1.00x
(EngineCore pid=364) INFO 09-25 16:05:48 [gpu_model_runner.py:6988] Graph capturing finished in 3 secs, took 0.56 GiB
(APIServer pid=1) INFO:     Application startup complete.";

    fn gib(value: f64) -> u64 {
        (value * BYTES_PER_GIB).round() as u64
    }

    fn extract(output: &str) -> ExtractedStartupFacts {
        VllmStartupFactExtractor.extract(output)
    }

    #[test]
    fn extracts_every_fact_from_real_startup_lines() {
        assert_eq!(
            extract(V2_STARTUP),
            ExtractedStartupFacts {
                facts: EngineFacts::Vllm(VllmStartupFacts {
                    extractor_version: EXTRACTOR_VERSION,
                    kv_cache_token_capacity: Some(172_480),
                    kv_cache_reserved_bytes: Some(gib(5.59)),
                    model_loading_bytes: Some(gib(21.34)),
                    cuda_graph_bytes: Some(gib(0.56)),
                    startup_cuda_free_bytes: Some(gib(30.86)),
                }),
                missing: Vec::new(),
            }
        );
    }

    #[test]
    fn truncated_output_yields_partial_facts() {
        let truncated = V2_STARTUP.lines().take(2).collect::<Vec<_>>().join("\n");

        assert_eq!(
            extract(&truncated),
            ExtractedStartupFacts {
                facts: EngineFacts::Vllm(VllmStartupFacts {
                    extractor_version: EXTRACTOR_VERSION,
                    kv_cache_reserved_bytes: Some(gib(5.59)),
                    model_loading_bytes: Some(gib(21.34)),
                    startup_cuda_free_bytes: Some(gib(30.86)),
                    ..VllmStartupFacts::default()
                }),
                missing: vec!["kv_cache_token_capacity", "cuda_graph_bytes"],
            }
        );
    }

    #[test]
    fn profiled_launch_reports_available_kv_memory() {
        let output =
            "(EngineCore pid=9) INFO [gpu_worker.py:400] Available KV cache memory: 12.25 GiB";

        let EngineFacts::Vllm(facts) = extract(output).facts;

        assert_eq!(facts.kv_cache_reserved_bytes, Some(gib(12.25)));
    }

    #[test]
    fn last_occurrence_wins_and_malformed_values_are_ignored() {
        let output = "GPU KV cache size: 1,000 tokens\nGPU KV cache size: 2,000 tokens\nModel loading took lots GiB memory";

        let EngineFacts::Vllm(facts) = extract(output).facts;

        assert_eq!(facts.kv_cache_token_capacity, Some(2_000));
        assert_eq!(facts.model_loading_bytes, None);
    }
}
