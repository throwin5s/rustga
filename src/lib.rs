//! Core of **rustga**, a lightweight manga, manhwa and manhua reader.
//!
//! Sources ("extensions") are ordinary Rust modules implementing
//! [`source::Source`] — not Android APKs interpreted at runtime. A source
//! describes one website: how to list it, search it, and turn a chapter into a
//! list of image URLs. Everything else (UI, library, downloads) is the host
//! application.
//!
//! ```text
//! Source       extension-facing trait, one implementation per website
//! HttpSource   shared HTTP client: headers, per-source rate limit, URL joining
//! html         scraper-backed helpers for HTML sources
//! filters      search filter declarations and per-search values
//! Registry     installed sources, looked up by stable id
//! ```
//!
//! Sources are synchronous and `Send + Sync`: the host calls them from worker
//! threads so a slow site never blocks the interface thread.

pub mod error;
pub mod extensions;
pub mod filters;
pub mod html;
pub mod http;
pub mod local;
pub mod models;
pub mod registry;
pub mod source;

pub use error::{Error, Result};
pub use http::{join_url, HttpSource};
pub use registry::Registry;
pub use source::{Source, SourceInfo};
