//! Removal of one site's node-local workload caches, which live below the cache root rather than
//! the site folder.
//!
//! Workloads run as root, so their cache files may be undeletable by this process; the pinned
//! no-network container that empties root-owned site data then clears the folder first.

use anyhow::Context;
use dotcore_domain::CacheLayout;

use super::site_data::remove_folder_tree;

/// Removes the workload cache folders of `site_id` below `cache_root`, in the current and the
/// earlier layout, and no other site's folders. Absent folders are already removed.
pub async fn remove_site_workload_caches(
    cache_root: &std::path::Path,
    site_id: &str,
) -> anyhow::Result<()> {
    if !dotcore_domain::valid_id(site_id) {
        anyhow::bail!("workload cache removal rejected an invalid site id");
    }
    for folder in CacheLayout(cache_root).site_workload_caches(site_id) {
        remove_folder_tree(&folder)
            .await
            .with_context(|| format!("remove workload cache {}", folder.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[tokio::test]
    async fn removes_both_layouts_of_the_site_and_keeps_every_other_site() {
        let directory = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("cache root");
        let root = directory.path();
        let layout = CacheLayout(root);
        let mut kept = Vec::new();
        for site in ["site-one", "site-two"] {
            for folder in layout.site_workload_caches(site) {
                let file = folder.join("engine-artifact/key/engine-cache/entry");
                std::fs::create_dir_all(file.parent().expect("parent")).expect("cache folder");
                std::fs::write(&file, site).expect("cache file");
                if site == "site-two" {
                    kept.push(file);
                }
            }
        }
        let model_artifacts = layout.model_artifacts().join("blob");
        std::fs::create_dir_all(model_artifacts.parent().expect("parent")).expect("artifacts");
        std::fs::write(&model_artifacts, "model").expect("artifact file");

        remove_site_workload_caches(root, "site-one")
            .await
            .expect("removal");

        assert_eq!(
            tree(root),
            [
                "model-artifacts",
                "model-artifacts/v1",
                "model-artifacts/v1/blob",
                "workload-cache",
                "workload-cache/v1",
                "workload-cache/v1/site-two",
                "workload-cache/v1/site-two/engine-artifact",
                "workload-cache/v1/site-two/engine-artifact/key",
                "workload-cache/v1/site-two/engine-artifact/key/engine-cache",
                "workload-cache/v1/site-two/engine-artifact/key/engine-cache/entry",
                "workload-cache/v2",
                "workload-cache/v2/site-two",
                "workload-cache/v2/site-two/engine-artifact",
                "workload-cache/v2/site-two/engine-artifact/key",
                "workload-cache/v2/site-two/engine-artifact/key/engine-cache",
                "workload-cache/v2/site-two/engine-artifact/key/engine-cache/entry",
            ]
        );
        for file in kept {
            assert_eq!(std::fs::read_to_string(file).expect("kept"), "site-two");
        }

        remove_site_workload_caches(root, "site-one")
            .await
            .expect("absent caches are already removed");
    }

    #[tokio::test]
    async fn rejects_an_invalid_site_id_before_touching_anything() {
        let directory = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("cache root");
        let sibling = directory.path().join("workload-cache/v2/other");
        std::fs::create_dir_all(&sibling).expect("sibling");

        let error = remove_site_workload_caches(directory.path(), "../v2/other")
            .await
            .expect_err("invalid id");

        assert_eq!(
            error.to_string(),
            "workload cache removal rejected an invalid site id"
        );
        assert!(sibling.is_dir());
    }

    /// Sorted relative paths of every entry below `root`.
    fn tree(root: &Path) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("read dir") {
                let path = entry.expect("entry").path();
                out.push(
                    path.strip_prefix(root)
                        .expect("below root")
                        .to_string_lossy()
                        .into_owned(),
                );
                if path.is_dir() {
                    walk(root, &path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }
}
