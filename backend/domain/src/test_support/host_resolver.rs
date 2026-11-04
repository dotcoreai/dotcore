//! Fake host-name resolver for unit tests: fixed answers per name, with every lookup recorded,
//! so tests never depend on the machine's DNS.

use std::{
    collections::BTreeMap,
    net::Ipv4Addr,
    sync::{Mutex, PoisonError},
};

use futures_util::future::BoxFuture;

use crate::HostResolver;

/// Resolver answering only the names it was given; any other name fails as unknown.
#[derive(Debug, Default)]
pub struct FakeHostResolver {
    answers: BTreeMap<String, Vec<Ipv4Addr>>,
    lookups: Mutex<Vec<String>>,
}

impl FakeHostResolver {
    /// Answers `name` with `addresses`; an empty list answers with no IPv4 address.
    pub fn with(mut self, name: &str, addresses: impl IntoIterator<Item = Ipv4Addr>) -> Self {
        self.answers
            .insert(name.to_owned(), addresses.into_iter().collect());
        self
    }

    /// Returns the names looked up so far, in order.
    pub fn lookups(&self) -> Vec<String> {
        self.lookups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl HostResolver for FakeHostResolver {
    fn lookup_ipv4<'a>(&'a self, name: &'a str) -> BoxFuture<'a, std::io::Result<Vec<Ipv4Addr>>> {
        self.lookups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(name.to_owned());
        let answer = self.answers.get(name).cloned().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, format!("{name} is unknown"))
        });
        Box::pin(async move { answer })
    }
}
