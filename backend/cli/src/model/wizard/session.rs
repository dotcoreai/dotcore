//! Keeps an open step's memory footer and hints current while the operator types.
//!
//! Each accepted edit becomes the server's estimate request for the values as they stand. A
//! background worker asks the server for the newest request only, so typing is never held up and
//! values typed past are skipped; answers seen before are reused without a call. The footer keeps
//! the last answer, marked as updating, until the newer one lands.

use std::sync::mpsc::{self, Receiver, RecvError, Sender, TryRecvError};

use dotcore_proto::dotcore::model::v1::{
    EstimateLocalModelResourcesRequest, EstimateLocalModelResourcesResponse,
};
use tokio::runtime::Handle;
use tonic::Status;

use super::{
    super::{memory::footer_lines, site::DeploySite},
    draft::{Draft, FieldKey},
};
use crate::terminal::{Footer, FormField, FormSession};

/// Runs `work` with an estimator whose requests a worker sends to `site` meanwhile, without
/// holding up the async runtime while `work` waits on the terminal.
pub(super) fn with_estimator<T>(site: &dyn DeploySite, work: impl FnOnce(Estimator) -> T) -> T {
    let handle = Handle::current();
    tokio::task::block_in_place(|| {
        std::thread::scope(|scope| {
            let (requests, inbox) = mpsc::channel();
            let (outbox, results) = mpsc::channel();
            scope.spawn(move || estimate_newest(site, &handle, &inbox, &outbox));
            work(Estimator { requests, results })
        })
    })
}

/// Sends estimate requests to the worker and receives its answers.
pub(super) struct Estimator {
    requests: Sender<EstimateLocalModelResourcesRequest>,
    results: Receiver<Answered>,
}

/// A request and the server's answer to it.
type Answered = (
    EstimateLocalModelResourcesRequest,
    Result<EstimateLocalModelResourcesResponse, Status>,
);

/// An open step of the draft: its fields, the request waiting for an answer and the last failure.
pub(super) struct DraftSession<'a> {
    draft: &'a mut Draft,
    keys: Vec<FieldKey>,
    estimator: Estimator,

    /// The request answering the values as they stand, while its answer is on its way.
    wanted: Option<EstimateLocalModelResourcesRequest>,

    /// Why the latest estimate failed, until one succeeds.
    failure: Option<String>,
}

impl<'a> DraftSession<'a> {
    /// Opens a step whose fields set `keys`, in order.
    pub(super) fn new(draft: &'a mut Draft, keys: Vec<FieldKey>, estimator: Estimator) -> Self {
        Self {
            draft,
            keys,
            estimator,
            wanted: None,
            failure: None,
        }
    }

    fn receive(&mut self, (request, result): Answered) {
        let wanted = self.wanted.as_ref() == Some(&request);
        match result {
            Ok(response) => {
                self.draft.keep((request, response));
                if wanted {
                    self.failure = None;
                }
            }
            Err(status) if wanted => {
                self.failure = Some(format!("Memory estimate failed: {}", status.message()));
            }
            Err(_) => {}
        }
        if wanted {
            self.wanted = None;
        }
    }
}

impl FormSession for DraftSession<'_> {
    fn edited(&mut self, fields: &mut [FormField]) {
        if self.draft.take(&self.keys, fields) {
            match self.draft.request() {
                Ok(request) => match self.draft.kept(&request).cloned() {
                    Some(kept) => {
                        self.draft.estimate = Some(kept);
                        self.wanted = None;
                    }
                    None if self.wanted.as_ref() == Some(&request) => {}
                    None => {
                        // A closed worker leaves nothing to wait for.
                        self.wanted = self
                            .estimator
                            .requests
                            .send(request.clone())
                            .is_ok()
                            .then_some(request);
                    }
                },
                Err(error) => self.failure = Some(format!("{error:#}")),
            }
        }
        self.draft.describe(&self.keys, fields);
    }

    fn refresh(&mut self, fields: &mut [FormField], wait: bool) -> bool {
        let mut changed = false;
        loop {
            let received = if wait && self.wanted.is_some() {
                self.estimator.results.recv().map_err(|RecvError| ())
            } else {
                self.estimator.results.try_recv().map_err(|error| {
                    if error == TryRecvError::Disconnected {
                        self.wanted = None;
                    }
                })
            };
            let Ok(answered) = received else {
                if wait {
                    self.wanted = None;
                }
                break;
            };
            self.receive(answered);
            changed = true;
        }
        if changed {
            self.draft.describe(&self.keys, fields);
        }
        changed
    }

    fn pending(&self) -> bool {
        self.wanted.is_some()
    }

    fn footer(&self) -> Option<Footer> {
        footer(self.draft, self.failure.as_deref(), self.pending())
    }
}

/// The footer of the draft's shown estimate, with a failure under it, if any.
pub(super) fn footer(draft: &Draft, failure: Option<&str>, updating: bool) -> Option<Footer> {
    let (_, estimate) = draft.estimate.as_ref()?;
    let mut lines = footer_lines(estimate, draft.parallel_requests());
    if let Some(failure) = failure {
        lines.push(crate::terminal::Message::Error(failure.to_owned()));
    }
    Some(Footer {
        title: format!(
            "{} {}",
            draft.selection.node_name, draft.selection.target_name
        ),
        updating,
        lines,
    })
}

/// Answers the newest request waiting each time, until the session closes.
fn estimate_newest(
    site: &dyn DeploySite,
    handle: &Handle,
    inbox: &Receiver<EstimateLocalModelResourcesRequest>,
    outbox: &Sender<Answered>,
) {
    while let Ok(mut request) = inbox.recv() {
        // Only the values as they now stand matter; older requests still queued are skipped.
        while let Ok(newer) = inbox.try_recv() {
            request = newer;
        }
        let result = handle.block_on(site.estimate(request.clone()));
        if outbox.send((request, result)).is_err() {
            return;
        }
    }
}
