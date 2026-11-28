//! Turning a pasted recommendation thread into titles to look up.
//!
//! Two extractors produce the same output. [`heuristic`] reads the plain text
//! with no key and no network; [`chat`] asks an OpenAI-compatible chat endpoint
//! to read what the thread actually recommends. The model is trusted only for
//! titles and quotes — covers, synopses and the series identity itself come from
//! the sources, so an invented title simply fails to resolve.
//!
//! Reddit no longer serves unauthenticated `.json` permalinks, so the thread
//! arrives as pasted text rather than a URL.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::local;
use crate::models::Manga;

/// Characters of thread text sent to the model. Long threads are truncated
/// rather than refused; the top comments are what matters.
const MAX_THREAD_CHARS: usize = 60_000;
const MAX_RECOMMENDATIONS: usize = 40;

/// Phrases that mark prose rather than a title.
const NOISE: [&str; 21] = [
    " i ",
    "i'm ",
    "i've ",
    "i hate",
    "i love",
    "i read",
    "recommend",
    "looking for",
    "does any",
    "is there",
    "anyone",
    "thank",
    "chapter",
    "episode",
    "season",
    "read this",
    "the op",
    "op ",
    "same as",
    "like ",
    "such as",
];

/// Openers that mean the title comes after them, never before.
const LEAD_INS: [&str; 9] = [
    "from ",
    "like ",
    "such as",
    "for example",
    "here are",
    "here is",
    "these are",
    "some of",
    "my picks",
];

/// Lines introduced this way are naming what the author does *not* want.
const COUNTER_EXAMPLES: [&str; 6] = [
    "like for example",
    "for example",
    "e.g",
    "i.e",
    "i don't like",
    "i dont like",
];

/// One series a thread recommends, with the comment that recommended it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    pub title: String,
    #[serde(default)]
    pub alt_titles: Vec<String>,
    /// Verbatim fragment of the comment that recommended it.
    #[serde(default)]
    pub quote: Option<String>,
    /// One line on why it fits the request.
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub confidence: Option<String>,
}

/// Where the extraction call goes. Stored in the app's data folder, never here.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    pub base_url: String,
    pub model: String,
    pub key: String,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            base_url: "https://api.z.ai/api/paas/v4".to_owned(),
            model: "glm-4-flash".to_owned(),
            key: String::new(),
        }
    }
}

impl AiSettings {
    pub fn load(root: &Path) -> Self {
        local::read(&root.join("ai.json"))
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        local::write(&root.join("ai.json"), self)
    }

    /// Settings with the environment folded in, for people who would rather not
    /// keep a key in a file. The stored key wins when both are present.
    pub fn active(&self) -> Self {
        let mut active = self.clone();
        if active.key.trim().is_empty() {
            if let Ok(key) = std::env::var("ZAI_API_KEY") {
                active.key = key;
            }
        }
        if let Ok(base) = std::env::var("ZAI_API_BASE") {
            if !base.trim().is_empty() {
                active.base_url = base;
            }
        }
        if let Ok(model) = std::env::var("ZAI_MODEL") {
            if !model.trim().is_empty() {
                active.model = model;
            }
        }
        active
    }

    pub fn configured(&self) -> bool {
        !self.active().key.trim().is_empty()
    }
}

const SYSTEM_PROMPT: &str = "You extract manga, manhwa and manhua recommendations from a Reddit thread. \
Reply with JSON only, no prose and no code fences, exactly in this shape: \
{\"recommendations\":[{\"title\":\"...\",\"alt_titles\":[\"...\"],\"quote\":\"...\",\"why\":\"...\",\"confidence\":\"high\"}]}. \
Rules: only include series that a person in the thread actually recommends. Leave out anything the \
original poster names as an example of what they dislike, and anything a commenter calls bad. Keep \
the title exactly as written, in its original script; never translate it or invent one. List every \
other name the thread uses for the same series in alt_titles, including native and romanized \\
titles. quote must be a verbatim fragment of 5 to 25 words from the comment that recommended it. why \
is one short sentence about how the series fits. confidence is high, medium or low. At most 40 \
entries, no duplicates. If nobody recommends anything, return {\"recommendations\":[]}.";

/// Ask an OpenAI-compatible endpoint what the thread recommends.
pub fn chat(settings: &AiSettings, thread: &str) -> Result<Vec<Recommendation>, String> {
    let active = settings.active();
    if active.key.trim().is_empty() {
        return Err("no API key".to_owned());
    }
    let endpoint = format!("{}/chat/completions", active.base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": active.model,
        "temperature": 0.1,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": excerpt(thread)}
        ]
    });
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(90))
        .user_agent(concat!("rustga/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .post(&endpoint)
        .bearer_auth(active.key.trim())
        .json(&body)
        .send()
        .map_err(|error| format!("could not reach the AI endpoint: {error}"))?;
    let status = response.status();
    let text = response.text().map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "AI endpoint answered {status}: {}",
            trim_to(&text, 200)
        ));
    }
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| "the AI endpoint did not return JSON".to_owned())?;
    let content = message_content(&value).ok_or("the AI reply had no content")?;
    parse_model_json(&content)
}

/// Handles both a plain string and a list of content parts.
fn message_content(value: &serde_json::Value) -> Option<String> {
    let content = value
        .get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let parts = content.as_array()?;
    let joined = parts
        .iter()
        .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    (!joined.is_empty()).then_some(joined)
}

/// Reads the JSON object out of a reply that may wrap it in prose or fences.
pub fn parse_model_json(content: &str) -> Result<Vec<Recommendation>, String> {
    let start = content.find('{').ok_or("the model did not return JSON")?;
    let end = content.rfind('}').ok_or("the model did not return JSON")?;
    if end <= start {
        return Err("the model did not return JSON".to_owned());
    }
    #[derive(Deserialize)]
    struct Reply {
        #[serde(default)]
        recommendations: Vec<Recommendation>,
    }
    let reply: Reply = serde_json::from_str(&content[start..=end])
        .map_err(|error| format!("could not read the model's JSON: {error}"))?;
    Ok(sanitize(reply.recommendations))
}

/// Drops unusable entries, normalises quotes and removes duplicates.
fn sanitize(recs: Vec<Recommendation>) -> Vec<Recommendation> {
    let mut seen = HashSet::new();
    recs.into_iter()
        .filter_map(|mut rec| {
            rec.title = clean_title(&rec.title);
            let length = rec.title.chars().count();
            if !(2..=120).contains(&length) {
                return None;
            }
            rec.quote = rec
                .quote
                .map(|quote| trim_to(&quote, 320))
                .filter(|quote| !quote.is_empty());
            rec.why = rec
                .why
                .map(|why| trim_to(&why, 180))
                .filter(|why| !why.is_empty());
            rec.alt_titles = dedupe_names(rec.alt_titles);
            let key = normalize_title(&rec.title);
            (!key.is_empty() && seen.insert(key)).then_some(rec)
        })
        .take(MAX_RECOMMENDATIONS)
        .collect()
}

/// A keyless pass over the pasted text.
///
/// Recommendation threads are repetitive: bold titles, `Title by Author`,
/// `Title — reason`, a quoted title inside a sentence, or a bare title on its
/// own line. This finds those. Prose that merely mentions a series in passing
/// is left for the AI path — and a pattern match that no source recognises is
/// dropped after the lookup, so over-generating here is safe.
pub fn heuristic(text: &str) -> Vec<Recommendation> {
    let text = text.replace("&amp;", "&").replace('\u{200b}', "");
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(mut candidate) = candidate(line) else {
            continue;
        };
        if !seen.insert(normalize_title(&candidate.title)) {
            continue;
        }
        if candidate.quote.as_deref().is_none_or(str::is_empty) {
            candidate.quote = context(&lines, index);
        }
        found.push(candidate);
    }
    found.truncate(MAX_RECOMMENDATIONS);
    found
}

fn candidate(line: &str) -> Option<Recommendation> {
    candidate_at(line, 0)
}

fn candidate_at(line: &str, depth: usize) -> Option<Recommendation> {
    if depth > 2 {
        return None;
    }
    let line = clean_line(line);
    let length = line.chars().count();
    if !(3..=400).contains(&length) || line.contains("http") || line.contains("](") {
        return None;
    }
    let lower = line.to_lowercase();
    if COUNTER_EXAMPLES
        .iter()
        .any(|marker| lower.starts_with(marker))
    {
        return None;
    }

    if let Some(found) = bold_candidate(&line) {
        return Some(found);
    }
    if let Some(found) = quoted_candidate(&line) {
        return Some(found);
    }
    if let Some(found) = separated_candidate(&line, depth) {
        return Some(found);
    }
    if let Some(found) = bare_candidate(&line) {
        return Some(found);
    }
    // After a lead-in, trailing chatter is common: "I Have a Wife, sadly only
    // 10 chapters". Only split on a comma inside a line already being mined.
    if depth > 0 {
        if let Some((head, _)) = line.split_once(", ") {
            if head.split_whitespace().count() >= 3 {
                return bare_candidate(head);
            }
        }
    }
    None
}

fn bold_candidate(line: &str) -> Option<Recommendation> {
    let rest = line
        .strip_prefix("**")
        .or_else(|| line.strip_prefix("__"))?;
    let marker = &line[..2];
    let end = rest.find(marker)?;
    let title = rest[..end].trim();
    if !plausible(title) {
        return None;
    }
    let after = rest[end + marker.len()..].trim();
    let after = after.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '-' | '—' | '–' | ':' | '|')
    });
    Some(rec(clean_title(title), nonempty(trim_to(after, 240))))
}

/// A quoted title, anywhere in the line. Threads write
/// `I know you wrote MC, but "How Do We Relationship?" fits`.
fn quoted_candidate(line: &str) -> Option<Recommendation> {
    let mut chars = line.char_indices();
    let (start, quote) = chars.find(|(_, c)| *c == '"' || *c == '“')?;
    let rest = &line[start + quote.len_utf8()..];
    let end = rest.find(['"', '”'])?;
    let title = rest[..end].trim();
    if !plausible_quoted(title) {
        return None;
    }
    // A quote standing alone on its line is often the poster's own wording —
    // `"either side cheated"` — while a series is named rather than spoken.
    // Three lowercase words alone are prose, not a title.
    let whole_line = line
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '“' | '”'))
        .trim()
        == title;
    let lowercase = title
        .chars()
        .filter(|c| c.is_alphabetic())
        .all(|c| c.is_lowercase());
    if whole_line && lowercase && title.split_whitespace().count() <= 3 {
        return None;
    }
    Some(rec(clean_title(title), Some(trim_to(line, 240))))
}

fn separated_candidate(line: &str, depth: usize) -> Option<Recommendation> {
    for separator in [" by ", " — ", " – ", " - ", " | ", ": "] {
        let Some(position) = find_ci(line, separator) else {
            continue;
        };
        let left = line[..position].trim();
        let right = line[position + separator.len()..].trim();
        if LEAD_INS
            .iter()
            .any(|lead| left.to_lowercase().starts_with(lead))
        {
            if let Some(found) = candidate_at(right, depth + 1) {
                return Some(found);
            }
            continue;
        }
        if separator == ": " && !plausible(left) {
            continue;
        }
        let allowed = if separator == " by " { 6 } else { 14 };
        if plausible(left) && !right.is_empty() && right.split_whitespace().count() <= allowed {
            return Some(rec(clean_title(left), Some(trim_to(line, 240))));
        }
    }
    None
}

/// A line that is just a title, which is how most recommendation comments look.
/// Romanised Japanese titles carry lowercase particles (`Kimi no iru machi`), so
/// a single capitalised word is enough.
fn bare_candidate(line: &str) -> Option<Recommendation> {
    let words = line.split_whitespace().count();
    if !(1..=9).contains(&words) || !plausible(line) {
        return None;
    }
    if line
        .chars()
        .any(|c| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'))
    {
        return None;
    }
    if !title_like(line) {
        return None;
    }
    Some(rec(clean_title(line), None))
}

/// The prose under a bare title, which is the comment it came from.
fn context(lines: &[&str], index: usize) -> Option<String> {
    let mut parts = Vec::new();
    for line in lines.iter().skip(index + 1).take(3) {
        let line = clean_line(line);
        if line.is_empty() {
            continue;
        }
        if candidate(&line).is_some() {
            break;
        }
        if line.chars().count() < 18 {
            continue;
        }
        parts.push(trim_to(&line, 220));
        if parts.len() == 2 {
            break;
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Strips list markers and quote prefixes Reddit adds when text is copied.
fn clean_line(line: &str) -> String {
    let mut text = line.trim().trim_start_matches('>').trim_start().to_owned();
    for marker in ["- ", "* ", "• ", "+ "] {
        if let Some(rest) = text.strip_prefix(marker) {
            text = rest.trim_start().to_owned();
            break;
        }
    }
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        if let Some(rest) = text[digits..]
            .strip_prefix('.')
            .or_else(|| text[digits..].strip_prefix(')'))
        {
            text = rest.trim_start().to_owned();
        }
    }
    text.trim().to_owned()
}

/// Whether a run of text could be a series title rather than prose.
fn plausible(title: &str) -> bool {
    let title = title.trim();
    let words = title.split_whitespace().count();
    let chars = title.chars().count();
    if words == 0 || words > 10 || !(3..=90).contains(&chars) {
        return false;
    }
    if title
        .chars()
        .any(|c| matches!(c, '?' | '!' | '@' | '/' | '[' | ']' | '=' | '<' | '>'))
    {
        return false;
    }
    if title.ends_with(['.', ',', ';', ':']) {
        return false;
    }
    plausible_words(title)
}

/// Quoted titles may end in a question mark — *How Do We Relationship?* does.
fn plausible_quoted(title: &str) -> bool {
    let title = title.trim();
    let words = title.split_whitespace().count();
    let chars = title.chars().count();
    if !(2..=10).contains(&words) || !(3..=90).contains(&chars) {
        return false;
    }
    if title
        .chars()
        .any(|c| matches!(c, '!' | '@' | '/' | '[' | ']' | '=' | '<' | '>'))
    {
        return false;
    }
    plausible_words(title)
}

fn plausible_words(title: &str) -> bool {
    let lower = title.to_lowercase();
    if STOP_TITLES.contains(&lower.trim()) {
        return false;
    }
    !NOISE.iter().any(|noise| lower.contains(noise))
}

/// Bare lines are only trusted when they read like titles rather than a
/// sentence: at least one capitalised word, so `maybe very few` is not one.
fn title_like(title: &str) -> bool {
    let words: Vec<&str> = title.split_whitespace().collect();
    if let [word] = words.as_slice() {
        return word.chars().count() >= 5
            && !STOP_TITLES.contains(&word.to_lowercase().as_str())
            && word.chars().next().is_some_and(char::is_uppercase);
    }
    words
        .iter()
        .any(|word| word.chars().next().is_some_and(char::is_uppercase))
}

fn clean_title(title: &str) -> String {
    let cleaned = title
        .trim()
        .trim_matches(|c: char| {
            c.is_whitespace() || matches!(c, '*' | '_' | '"' | '“' | '”' | '\'')
        })
        .trim_end_matches(['.', ',', ':', ';'])
        .trim();
    match cleaned.strip_suffix(" ?") {
        Some(stem) => format!("{stem}?"),
        None => cleaned.to_owned(),
    }
}

fn dedupe_names(names: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    names
        .into_iter()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty() && seen.insert(normalize_title(name)))
        .collect()
}

fn rec(title: String, quote: Option<String>) -> Recommendation {
    Recommendation {
        title,
        quote,
        ..Default::default()
    }
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let lowered = haystack.to_ascii_lowercase();
    lowered
        .find(needle)
        .filter(|&index| haystack.is_char_boundary(index + needle.len()))
}

fn excerpt(thread: &str) -> String {
    let count = thread.chars().count();
    if count <= MAX_THREAD_CHARS {
        return thread.to_owned();
    }
    let mut cut = thread
        .char_indices()
        .nth(MAX_THREAD_CHARS)
        .map_or(thread.len(), |(index, _)| index);
    while cut > 0 && !thread.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n\n[the rest of the thread was left out]",
        &thread[..cut]
    )
}

fn trim_to(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max {
        return text;
    }
    let mut cut = text.chars().take(max.saturating_sub(1)).collect::<String>();
    cut.push('…');
    cut
}

/// Lowercase, punctuation-free form used for dedupe and matching.
pub fn normalize_title(title: &str) -> String {
    title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// How closely a source's title answers the title a thread named, from 0 to 1.
///
/// Exact and prefix matches are rewarded outright; otherwise coverage of the
/// query's words matters more than extra words in the candidate, because sources
/// decorate titles with volume numbers and imprints.
pub fn title_score(query: &str, candidate: &str) -> f32 {
    let query = normalize_title(query);
    let candidate = normalize_title(candidate);
    if query.is_empty() || candidate.is_empty() {
        return 0.0;
    }
    if query == candidate {
        return 1.0;
    }
    if candidate.starts_with(&format!("{query} ")) || candidate.ends_with(&format!(" {query}")) {
        return 0.92;
    }
    let query_words: HashSet<&str> = query.split(' ').collect();
    let candidate_words: HashSet<&str> = candidate.split(' ').collect();
    let shared = query_words.intersection(&candidate_words).count() as f32;
    if shared == 0.0 {
        return 0.0;
    }
    let coverage = shared / query_words.len() as f32;
    let precision = shared / candidate_words.len() as f32;
    (coverage * 0.75 + precision * 0.25).min(0.9)
}

/// Best score over every name the thread used and every name the source lists,
/// so a romanised or native title can match a differently-titled entry.
pub fn manga_score(names: &[&str], manga: &Manga) -> f32 {
    let mut titles: Vec<&str> = Vec::with_capacity(manga.alt_titles.len() + 1);
    titles.push(manga.title.as_str());
    titles.extend(manga.alt_titles.iter().map(String::as_str));
    let mut best: f32 = 0.0;
    for name in names {
        for title in &titles {
            best = best.max(title_score(name, title));
        }
    }
    best
}

const STOP_TITLES: [&str; 22] = [
    "manga",
    "manhwa",
    "manhua",
    "anime",
    "romance",
    "school",
    "comedy",
    "drama",
    "series",
    "chapter",
    "chapters",
    "completed",
    "ongoing",
    "finished",
    "reading",
    "update",
    "edit",
    "same",
    "this",
    "that",
    "also",
    "okay",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The comments from the thread this feature was built against, minus the
    /// usernames. Both junk candidates and the four titles the first version
    /// missed are here.
    const THREAD_COMMENTS: &str = r#""either side cheated"

But WAITTT, them moving away is too much for me

“Tsuzura-senpai is My Ex-Girlfriend” has been pretty good so far but it’s only started publishing this year so there aren’t many chapters yet.

It's on my list too! It looks cute but I don't think it'll hit what the OP wants, it kinda seems like "i broke up with you because I felt I wasn't good enough" that's the kinda vibe I get

I thought it fit because he seemed to not like the reason they broke up being external reasons.

Kimi no iru machi

from the top of my head: I Have A Rejuvenated Ex-wife In My Class, sadly only 10 chapters tho

I know you wrote MC but if youre not against Yuri, "how do we relationship ?" would be right your alley

maybe very few

dekinai futari - completed
yori kun to yori wo modosu wake ni wa - ongoing

both are manga with no anime adaptations though

"My Ex-Wife Is Young Again And She's In My Class"

Manga is finished with a conclusion, but I heard the Web novel is still going and avoided the Manga Ending.

My stepsister is my ex

Uhh, you recommended what he put as the cover image.

okay.. do you have any recommendations though"#;

    #[test]
    fn finds_every_title_the_real_thread_recommends() {
        let found = heuristic(THREAD_COMMENTS);
        let titles: Vec<&str> = found.iter().map(|rec| rec.title.as_str()).collect();
        for expected in [
            "Tsuzura-senpai is My Ex-Girlfriend",
            "Kimi no iru machi",
            "I Have A Rejuvenated Ex-wife In My Class",
            "how do we relationship?",
            "dekinai futari",
            "yori kun to yori wo modosu wake ni wa",
            "My Ex-Wife Is Young Again And She's In My Class",
            "My stepsister is my ex",
        ] {
            assert!(
                titles.contains(&expected),
                "missing {expected:?}; found {titles:?}"
            );
        }
        // The lead-in itself is not a title, and neither is the sentence.
        assert!(!titles.iter().any(|title| title.contains("top of my head")));
        assert!(!titles.iter().any(|title| title.starts_with("But WAITTT")));
        assert!(!titles
            .iter()
            .any(|title| title.starts_with("Manga is finished")));
    }

    #[test]
    fn keeps_the_comment_a_bare_title_came_from() {
        let found =
            heuristic("Somebody said\nSolo Leveling\nGreat art, and it is already complete.\n");
        let solo = found
            .iter()
            .find(|rec| rec.title == "Solo Leveling")
            .expect("the bare title should be found");
        assert!(solo
            .quote
            .as_deref()
            .is_some_and(|quote| quote.contains("already complete")));
    }

    #[test]
    fn does_not_treat_the_authors_counter_example_as_a_recommendation() {
        let found = heuristic(
            "Like for example: Harapeko Oyako to Motokare Yanushi\nI would like this story but i hate the mc.\n",
        );
        assert!(
            !found.iter().any(|rec| rec.title.contains("Harapeko")),
            "a counter-example is not a recommendation"
        );
    }

    #[test]
    fn reads_model_json_wrapped_in_prose_and_fences() {
        let reply = "Here you go:\n```json\n{\"recommendations\":[\
            {\"title\":\" My Ex-Wife Is Young Again And She's In My Class \",\
             \"alt_titles\":[\"My Ex-Wife Is Young Again\"],\"quote\":\"read this one last week\",\
             \"why\":\"same reunion premise\",\"confidence\":\"high\"},\
            {\"title\":\"My Ex-Wife Is Young Again And She's In My Class\"}]}\n```";
        let recs = parse_model_json(reply).unwrap();
        assert_eq!(recs.len(), 1, "duplicates collapse");
        assert_eq!(
            recs[0].title,
            "My Ex-Wife Is Young Again And She's In My Class"
        );
        assert_eq!(recs[0].why.as_deref(), Some("same reunion premise"));
        assert_eq!(recs[0].alt_titles.len(), 1, "every alternate name is kept");
    }

    #[test]
    fn rejects_a_reply_without_json() {
        assert!(parse_model_json("no titles today").is_err());
    }

    #[test]
    fn scores_an_exact_title_above_a_word_in_common() {
        assert_eq!(title_score("Horimiya", "Horimiya"), 1.0);
        assert!(title_score("Horimiya", "Horimiya (Official)") > 0.9);
        assert!(
            title_score(
                "My Ex-Wife Is Young Again And She's In My Class",
                "My Ex-Wife"
            ) > title_score("Horimiya", "Aku no Kyouten")
        );
        assert_eq!(title_score("Horimiya", "Aku no Kyouten"), 0.0);
    }

    #[test]
    fn a_native_alternate_name_can_carry_the_match() {
        let mut manga = Manga::new("/manga/118", "Solo Leveling");
        manga.alt_titles = vec![
            "나 혼자만 레벨업".to_owned(),
            "Na Honjaman Level Up".to_owned(),
        ];
        let names = ["Na Honjaman Level Up"];
        assert_eq!(manga_score(&names, &manga), 1.0);
    }

    #[test]
    fn falls_back_without_a_key() {
        let settings = AiSettings::default();
        if !settings.configured() {
            assert!(chat(&settings, "anything").is_err());
        }
    }
}
