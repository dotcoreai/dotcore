//! NVIDIA CDI selector discovery through nvidia-ctk and standard specification files.

use std::{collections::BTreeSet, future::Future, io::ErrorKind, path::Path};

use anyhow::Context;
use serde::Deserialize;
use tokio::process::Command;

const STANDARD_CDI_SPEC_DIRS: [&str; 2] = ["/etc/cdi", "/var/run/cdi"];

/// Returns NVIDIA CDI selectors using nvidia-ctk before standard-file fallback.
pub(super) async fn discover_selectors() -> anyhow::Result<BTreeSet<String>> {
    selectors_or_fallback(nvidia_ctk_selectors().await?, || async {
        selectors_from_directories(STANDARD_CDI_SPEC_DIRS).await
    })
    .await
}

#[derive(Deserialize)]
struct CdiSpecification {
    kind: String,
    #[serde(default)]
    devices: Vec<CdiDevice>,
}

#[derive(Deserialize)]
struct CdiDevice {
    name: String,
}

async fn nvidia_ctk_selectors() -> anyhow::Result<Option<BTreeSet<String>>> {
    match Command::new("nvidia-ctk")
        .args(["cdi", "list"])
        .kill_on_drop(true)
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            Ok(Some(selectors_from_nvidia_ctk(&output.stdout)))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::Error::new(error).context("run nvidia-ctk cdi list")),
    }
}

async fn selectors_from_directories(
    directories: impl IntoIterator<Item = impl AsRef<Path>>,
) -> anyhow::Result<BTreeSet<String>> {
    let mut selectors = BTreeSet::new();
    for directory in directories {
        let directory = directory.as_ref();
        let mut entries = match tokio::fs::read_dir(directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "read CDI specification directory {}",
                    directory.display()
                )));
            }
        };
        while let Some(entry) = entries.next_entry().await.map_err(|error| {
            anyhow::Error::new(error).context(format!("read CDI entry in {}", directory.display()))
        })? {
            let path = entry.path();
            if is_cdi_specification(&path) {
                selectors.extend(selectors_from_cdi_specification(&path).await?);
            }
        }
    }
    Ok(selectors)
}

async fn selectors_or_fallback<F, O>(
    selectors: Option<BTreeSet<String>>,
    fallback: F,
) -> anyhow::Result<BTreeSet<String>>
where
    F: FnOnce() -> O,
    O: Future<Output = anyhow::Result<BTreeSet<String>>>,
{
    match selectors {
        Some(selectors) => Ok(selectors),
        None => fallback().await,
    }
}

fn selectors_from_nvidia_ctk(output: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .filter(|value| is_nvidia_selector(value))
        .map(str::to_owned)
        .collect()
}

async fn selectors_from_cdi_specification(path: &Path) -> anyhow::Result<BTreeSet<String>> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read CDI specification {}", path.display()))?;
    selectors_from_cdi_contents(&contents)
        .with_context(|| format!("parse CDI specification {}", path.display()))
}

fn selectors_from_cdi_contents(contents: &str) -> anyhow::Result<BTreeSet<String>> {
    let specification: CdiSpecification = serde_yaml_ng::from_str(contents)?;
    if !specification.kind.starts_with("nvidia.com/") {
        return Ok(BTreeSet::new());
    }
    Ok(specification
        .devices
        .into_iter()
        .map(|device| format!("{}={}", specification.kind, device.name))
        .filter(|selector| is_nvidia_selector(selector))
        .collect())
}

fn is_cdi_specification(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("json" | "yaml" | "yml")
    )
}

fn is_nvidia_selector(value: &str) -> bool {
    value
        .split_once('=')
        .is_some_and(|(kind, name)| kind.starts_with("nvidia.com/") && !name.is_empty())
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::BTreeSet};

    use super::{selectors_from_cdi_contents, selectors_from_nvidia_ctk, selectors_or_fallback};

    #[test]
    fn nvidia_ctk_output_keeps_only_valid_nvidia_selectors_in_lexical_order() {
        let selectors = selectors_from_nvidia_ctk(
            b"nvidia.com/gpu=gpu1\ninvalid\nvendor.example/device=x\nnvidia.com/gpu=gpu0\n",
        );

        assert_eq!(
            selectors.into_iter().collect::<Vec<_>>(),
            ["nvidia.com/gpu=gpu0", "nvidia.com/gpu=gpu1"]
        );
    }

    #[tokio::test]
    async fn file_fallback_runs_once_only_when_nvidia_ctk_has_no_result() {
        let calls = Cell::new(0);
        let fallback = || {
            calls.set(calls.get() + 1);
            async { Ok(BTreeSet::from(["nvidia.com/gpu=gpu0".to_owned()])) }
        };

        let preferred = selectors_or_fallback(
            Some(BTreeSet::from(["nvidia.com/gpu=all".to_owned()])),
            fallback,
        )
        .await
        .expect("preferred selectors");
        assert_eq!(preferred, BTreeSet::from(["nvidia.com/gpu=all".to_owned()]));
        assert_eq!(calls.get(), 0);

        let fallback = selectors_or_fallback(None, fallback)
            .await
            .expect("fallback selectors");
        assert_eq!(fallback, BTreeSet::from(["nvidia.com/gpu=gpu0".to_owned()]));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn cdi_yaml_and_json_inventory_produce_full_nvidia_selectors() {
        let yaml = selectors_from_cdi_contents(
            "kind: nvidia.com/gpu\ndevices:\n  - name: gpu1\n  - name: gpu0\n",
        )
        .expect("YAML CDI specification");
        let json =
            selectors_from_cdi_contents(r#"{"kind":"nvidia.com/gpu","devices":[{"name":"all"}]}"#)
                .expect("JSON CDI specification");

        assert_eq!(
            yaml.into_iter().collect::<Vec<_>>(),
            ["nvidia.com/gpu=gpu0", "nvidia.com/gpu=gpu1"]
        );
        assert_eq!(json.into_iter().collect::<Vec<_>>(), ["nvidia.com/gpu=all"]);
    }
}
