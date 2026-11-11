//! Responses function tools that a client groups under a namespace, such as one MCP server's tools.
//!
//! Engines only know flat function names, so each namespaced tool reaches the model under one
//! flat name joining its namespace and its own name. The names of one request are kept here so
//! the function calls the model makes go back to the client under their namespace again.

use std::collections::BTreeMap;

/// The namespaced function tools of one Responses request, keyed by the flat name the model sees.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToolNamespaces {
    tools: BTreeMap<String, NamespacedTool>,
}

impl ToolNamespaces {
    /// Records a namespaced tool and returns its flat name, or `None` when that name is taken.
    pub(super) fn insert(&mut self, namespace: &str, name: &str) -> Option<String> {
        let flat_name = flat_name(namespace, name);
        if self.tools.contains_key(&flat_name) {
            return None;
        }
        self.tools.insert(
            flat_name.clone(),
            NamespacedTool {
                namespace: namespace.to_owned(),
                name: name.to_owned(),
            },
        );
        Some(flat_name)
    }

    /// Returns whether a flat name belongs to a namespaced tool of this request.
    pub(super) fn contains(&self, flat_name: &str) -> bool {
        self.tools.contains_key(flat_name)
    }

    /// Returns the namespaced tool the model called by its flat name, if it is one of this request.
    pub(super) fn resolve(&self, flat_name: &str) -> Option<&NamespacedTool> {
        self.tools.get(flat_name)
    }
}

/// A function tool's own name within the namespace a client declared it in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NamespacedTool {
    pub(super) namespace: String,
    pub(super) name: String,
}

/// Returns the flat name the model sees for a tool of a namespace.
pub(super) fn flat_name(namespace: &str, name: &str) -> String {
    format!("{namespace}__{name}")
}
