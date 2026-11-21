//! The set of installed sources.

use crate::extensions;
use crate::source::Source;

/// Every source the app can talk to, looked up by stable id.
#[derive(Default)]
pub struct Registry {
    sources: Vec<Box<dyn Source>>,
}

impl Registry {
    pub fn new(sources: Vec<Box<dyn Source>>) -> Self {
        Self { sources }
    }

    /// The sources compiled into this build.
    pub fn builtin() -> Self {
        Self::new(extensions::builtin())
    }

    pub fn add(&mut self, source: Box<dyn Source>) {
        self.sources.push(source);
    }

    /// All sources, e.g. for a browse screen grouped by language.
    pub fn all(&self) -> &[Box<dyn Source>] {
        &self.sources
    }

    /// Source owning a library entry. `None` means its extension is missing.
    pub fn by_id(&self, id: u64) -> Option<&dyn Source> {
        self.sources
            .iter()
            .find(|source| source.info().id == id)
            .map(|source| source.as_ref())
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = self
            .sources
            .iter()
            .map(|source| source.info().name.as_str())
            .collect::<Vec<_>>();
        formatter
            .debug_struct("Registry")
            .field("sources", &names)
            .finish()
    }
}
