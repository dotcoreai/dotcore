//! Holds one long-running host process per workload directory, outside the lifetime of the
//! process that created it, plus the client that creates and later re-attaches to those workloads.
//!
//! The `dotcore-launcher` binary detaches into its own session, runs the declared executable with
//! only the declared environment, records timestamped output and the final exit status in the
//! workload directory, and forwards stop requests. The crate is owner-neutral: it knows processes,
//! files, and signals, never what the held process does. It reads raw host process facts through
//! the host process crate, so it records the same identity for a process as the rest of the
//! backend.
//!
//! The binary is synchronous and single-threaded, which keeps its fork sequence safe; only the
//! supervisor, used by the creating process, is async.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Dotcore supports only Linux and macOS targets");

#[doc(hidden)]
pub mod launcher;
pub mod supervisor;
pub mod workload;

#[cfg(test)]
mod tests {
    #[test]
    fn launcher_binary_does_not_use_the_async_supervisor() {
        for (name, source) in [
            ("main.rs", include_str!("main.rs")),
            ("launcher/mod.rs", include_str!("launcher/mod.rs")),
            ("launcher/log.rs", include_str!("launcher/log.rs")),
        ] {
            for forbidden in ["::supervisor", "tokio"] {
                assert!(!source.contains(forbidden), "{name} uses {forbidden}");
            }
        }
    }

    /// Every authored source of the crate.
    const SOURCES: [(&str, &str); 7] = [
        ("lib.rs", include_str!("lib.rs")),
        ("main.rs", include_str!("main.rs")),
        ("workload.rs", include_str!("workload.rs")),
        ("launcher/mod.rs", include_str!("launcher/mod.rs")),
        ("launcher/log.rs", include_str!("launcher/log.rs")),
        ("supervisor/mod.rs", include_str!("supervisor/mod.rs")),
        ("supervisor/logs.rs", include_str!("supervisor/logs.rs")),
    ];

    #[test]
    fn sources_import_only_the_launcher_protocol_package() {
        let crate_name = ["dotcore", "_proto"].concat();
        for (name, source) in SOURCES {
            for (offset, _) in source.match_indices(&crate_name) {
                assert!(
                    source[offset + crate_name.len()..].starts_with("::dotcore::launcher::"),
                    "{name} imports a {crate_name} package other than launcher"
                );
            }
        }
    }

    #[test]
    fn sources_are_owner_neutral() {
        let contract = (
            "launcher/store/v1/workload.proto",
            include_str!("../../../proto/dotcore/launcher/store/v1/workload.proto"),
        );
        for (name, source) in SOURCES.into_iter().chain([contract]) {
            let production = source
                .split("#[cfg(test)]")
                .next()
                .expect("production source")
                .to_lowercase();
            for forbidden in [
                ["eng", "ine"].concat(),
                ["mod", "el"].concat(),
                ["infer", "ence"].concat(),
                ["ser", "ver"].concat(),
                ["ll", "m"].concat(),
                ["lla", "ma"].concat(),
                ["gg", "uf"].concat(),
                ["hug", "ging"].concat(),
            ] {
                assert!(
                    !production.contains(&forbidden),
                    "{name} contains {forbidden}"
                );
            }
        }
    }
}
