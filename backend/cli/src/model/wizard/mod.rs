//! Deploys one local model step by step on a terminal, asking what the console's deploy dialog
//! asks, in the same order and with the same fields in each step.
//!
//! The model, machine and profile are picked from lists; the capabilities, the context and the
//! review are each one form showing all of the step's fields, with the server's memory estimate
//! of the values as they stand pinned under it. Each finished step leaves a one-line summary,
//! what the operator already named on the command line is not asked, and going back reopens the
//! previous step. The wizard computes no fit or sizing itself; nothing is sent until the operator
//! deploys, and the deployment is created from the same payload and request a payload file makes.

mod draft;
mod session;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use anyhow::bail;
use dotcore_proto::dotcore::{
    catalog::v1::{Model, ModelProfile},
    model::v1::{ModelCompatibilityStatus, ModelDeployment},
};
use tonic::Code;

use super::{
    catalog::{SiteCatalog, best_fit, fit_label, reason_words},
    deploy::{advisories_words, create_request, deployment_name},
    memory::{memory_block, memory_line, verdict},
    payload::ModelPayload,
    selection::{Selection, Target, default_profile, default_target, entry, preview, targets},
    site::DeploySite,
};
use crate::{
    deploy::Setting,
    management::{DeploymentGroup, call_failed},
    reference::{ReferenceKind, resolve},
    terminal::{FormAnswer, FormPrompt, Message, PromptError, SelectItem, SelectPrompt, Terminal},
};
use draft::Draft;
use session::{DraftSession, with_estimator};

/// What the operator named on the command line, which the wizard takes without asking.
#[derive(Clone, Debug, Default)]
pub(super) struct WizardDefaults {
    /// Catalog model reference.
    pub(super) model: Option<String>,

    /// Profile reference.
    pub(super) profile: Option<String>,

    /// Node reference.
    pub(super) node: Option<String>,

    /// Execution target reference on the node.
    pub(super) target: Option<String>,

    /// Display name of the deployment.
    pub(super) name: Option<String>,

    /// Name of the route created with the deployment.
    pub(super) route: Option<String>,

    /// Parameter and option overrides, each answering its field.
    pub(super) settings: Vec<Setting>,
}

/// A deployment the wizard created, with the payload it was created from.
#[derive(Debug)]
pub(super) struct Deployed {
    /// The deployment as the manager accepted it.
    pub(super) deployment: ModelDeployment,

    /// The payload the operator built, which a payload file can deploy again.
    pub(super) payload: ModelPayload,
}

/// Walks the operator through the steps on `terminal` and creates the deployment they review.
/// Cancelling at any step creates nothing.
pub(super) async fn run(
    site: &dyn DeploySite,
    terminal: &mut dyn Terminal,
    defaults: WizardDefaults,
) -> anyhow::Result<Deployed> {
    let catalog = SiteCatalog::read(site, defaults.model.as_deref()).await?;
    if catalog.models.is_empty() {
        bail!("the site's catalog has no model to deploy");
    }
    let selection = choose(site, terminal, &catalog, &defaults, None).await?;
    let draft = Draft::new(selection, &defaults)?;
    let mut wizard = Wizard {
        site,
        terminal,
        defaults,
        catalog,
        draft,
        refusal: None,
    };
    wizard.summarize(Step::ModelAndMachine)?;

    let mut position = 0;
    loop {
        let steps = wizard.form_steps();
        let step = steps[position.min(steps.len() - 1)];
        match wizard.form(step).await? {
            FormAnswer::Back if position == 0 => wizard.choose_again().await?,
            FormAnswer::Back => position -= 1,
            FormAnswer::Next if step == Step::Review => {
                if let Some(deployed) = wizard.create().await? {
                    return Ok(deployed);
                }
            }
            FormAnswer::Next => {
                wizard.finish(step)?;
                position += 1;
            }
        }
    }
}

/// One step of the wizard. Advanced steps go between the context and the review.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    ModelAndMachine,

    Capabilities,

    Context,

    Review,
}

impl Step {
    fn title(self) -> &'static str {
        match self {
            Self::ModelAndMachine => "Model & machine",
            Self::Capabilities => "Capabilities",
            Self::Context => "Context",
            Self::Review => "Review & deploy",
        }
    }
}

/// The wizard once a model, machine and profile are chosen.
struct Wizard<'a> {
    site: &'a dyn DeploySite,
    terminal: &'a mut dyn Terminal,
    defaults: WizardDefaults,
    catalog: SiteCatalog,
    draft: Draft,

    /// Why the manager refused the last deployment, shown at the review until it is deployed.
    refusal: Option<String>,
}

impl Wizard<'_> {
    /// The steps asked as forms, in order: Capabilities only with a basic option to ask.
    fn form_steps(&self) -> Vec<Step> {
        let mut steps = Vec::new();
        if self.draft.asks_capabilities(&self.defaults) {
            steps.push(Step::Capabilities);
        }
        steps.extend([Step::Context, Step::Review]);
        steps
    }

    /// Shows one step as a form until the operator finishes it or goes back.
    async fn form(&mut self, step: Step) -> anyhow::Result<FormAnswer> {
        // The form opens with the footer of the values as they stand.
        self.estimate().await?;
        let live = self.terminal.live_footer();
        let (keys, fields) = self.draft.fields(step, &self.defaults);
        let mut steps = vec![Step::ModelAndMachine];
        steps.extend(self.form_steps());
        let mut prompt = FormPrompt {
            title: step.title().to_owned(),
            current: steps.iter().position(|shown| *shown == step).unwrap_or(0),
            steps: steps.iter().map(|step| step.title().to_owned()).collect(),
            fields,
            notes: self.notes(step, live),
            action: (step == Step::Review).then(|| "Deploy".to_owned()),
            can_go_back: true,
        };

        let (site, terminal, draft) = (self.site, &mut *self.terminal, &mut self.draft);
        let answer = with_estimator(site, |estimator| {
            let mut session = DraftSession::new(draft, keys, estimator);
            terminal.form(&mut prompt, &mut session)
        })?;
        Ok(answer)
    }

    /// Leaves a finished step's summary line and, without a live footer, the memory after the
    /// context.
    fn finish(&mut self, step: Step) -> anyhow::Result<()> {
        self.summarize(step)?;
        if step == Step::Context && !self.terminal.live_footer() {
            for message in self.memory_messages() {
                self.terminal.prompt_context(&message)?;
            }
        }
        Ok(())
    }

    /// Asks the model, machine and profile again, starting over when the choice changes. With
    /// nothing to ask, going back from the first step cancels.
    async fn choose_again(&mut self) -> anyhow::Result<()> {
        let targets = targets(
            &self.catalog,
            self.defaults.node.as_deref(),
            self.defaults.target.as_deref(),
        )?;
        if self.catalog.models.len() == 1 && targets.len() == 1 && self.defaults.profile.is_some() {
            return Err(PromptError::Cancelled.into());
        }
        let selection = choose(
            self.site,
            &mut *self.terminal,
            &self.catalog,
            &self.defaults,
            Some(&self.draft.selection),
        )
        .await?;
        if !same_choice(&selection, &self.draft.selection) {
            self.draft.select(selection, &self.defaults)?;
        }
        self.summarize(Step::ModelAndMachine)?;
        Ok(())
    }

    /// Creates the deployment and leaves the review and memory lines, or keeps why the manager
    /// refused it for the review and returns none.
    async fn create(&mut self) -> anyhow::Result<Option<Deployed>> {
        let request = create_request(&self.draft.selection, &self.draft.payload)?;
        match self.site.create(request).await {
            Ok(deployment) => {
                self.summarize(Step::Review)?;
                if let Some((_, estimate)) = &self.draft.estimate {
                    let machine = format!(
                        "{} {}",
                        self.draft.selection.node_name, self.draft.selection.target_name
                    );
                    self.terminal
                        .prompt_context(&memory_line(estimate, &machine))?;
                }
                Ok(Some(Deployed {
                    deployment,
                    payload: self.draft.payload.clone(),
                }))
            }
            Err(status) if matches!(status.code(), Code::InvalidArgument | Code::AlreadyExists) => {
                self.refusal = Some(format!(
                    "The manager refused the deployment: {}",
                    status.message()
                ));
                Ok(None)
            }
            Err(status) => Err(call_failed(
                DeploymentGroup::Model,
                format!(
                    "deploy {}",
                    deployment_name(&self.draft.selection, &self.draft.payload)
                ),
            )(status)
            .into()),
        }
    }

    /// Has the server estimate the values as they stand unless it already did.
    async fn estimate(&mut self) -> anyhow::Result<()> {
        let request = self.draft.request()?;
        if let Some(kept) = self.draft.kept(&request).cloned() {
            self.draft.estimate = Some(kept);
            return Ok(());
        }
        let response = self
            .site
            .estimate(request.clone())
            .await
            .map_err(call_failed(
                DeploymentGroup::Model,
                format!(
                    "estimate the memory of {}",
                    self.draft.selection.model.display_name
                ),
            ))?;
        self.draft.keep((request, response));
        Ok(())
    }

    /// The lines under a step's fields. The review shows its facts, every standing warning and a
    /// refusal; without a live footer it also states the memory.
    fn notes(&self, step: Step, live: bool) -> Vec<Message> {
        if step != Step::Review {
            return Vec::new();
        }
        let mut notes = if live {
            Vec::new()
        } else {
            self.memory_messages()
        };
        notes.push(Message::KeyValues(self.draft.review_facts()));
        if live
            && let Some((_, estimate)) = &self.draft.estimate
            && let message @ Message::Warning(_) = verdict(estimate)
        {
            notes.push(message);
        }
        notes.extend(
            advisories_words(&self.draft.selection)
                .into_iter()
                .map(|advisory| Message::Warning(advisory.to_owned())),
        );
        notes.extend(self.refusal.clone().map(Message::Error));
        notes
    }

    /// The memory of the shown estimate as plain messages: the verdict, the bars and the fits.
    fn memory_messages(&self) -> Vec<Message> {
        let Some((_, estimate)) = &self.draft.estimate else {
            return Vec::new();
        };
        let mut messages = vec![verdict(estimate)];
        messages.extend(memory_block(estimate, self.draft.parallel_requests()));
        messages.extend(
            estimate
                .hardware_class_estimates
                .iter()
                .filter_map(|entry| entry.launch_rejection.as_deref())
                .map(|reason| {
                    Message::Error(format!("A launch would reject these values: {reason}"))
                }),
        );
        messages
    }

    /// Shows a finished step as one line.
    fn summarize(&mut self, step: Step) -> Result<(), PromptError> {
        let summary = self.draft.summary(step);
        self.terminal
            .prompt_context(&Message::Success(format!("{}: {summary}", step.title())))
    }
}

/// Asks the model, the machine and the profile, each unless named or alone, then has the server
/// preview the choice unless it is `current`'s.
async fn choose(
    site: &dyn DeploySite,
    terminal: &mut dyn Terminal,
    catalog: &SiteCatalog,
    defaults: &WizardDefaults,
    current: Option<&Selection>,
) -> anyhow::Result<Selection> {
    terminal.prompt_context(&Message::Heading(Step::ModelAndMachine.title().to_owned()))?;
    let model = choose_model(terminal, catalog, current)?;
    let targets = targets(
        catalog,
        defaults.node.as_deref(),
        defaults.target.as_deref(),
    )?;
    let named = defaults
        .profile
        .as_deref()
        .map(|reference| resolve(ReferenceKind::Profile, reference, &model.profiles))
        .transpose()?;
    let target = choose_target(terminal, catalog, model, named, &targets, current)?;
    let profile = match named {
        Some(profile) => profile,
        None => choose_profile(terminal, catalog, model, target, current)?,
    };

    match current {
        Some(current)
            if current.model.model_id == model.model_id
                && current.profile.profile_id == profile.profile_id
                && current.placement == target.placement =>
        {
            Ok(current.clone())
        }
        _ => preview(site, catalog, model, profile, target).await,
    }
}

/// The model to deploy: the only one, or the one the operator picks among the catalog's, each
/// with its best fit on the site. The pick starts at `current`'s model, else the first that fits
/// now. A model no profile of which runs anywhere says so and is asked again.
fn choose_model<'a>(
    terminal: &mut dyn Terminal,
    catalog: &'a SiteCatalog,
    current: Option<&Selection>,
) -> anyhow::Result<&'a Model> {
    let models = &catalog.models;
    if let [model] = models.as_slice() {
        return Ok(model);
    }

    let fits = models
        .iter()
        .map(|model| {
            let entries = || {
                catalog
                    .compatibility
                    .entries
                    .iter()
                    .filter(|entry| entry.model_id == model.model_id)
            };
            let runs = entries().any(|entry| entry.status() == ModelCompatibilityStatus::Runs);
            (best_fit(entries()), runs)
        })
        .collect::<Vec<_>>();
    let items = models
        .iter()
        .zip(&fits)
        .map(|(model, (fit, runs))| {
            let item = SelectItem::new(model.display_name.clone());
            match (fit, runs) {
                (None, _) => item
                    .with_description(model.model_id.clone())
                    .with_unavailable_reason("no machine can run models"),
                (Some(_), false) => item
                    .with_description(model.model_id.clone())
                    .with_unavailable_reason("no profile runs on any machine"),
                (Some(fit), true) => item.with_description(format!("{} · {fit}", model.model_id)),
            }
        })
        .collect::<Vec<_>>();
    let default = current
        .and_then(|current| {
            models
                .iter()
                .position(|model| model.model_id == current.model.model_id)
        })
        .or_else(|| {
            fits.iter()
                .position(|(fit, runs)| *runs && *fit == Some("fits now"))
        })
        .unwrap_or(0);

    loop {
        let index = terminal
            .select(&SelectPrompt::new("Model", items.iter().cloned()).with_default(default))?;
        match items[index].unavailable_reason() {
            Some(reason) => terminal.prompt_context(&Message::Error(format!(
                "{} can't be deployed: {reason}",
                items[index].label()
            )))?,
            None => return Ok(&models[index]),
        }
    }
}

/// The machine to deploy on: the only target, or the one the operator picks, starting from
/// `current`'s or the default target.
fn choose_target<'a>(
    terminal: &mut dyn Terminal,
    catalog: &SiteCatalog,
    model: &Model,
    profile: Option<&ModelProfile>,
    targets: &'a [Target],
    current: Option<&Selection>,
) -> anyhow::Result<&'a Target> {
    if let [target] = targets {
        return Ok(target);
    }
    let fallback = default_target(catalog, model, profile, targets);
    let default = current
        .and_then(|current| {
            targets
                .iter()
                .position(|target| target.placement == current.placement)
        })
        .or_else(|| {
            targets
                .iter()
                .position(|target| target.placement == fallback.placement)
        })
        .unwrap_or(0);
    let items = targets.iter().map(|target| {
        let fit = best_fit(
            model
                .profiles
                .iter()
                .filter_map(|profile| entry(catalog, model, profile, target)),
        )
        .unwrap_or("fit unknown");
        SelectItem::new(format!("{} {}", target.node_name, target.target_name))
            .with_description(fit)
    });
    let index = terminal.select(&SelectPrompt::new("Machine", items).with_default(default))?;
    Ok(&targets[index])
}

/// The profile to deploy: the one the operator picks among the model's, each with its fit on the
/// target. A profile that can't run there says why and is asked again.
fn choose_profile<'a>(
    terminal: &mut dyn Terminal,
    catalog: &SiteCatalog,
    model: &'a Model,
    target: &Target,
    current: Option<&Selection>,
) -> anyhow::Result<&'a ModelProfile> {
    let fallback = default_profile(catalog, model, std::slice::from_ref(target))?;
    let default = current
        .and_then(|current| {
            model
                .profiles
                .iter()
                .position(|profile| profile.profile_id == current.profile.profile_id)
        })
        .or_else(|| {
            model
                .profiles
                .iter()
                .position(|profile| profile == fallback)
        })
        .unwrap_or(0);

    let items = model
        .profiles
        .iter()
        .map(|profile| {
            let item = SelectItem::new(profile_label(profile));
            match entry(catalog, model, profile, target) {
                None => item.with_description("fit unknown"),
                Some(entry) if entry.status() != ModelCompatibilityStatus::Runs => {
                    item.with_unavailable_reason(reason_words(entry, &target.node_name))
                }
                Some(entry) => {
                    item.with_description(match entry.largest_context_tokens_per_request {
                        Some(tokens) => format!(
                            "{} · fits up to {tokens} per request, all requests at once",
                            fit_label(entry)
                        ),
                        None => fit_label(entry).to_owned(),
                    })
                }
            }
        })
        .collect::<Vec<_>>();
    loop {
        let index = terminal
            .select(&SelectPrompt::new("Profile", items.iter().cloned()).with_default(default))?;
        match items[index].unavailable_reason() {
            Some(reason) => terminal.prompt_context(&Message::Error(format!(
                "{} can't be deployed: {reason}",
                items[index].label()
            )))?,
            None => return Ok(&model.profiles[index]),
        }
    }
}

/// Whether two selections deploy the same profile of the same model on the same target.
fn same_choice(left: &Selection, right: &Selection) -> bool {
    left.model.model_id == right.model.model_id
        && left.profile.profile_id == right.profile.profile_id
        && left.placement == right.placement
}

/// A profile as the operator reads it: its name, else its ID.
fn profile_label(profile: &ModelProfile) -> &str {
    if profile.display_name.is_empty() {
        &profile.profile_id
    } else {
        &profile.display_name
    }
}

/// The route a deployment name suggests: lowercase letters, digits and dots, other characters
/// joined by hyphens, starting with a letter, at most 63 characters. The server validates it.
fn route_name(name: &str) -> String {
    let mut slug = String::new();
    for character in name.trim().to_lowercase().chars() {
        match character {
            'a'..='z' | '0'..='9' => slug.push(character),
            '.' if !slug.ends_with('.') => slug.push('.'),
            '.' => {}
            _ if !slug.ends_with('-') => slug.push('-'),
            _ => {}
        }
    }
    let slug = slug.trim_matches(['-', '.']);
    let slug = match slug.chars().next() {
        Some(first) if !first.is_ascii_lowercase() => format!("model-{slug}"),
        _ => slug.to_owned(),
    };
    slug.chars()
        .take(63)
        .collect::<String>()
        .trim_end_matches(['-', '.'])
        .to_owned()
}
