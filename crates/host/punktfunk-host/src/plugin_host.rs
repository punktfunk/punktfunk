//! Plugins on this host: the runner and each plugin's access, and the store that installs
//! them from signed catalogs.

pub(crate) mod plugins;
// Signed catalogs and install jobs via the `plugins` runner — design/plugin-store.md.
pub(crate) mod store;
