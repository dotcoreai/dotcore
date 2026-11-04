//! In-process registry for exact Docker workload-group launch identity.

use super::*;

impl<C: DockerClient> DockerAdapter<C> {
    pub(super) fn known_groups(&self) -> &Mutex<BTreeMap<String, KnownGroup>> {
        self.groups.get_or_init(|| Mutex::new(BTreeMap::new()))
    }
}
