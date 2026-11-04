//! Source checks shared by the unit tests of the runtime contracts and backends, compiled only for
//! tests and the `test-support` feature.
//!
//! They keep runtime owner-neutral: a runtime source must not name an engine, a model owner, an
//! engine protocol path, controller dispatch, or site lifecycle fencing.

/// Fails when the production part of a runtime source names an engine, model owner, engine
/// protocol path, controller dispatch, or site lifecycle fencing.
pub fn assert_owner_neutral(name: &str, source: &str) {
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("production source");
    for forbidden in [
        ["v", "LLM"].concat(),
        ["lla", "ma"].concat(),
        ["nin", "fer"].concat(),
        ["hug", "ging"].concat(),
        ["gg", "uf"].concat(),
        ["Catalog", "Model"].concat(),
        ["Model", "Deployment"].concat(),
        ["/he", "alth"].concat(),
        ["/v1/", "models"].concat(),
        ["crate::", "dispatch"].concat(),
        ["RuntimeAction", "Authority"].concat(),
    ] {
        assert!(
            !production.contains(&forbidden),
            "{name} contains {forbidden}"
        );
    }
}
