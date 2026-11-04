//! Source contract that every backend item has one public path: a `lib.rs` or `mod.rs` never
//! re-exports through `pub use` an item of a module it declares `pub mod`, and never glob
//! re-exports.

use std::{
    fs,
    path::{Path, PathBuf},
};

#[test]
fn backend_modules_expose_one_public_path_per_item() {
    let backend = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("backend directory");
    let mut violations = Vec::new();
    for entry in fs::read_dir(backend).expect("backend directory entries") {
        let src = entry.expect("backend entry").path().join("src");
        if src.is_dir() {
            for file in module_roots(&src) {
                let source = fs::read_to_string(&file).expect("module source");
                let label = file.strip_prefix(backend).expect("backend file");
                violations.extend(public_path_violations(&label.to_string_lossy(), &source));
            }
        }
    }

    assert_eq!(violations, Vec::<String>::new());
}

#[test]
fn a_re_export_of_a_public_module_item_or_a_glob_is_reported() {
    let source = "\
pub mod hardware;
mod local;
pub mod vram;

pub use hardware::{Collector, nested::Adapter};
pub use local::{LocalError, LocalNodeCapability};
pub use vram::*;
pub(crate) use hardware::Hidden;
";

    assert_eq!(
        public_path_violations("lib.rs", source),
        vec![
            "lib.rs: `pub use hardware::Collector` re-exports an item of `pub mod hardware`"
                .to_string(),
            "lib.rs: `pub use hardware::nested::Adapter` re-exports an item of `pub mod hardware`"
                .to_string(),
            "lib.rs: `pub use vram::*` re-exports an item of `pub mod vram`".to_string(),
            "lib.rs: `pub use vram::*` is a glob re-export".to_string(),
        ]
    );
}

/// Every `lib.rs` and `mod.rs` under `directory`.
fn module_roots(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(directory).expect("source directory entries") {
        let path = entry.expect("source entry").path();
        if path.is_dir() {
            found.extend(module_roots(&path));
        } else if matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some("lib.rs" | "mod.rs")
        ) {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// Describes each `pub use` in `source` that names a module the same file declares `pub mod`, or
/// that ends in a glob.
fn public_path_violations(label: &str, source: &str) -> Vec<String> {
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let public_modules: Vec<&str> = code
        .split(';')
        .filter_map(|statement| {
            let statement = statement.trim();
            statement
                .rsplit_once("pub mod ")
                .filter(|(before, _)| before.trim_end().is_empty() || before.ends_with(['\n', ']']))
                .map(|(_, name)| name.trim())
        })
        .collect();

    let mut violations = Vec::new();
    for statement in code.split(';') {
        let Some((_, tree)) = statement.rsplit_once("pub use ") else {
            continue;
        };
        for path in flatten_use_tree(tree) {
            let rendered = path.join("::");
            if let Some(module) = public_modules.iter().find(|module| **module == path[0]) {
                violations.push(format!(
                    "{label}: `pub use {rendered}` re-exports an item of `pub mod {module}`"
                ));
            }
            if path.last().is_some_and(|segment| segment == "*") {
                violations.push(format!("{label}: `pub use {rendered}` is a glob re-export"));
            }
        }
    }
    violations
}

/// Expands a `use` tree such as `a::{b, c::d}` into its leaf paths `[a, b]` and `[a, c, d]`.
fn flatten_use_tree(tree: &str) -> Vec<Vec<String>> {
    let tokens: Vec<String> = tree
        .replace("::", " :: ")
        .replace('{', " { ")
        .replace('}', " } ")
        .replace(',', " , ")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let mut position = 0;
    parse_item(&tokens, &mut position)
}

fn parse_item(tokens: &[String], position: &mut usize) -> Vec<Vec<String>> {
    let mut prefix = Vec::new();
    while let Some(token) = tokens.get(*position) {
        match token.as_str() {
            "::" => *position += 1,
            "{" => {
                *position += 1;
                let mut leaves = Vec::new();
                while tokens.get(*position).is_some_and(|token| token != "}") {
                    leaves.extend(parse_item(tokens, position));
                    if tokens.get(*position).is_some_and(|token| token == ",") {
                        *position += 1;
                    }
                }
                *position += 1;
                return leaves
                    .into_iter()
                    .map(|leaf| prefix.iter().cloned().chain(leaf).collect())
                    .collect();
            }
            "," | "}" => break,
            _ => {
                prefix.push(token.clone());
                *position += 1;
            }
        }
    }
    vec![prefix]
}
