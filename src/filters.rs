//! Search filters, modelled on Mihon's `FilterList`.
//!
//! A source *declares* the filters it understands ([`Filter`]); the host renders
//! widgets for them and passes the user's choices back as [`FilterValues`].
//! Declarations are static, values are per-search, so a source stays `Sync`.

use std::collections::HashMap;

/// One filter control a source supports, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// Section label, not an input.
    Header(String),
    /// Visual break between sections.
    Separator,
    /// Dropdown; the answer is an index into `options`.
    Select { name: String, options: Vec<String> },
    /// Free-text field.
    Text { name: String },
    /// Single checkbox.
    CheckBox { name: String },
    /// Include / exclude / ignore, like Mihon's `TriState`.
    TriState { name: String },
}

impl Filter {
    /// Label shown to the user; `None` for separators.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Header(name)
            | Self::Select { name, .. }
            | Self::Text { name }
            | Self::CheckBox { name }
            | Self::TriState { name } => Some(name.as_str()),
            Self::Separator => None,
        }
    }
}

/// Three-way state used by [`Filter::TriState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TriState {
    #[default]
    Ignore,
    Include,
    Exclude,
}

impl TriState {
    pub fn is_include(self) -> bool {
        matches!(self, Self::Include)
    }

    pub fn is_exclude(self) -> bool {
        matches!(self, Self::Exclude)
    }
}

/// A user's answer for one filter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FilterValue {
    #[default]
    Unset,
    Text(String),
    CheckBox(bool),
    /// Index into the owning [`Filter::Select`].
    Select(usize),
    TriState(TriState),
}

/// The values chosen for one search.
#[derive(Debug, Clone, Default)]
pub struct FilterValues {
    values: HashMap<String, FilterValue>,
}

impl FilterValues {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: impl Into<String>, value: FilterValue) {
        self.values.insert(name.into(), value);
    }

    pub fn get(&self, name: &str) -> Option<&FilterValue> {
        self.values.get(name)
    }

    /// Text of a [`Filter::Text`]; empty and unset both give `None`.
    pub fn text(&self, name: &str) -> Option<&str> {
        match self.get(name) {
            Some(FilterValue::Text(text)) if !text.is_empty() => Some(text.as_str()),
            _ => None,
        }
    }

    pub fn checked(&self, name: &str) -> bool {
        matches!(self.get(name), Some(FilterValue::CheckBox(true)))
    }

    /// Chosen index of a [`Filter::Select`].
    pub fn selected(&self, name: &str) -> Option<usize> {
        match self.get(name) {
            Some(FilterValue::Select(index)) => Some(*index),
            _ => None,
        }
    }

    /// State of a [`Filter::TriState`], which defaults to ignore.
    pub fn tri_state(&self, name: &str) -> TriState {
        match self.get(name) {
            Some(FilterValue::TriState(state)) => *state,
            _ => TriState::Ignore,
        }
    }
}
