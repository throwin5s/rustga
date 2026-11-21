//! Small helpers over [`scraper`], so each extension does not repeat them.
//!
//! Selectors are strings because they come from the site, not the type system:
//! an invalid selector yields `None` instead of panicking, and a missing element
//! is a normal parse failure the extension can decide how to handle.

use scraper::{Html, Selector};

/// Parse a document. Call once per response and select from the result.
pub fn document(body: &str) -> Html {
    Html::parse_document(body)
}

/// Text of the first match, with runs of whitespace collapsed.
pub fn text(document: &Html, selector: &str) -> Option<String> {
    let selector = Selector::parse(selector).ok()?;
    document
        .select(&selector)
        .next()
        .map(|element| normalize(&element.text().collect::<Vec<_>>().join(" ")))
        .filter(|text| !text.is_empty())
}

/// Texts of every match, in document order.
pub fn texts(document: &Html, selector: &str) -> Vec<String> {
    let Ok(selector) = Selector::parse(selector) else {
        return Vec::new();
    };
    document
        .select(&selector)
        .map(|element| normalize(&element.text().collect::<Vec<_>>().join(" ")))
        .filter(|text| !text.is_empty())
        .collect()
}

/// One attribute of the first match.
pub fn attr(document: &Html, selector: &str, attribute: &str) -> Option<String> {
    let selector = Selector::parse(selector).ok()?;
    document
        .select(&selector)
        .next()
        .and_then(|element| element.attr(attribute))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// That attribute from every match, skipping elements without it.
pub fn attrs(document: &Html, selector: &str, attribute: &str) -> Vec<String> {
    let Ok(selector) = Selector::parse(selector) else {
        return Vec::new();
    };
    document
        .select(&selector)
        .filter_map(|element| element.attr(attribute))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Collapse whitespace, which scraped markup is full of.
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
