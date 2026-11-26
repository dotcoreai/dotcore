//! Lists the catalog models the running site can deploy, with whether each profile fits each
//! model target.
//!
//! Fit comes from the site's profile compatibility read, which estimates each profile's defaults
//! against every configured target: it fits now, needs memory freed, is too large, or can't run.
//! Asked for YAML, it prints the catalog models and the compatibility response as returned.

use clap::Args;
use dotcore_proto::dotcore::{
    catalog::v1::{ListModelsRequest, Model},
    deployment::v1::MemoryFit,
    model::v1::{
        ListModelProfileCompatibilityRequest, ListModelProfileCompatibilityResponse,
        ModelCompatibilityStatus, ModelProfileCompatibility,
    },
};
use serde::Serialize;

use crate::{
    management::{DeploymentGroup, Manager, ManagerArgs, NodeNames, call_failed, single_line},
    output::{OutputFormat, present, write_yaml},
    reference::{Referable, ReferenceKind, resolve},
    terminal::Message,
};

/// Arguments for listing the deployable catalog models.
#[derive(Debug, Args)]
pub struct ModelCatalogArgs {
    /// Catalog model to show alone: its ID, name, or a unique part of either.
    #[arg(value_name = "MODEL")]
    model: Option<String>,

    #[command(flatten)]
    manager: ManagerArgs,
}

/// Lists the catalog models and their profiles' fit, as YAML when asked, else as a table.
pub(crate) async fn model_catalog(
    arguments: ModelCatalogArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let manager = arguments.manager.connect().await?;
    let catalog = SiteCatalog::read(&manager, arguments.model.as_deref()).await?;

    if format == OutputFormat::Yaml {
        return write_yaml(&CatalogDocument {
            models: &catalog.models,
            compatibility: &catalog.compatibility,
        });
    }

    if catalog.models.is_empty() {
        return present(&[Message::Info("No catalog models".to_owned())]);
    }
    present(&[table(&catalog)])
}

/// The catalog models the site can deploy, with their profiles' compatibility on every model
/// target, read together so every model command decides from one view.
pub(super) struct SiteCatalog {
    /// Local catalog models, one when a model was named.
    pub(super) models: Vec<Model>,

    /// Each profile's compatibility on each configured model target.
    pub(super) compatibility: ListModelProfileCompatibilityResponse,

    /// Display names of the nodes and their targets.
    pub(super) nodes: NodeNames,
}

impl SiteCatalog {
    /// Reads the catalog and the compatibility of every profile, narrowed to the model `reference`
    /// names when given.
    pub(super) async fn read(manager: &Manager, reference: Option<&str>) -> anyhow::Result<Self> {
        let mut models = manager
            .catalog()
            .list_models(ListModelsRequest {})
            .await
            .map_err(call_failed(
                DeploymentGroup::Model,
                "read the model catalog",
            ))?
            .into_inner()
            .models;
        if let Some(reference) = reference {
            let model = resolve(ReferenceKind::CatalogModel, reference, &models)?.clone();
            models = vec![model];
        }
        let model_id = reference
            .and(models.first())
            .map(|model| model.model_id.clone());
        let compatibility = manager
            .models()
            .list_model_profile_compatibility(ListModelProfileCompatibilityRequest {
                model_id,
                target: None,
                scenario: None,
            })
            .await
            .map_err(call_failed(
                DeploymentGroup::Model,
                "read where the catalog models fit",
            ))?
            .into_inner();
        Ok(Self {
            models,
            compatibility,
            nodes: manager.node_names().await,
        })
    }

    /// The compatibility entries of one profile, one per target that runs models, in node then
    /// target order.
    pub(super) fn entries(
        &self,
        model_id: &str,
        profile_id: &str,
    ) -> impl Iterator<Item = &ModelProfileCompatibility> {
        let (model_id, profile_id) = (model_id.to_owned(), profile_id.to_owned());
        self.compatibility
            .entries
            .iter()
            .filter(move |entry| entry.model_id == model_id && entry.profile_id == profile_id)
    }
}

/// The YAML document of the catalog: the models and the compatibility response, as returned.
#[derive(Serialize)]
struct CatalogDocument<'a> {
    models: &'a [Model],

    compatibility: &'a ListModelProfileCompatibilityResponse,
}

/// The operator words for whether a profile fits a target now.
pub(super) fn fit_label(entry: &ModelProfileCompatibility) -> &'static str {
    match entry.status() {
        ModelCompatibilityStatus::Runs => match (entry.memory_fit(), entry.free_memory_fit()) {
            (_, MemoryFit::Fits) => "fits now",
            (MemoryFit::Fits, MemoryFit::Exceeds) => "needs memory freed",
            (MemoryFit::Exceeds, _) => "too large",
            (MemoryFit::Unspecified, _) => "unsupported sizing",
            _ => "fit unknown",
        },
        ModelCompatibilityStatus::CannotRun => "can't run",
        ModelCompatibilityStatus::Unknown | ModelCompatibilityStatus::Unspecified => "unknown",
    }
}

fn table(catalog: &SiteCatalog) -> Message {
    let mut rows = Vec::new();
    for model in &catalog.models {
        for profile in &model.profiles {
            let row = |target: String, fit: &str| {
                vec![
                    single_line(&model.display_name),
                    single_line(&model.model_id),
                    single_line(&profile.profile_id),
                    target,
                    fit.to_owned(),
                ]
            };
            let mut entries = catalog
                .entries(&model.model_id, &profile.profile_id)
                .peekable();
            if entries.peek().is_none() {
                rows.push(row("-".to_owned(), "no model target"));
            }
            for entry in entries {
                let target = entry.target.as_ref().map_or_else(
                    || "-".to_owned(),
                    |target| {
                        single_line(&format!(
                            "{} {}",
                            catalog.nodes.display(&target.site_node_id),
                            catalog
                                .nodes
                                .target_display(&target.site_node_id, &target.execution_target_id)
                        ))
                    },
                );
                rows.push(row(target, fit_label(entry)));
            }
        }
    }

    Message::Table {
        header: ["MODEL", "ID", "PROFILE", "TARGET", "FIT"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        rows,
    }
}

impl Referable for Model {
    fn id(&self) -> &str {
        &self.model_id
    }

    fn name(&self) -> &str {
        &self.display_name
    }
}
