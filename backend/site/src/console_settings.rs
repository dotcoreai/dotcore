//! Site-wide console settings: operator choices about the console kept with the desired site.
//!
//! The site manager keeps one set of console settings for the whole site, so every browser that
//! opens the console sees the same choices. Updates name the fields they write and the last writer
//! wins. The settings drive no controller and never reach a node.

use std::collections::BTreeSet;

use dotcore_proto::{dotcore::site::v1::ConsoleSettings, google::protobuf::FieldMask};

use crate::repository::{ScopedSiteMutationError, SiteRepository, SiteRepositoryError};

const GET_STARTED_HIDDEN_PATH: &str = "get_started_hidden";

/// Reads and writes the site-wide console settings.
pub trait ConsoleSettingsStore: Send + Sync {
    /// Reads the current settings, with defaults for values never written.
    fn console_settings(&self) -> Result<ConsoleSettings, SiteRepositoryError>;

    /// Applies one patch and returns the complete settings it produced.
    fn update_console_settings(
        &self,
        patch: &ConsoleSettingsPatch,
    ) -> Result<ConsoleSettings, SiteRepositoryError>;
}

impl ConsoleSettingsStore for SiteRepository {
    fn console_settings(&self) -> Result<ConsoleSettings, SiteRepositoryError> {
        Ok(self.snapshot()?.console_settings.unwrap_or_default())
    }

    fn update_console_settings(
        &self,
        patch: &ConsoleSettingsPatch,
    ) -> Result<ConsoleSettings, SiteRepositoryError> {
        self.mutate_console_settings(|settings| {
            patch.apply(settings);
            Ok::<_, std::convert::Infallible>(*settings)
        })
        .map_err(|error| match error {
            ScopedSiteMutationError::Repository(error) => error,
        })
    }
}

/// A validated update of the console settings fields an update mask selects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleSettingsPatch {
    get_started_hidden: Option<bool>,
}

impl ConsoleSettingsPatch {
    /// Parses the masked fields of `settings`; every mask path must name a writable field once.
    pub fn parse(
        settings: Option<&ConsoleSettings>,
        update_mask: Option<&FieldMask>,
    ) -> Result<Self, ConsoleSettingsPatchError> {
        let settings = settings.ok_or(ConsoleSettingsPatchError::MissingSettings)?;
        let update_mask = update_mask.ok_or(ConsoleSettingsPatchError::MissingUpdateMask)?;
        if update_mask.paths.is_empty() {
            return Err(ConsoleSettingsPatchError::EmptyUpdateMask);
        }

        let mut paths = BTreeSet::new();
        let mut patch = Self {
            get_started_hidden: None,
        };
        for path in &update_mask.paths {
            if !paths.insert(path.as_str()) {
                return Err(ConsoleSettingsPatchError::DuplicatePath(path.clone()));
            }
            match path.as_str() {
                GET_STARTED_HIDDEN_PATH => {
                    patch.get_started_hidden = Some(settings.get_started_hidden)
                }
                _ => return Err(ConsoleSettingsPatchError::UnwritablePath(path.clone())),
            }
        }
        Ok(patch)
    }

    /// Writes the masked fields into `settings`, leaving the others unchanged.
    pub fn apply(&self, settings: &mut ConsoleSettings) {
        if let Some(hidden) = self.get_started_hidden {
            settings.get_started_hidden = hidden;
        }
    }
}

/// Rejects a console settings update before it changes the site.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConsoleSettingsPatchError {
    #[error("console settings are required")]
    MissingSettings,

    #[error("console settings update_mask is required")]
    MissingUpdateMask,

    #[error("console settings update_mask must not be empty")]
    EmptyUpdateMask,

    #[error("console settings update_mask repeats path {0:?}")]
    DuplicatePath(String),

    #[error("console settings update_mask path {0:?} is not writable")]
    UnwritablePath(String),
}
