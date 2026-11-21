//! Built-in extensions.
//!
//! A source is an ordinary Rust module in this crate, not an APK: implement
//! [`Source`] over a [`crate::http::HttpSource`] and return it from [`builtin`].
//! Adding one touches this file and one new module, nothing else.
//!
//! # Adding a source
//!
//! 1. Create `src/extensions/<site>.rs` with a struct holding a
//!    [`SourceInfo`](crate::source::SourceInfo) and an
//!    [`HttpSource`](crate::http::HttpSource); implement
//!    [`Source`] (`popular`, `details`, `chapters`, `pages`, plus `search`
//!    and `latest` when the site supports them).
//! 2. Port selectors, URL patterns and the rate limit from the matching
//!    Keiyoushi extension instead of guessing; a wrong selector is silent.
//!    Where no extension exists, record the endpoint or selector source in the
//!    module docs the way `mangadot` does.
//! 3. Declare the module below and push the source in [`builtin`].
//! 4. Keep parsing pure functions and unit-test them with recorded markup, so
//!    a layout change fails a test instead of silently returning nothing.

use crate::source::Source;

mod mangadot;

/// Sources compiled into this build.
pub fn builtin() -> Vec<Box<dyn Source>> {
    vec![Box::new(
        mangadot::MangaDot::new().expect("MangaDot source needs a working HTTP client"),
    )]
}
