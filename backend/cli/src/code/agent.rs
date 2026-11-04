//! The coding agents that can be started on the site's inference API, and how each one is
//! launched with a route as its active model.
//!
//! A launch is a plain value (arguments, extra environment and files kept under the cache folder)
//! built from the base URL, the listed routes and the selected route, so it can be checked
//! without starting anything. Every agent registers all listed routes under one provider, uses
//! Chat Completions without an API key, and leaves the user's own agent configuration untouched.

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use clap::ValueEnum;

use super::{opencode, pi};

/// A coding agent that can be started on the site's inference API.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum CodingAgent {
    /// OpenCode (https://opencode.ai).
    Opencode,

    /// Pi (https://github.com/earendil-works/pi).
    Pi,
}

impl CodingAgent {
    /// The agent's name, as the operator types it, which is also its executable's name.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Pi => "pi",
        }
    }

    /// The agent's product name, for text read by the operator.
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::Opencode => "OpenCode",
            Self::Pi => "Pi",
        }
    }

    /// Where or how to install the agent: a web page or a command to copy.
    pub(crate) fn install_hint(self) -> &'static str {
        match self {
            Self::Opencode => "https://opencode.ai",
            Self::Pi => "npm install -g @earendil-works/pi-coding-agent",
        }
    }

    /// Builds the launch that points the agent at `setup`'s route.
    pub(super) fn launch(self, setup: &LaunchSetup<'_>) -> anyhow::Result<AgentLaunch> {
        match self {
            Self::Opencode => opencode::launch(setup),
            Self::Pi => pi::launch(setup),
        }
    }
}

/// What every agent launch is built from.
pub(super) struct LaunchSetup<'a> {
    /// The OpenAI-compatible base URL of the inference API.
    pub(super) base_url: String,

    /// Every route name the inference API lists, in its order.
    pub(super) routes: Vec<&'a str>,

    /// The route the agent starts with; one of `routes`.
    pub(super) selected: &'a str,

    /// Arguments the operator passed for the agent, kept unchanged.
    pub(super) passthrough: &'a [OsString],

    /// Resolves the cache root that owned files live under; only agents with owned files call it.
    pub(super) cache_root: &'a dyn Fn() -> anyhow::Result<PathBuf>,
}

/// How to start one agent's executable.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct AgentLaunch {
    /// Arguments of the executable, in order.
    pub(super) args: Vec<OsString>,

    /// Variables added to the inherited environment.
    pub(super) env: Vec<(&'static str, String)>,

    /// Files that must exist before the agent starts.
    pub(super) files: Vec<OwnedFile>,
}

/// A file kept under the cache root whose name carries its content's hash, so an existing file
/// always has the right content and concurrent launches never rewrite it.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct OwnedFile {
    /// Where the file lives under the cache root.
    pub(super) path: PathBuf,

    /// The file's fixed content.
    pub(super) content: &'static str,
}

impl OwnedFile {
    /// Writes the file when it is absent, as a whole so a concurrent launch never reads a partial
    /// file.
    pub(super) fn ensure(&self) -> io::Result<()> {
        if self.path.exists() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let temporary = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        fs::write(&temporary, self.content)
            .and_then(|()| fs::rename(&temporary, &self.path))
            .inspect_err(|_| {
                let _ = fs::remove_file(&temporary);
            })
    }
}

/// The FNV-1a hash of `content`, stable across builds and platforms, for naming a file after its
/// content.
const fn content_hash(content: &str) -> u64 {
    let bytes = content.as_bytes();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }
    hash
}

/// The path of a content-named file: `<folder>/<stem>-<hash>.<extension>`.
pub(super) fn content_named(folder: &Path, stem: &str, extension: &str, content: &str) -> PathBuf {
    folder.join(format!("{stem}-{:016x}.{extension}", content_hash(content)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owned_file_is_written_when_absent_and_kept_when_present() {
        let root = tempfile::tempdir().expect("cache root");
        let file = OwnedFile {
            path: root.path().join("code/pi/extension.ts"),
            content: "export default 1;\n",
        };

        file.ensure().expect("write absent file");
        assert_eq!(
            fs::read_to_string(&file.path).expect("written"),
            file.content
        );

        fs::write(&file.path, "kept").expect("change it");
        file.ensure().expect("keep present file");
        assert_eq!(fs::read_to_string(&file.path).expect("kept"), "kept");
        assert_eq!(
            fs::read_dir(file.path.parent().expect("folder"))
                .expect("list folder")
                .count(),
            1,
            "no temporary file is left"
        );
    }

    #[test]
    fn content_named_files_change_name_with_their_content() {
        let folder = Path::new("/cache/code/pi");

        let first = content_named(folder, "provider", "ts", "a");
        let second = content_named(folder, "provider", "ts", "b");

        assert_eq!(
            first,
            Path::new("/cache/code/pi/provider-af63dc4c8601ec8c.ts")
        );
        assert_ne!(first, second);
        assert_eq!(first, content_named(folder, "provider", "ts", "a"));
    }
}
