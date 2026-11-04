//! Build script that compiles the hardware simulated inference offers into the crate.
//!
//! Every YAML file of the `simulated/machines` folder becomes one named machine, and every one of
//! `simulated/gpus` one named GPU, named after its file, so adding either is adding a file. The
//! descriptions are embedded as text and parsed when they are looked up.

use std::{env, fs, path::Path};

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = Path::new(&env::var("OUT_DIR").expect("cargo sets OUT_DIR")).to_owned();
    let simulated = manifest.join("simulated");
    embed(&simulated.join("machines"), &out.join("named_machines.rs"));
    embed(&simulated.join("gpus"), &out.join("named_gpus.rs"));
}

/// Writes a table of `(name, include_str!(path))` entries for the YAML files of `folder`.
fn embed(folder: &Path, out: &Path) {
    // A directory path reruns the script when any file under it changes.
    println!("cargo:rerun-if-changed={}", folder.display());

    let mut files: Vec<_> = fs::read_dir(folder)
        .expect("read a description folder")
        .map(|entry| entry.expect("read a description entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        })
        .collect();
    files.sort();

    let entries: String = files
        .iter()
        .map(|path| {
            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("description file names are UTF-8");
            format!("    ({name:?}, include_str!({:?})),\n", path.display())
        })
        .collect();
    fs::write(out, format!("&[\n{entries}]\n")).expect("write a description table");
}
