use std::cmp::Ordering;
use std::sync::Arc;

use anyhow::Result;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use time::OffsetDateTime;

use crate::session::{Message, SearchResult, Session, Snippet, SnippetSegment};

const RECENCY_BASE: i64 = 50_000;
const RECENCY_MAX_PENALTY: i64 = 45_000;
const SNIPPET_CONTEXT_CHARS: usize = 60;

pub struct Scorer {
    matcher: SkimMatcherV2,
    query: String,
    query_lower: String,
    now: OffsetDateTime,
    is_empty_query: bool,
}

impl Scorer {
    pub fn new(query: &str) -> Self {
        let trimmed = query.trim().to_owned();
        let query_lower = trimmed.to_lowercase();
        let is_empty_query = trimmed.is_empty();
        let matcher = SkimMatcherV2::default()
            .ignore_case()
            .use_cache(true)
            .smart_case();

        Self {
            matcher,
            query: trimmed,
            query_lower,
            now: OffsetDateTime::now_utc(),
            is_empty_query,
        }
    }

    pub fn score_session_arc(&mut self, session: Arc<Session>) -> Option<SearchResult> {
        self.score_session(session.as_ref())
            .map(|(score, matched_message, snippet)| SearchResult {
                session,
                matched_message,
                score,
                snippet,
            })
    }

    pub fn score_session(&mut self, session: &Session) -> Option<(i64, Option<Message>, Snippet)> {
        if self.is_empty_query {
            let anchor = session.latest_message_time.unwrap_or(session.updated_at);
            let score = recency_bonus(anchor, self.now);
            let preview = session.preview().cloned();
            let source = preview
                .as_ref()
                .map(|m| m.full_text.as_str())
                .unwrap_or_else(|| session.label.as_str());
            let snippet = snippet_from_text(source, "", SNIPPET_CONTEXT_CHARS);
            return Some((score, preview, snippet));
        }

        let blob_score = self.matcher.fuzzy_match(&session.search_blob, &self.query);
        let label_score = self.matcher.fuzzy_match(&session.label, &self.query);
        let uuid_score = self.matcher.fuzzy_match(&session.uuid, &self.query);

        let blob_lower = session.search_blob.to_lowercase();
        let label_lower = session.label.to_lowercase();
        let uuid_lower = session.uuid.to_lowercase();

        let matches_text = blob_lower.contains(&self.query_lower)
            || label_lower.contains(&self.query_lower)
            || uuid_lower.contains(&self.query_lower);

        if blob_score.is_none() && label_score.is_none() && uuid_score.is_none() && !matches_text {
            return None;
        }

        let (best_message, best_message_score) =
            best_message_for_session(&mut self.matcher, session, &self.query, &self.query_lower);

        let snippet = if let Some(ref message) = best_message {
            snippet_from_text(&message.full_text, &self.query_lower, SNIPPET_CONTEXT_CHARS)
        } else if label_lower.contains(&self.query_lower) {
            snippet_from_text(&session.label, &self.query_lower, SNIPPET_CONTEXT_CHARS)
        } else if blob_lower.contains(&self.query_lower) {
            snippet_from_text(
                &session.search_blob,
                &self.query_lower,
                SNIPPET_CONTEXT_CHARS,
            )
        } else {
            snippet_from_text(&session.label, &self.query_lower, SNIPPET_CONTEXT_CHARS)
        };

        let mut score = 0i64;
        score += blob_score.unwrap_or(0) as i64 * 2;
        score += label_score.unwrap_or(0) as i64 * 3;
        score += uuid_score.unwrap_or(0) as i64;

        if matches_text {
            score += 10_000;
        }

        if best_message_score > 0 {
            score += best_message_score as i64;
        }

        let anchor = best_message
            .as_ref()
            .and_then(|m| m.timestamp)
            .or(session.latest_message_time)
            .unwrap_or(session.updated_at);

        score += recency_bonus(anchor, self.now);

        Some((score, best_message, snippet))
    }

    pub fn is_query_empty(&self) -> bool {
        self.is_empty_query
    }
}

pub fn search_sessions(
    sessions: &[Session],
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let mut ordered: Vec<&Session> = sessions.iter().collect();
    ordered.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    let mut scorer = Scorer::new(query);

    let mut scored: Vec<SearchResult> = ordered
        .into_iter()
        .filter_map(|session| {
            let arc = Arc::new(session.clone());
            scorer.score_session_arc(arc)
        })
        .collect();

    scored.sort_by(|a, b| match b.match_timestamp().cmp(&a.match_timestamp()) {
        Ordering::Equal => b.score.cmp(&a.score),
        other => other,
    });

    scored.truncate(limit);
    Ok(scored)
}

pub fn recency_bonus(updated_at: OffsetDateTime, now: OffsetDateTime) -> i64 {
    let age = now - updated_at;
    let minutes = age.whole_minutes().clamp(0, RECENCY_MAX_PENALTY);
    RECENCY_BASE - minutes
}

fn best_message_for_session(
    matcher: &mut SkimMatcherV2,
    session: &Session,
    query: &str,
    query_lower: &str,
) -> (Option<Message>, i64) {
    let mut best_message = None;
    let mut best_score: i64 = i64::MIN;

    for message in &session.messages {
        let fuzzy = matcher.fuzzy_match(&message.full_text, query).unwrap_or(0) as i64;
        let contains = message.full_text.to_lowercase().contains(query_lower);
        let total = fuzzy + if contains { 6_000 } else { 0 };
        if total > best_score {
            best_score = total;
            best_message = Some(message.clone());
        }
    }

    if best_score > 0 {
        (best_message, best_score)
    } else {
        (best_message, 0)
    }
}

fn snippet_from_text(text: &str, query_lower: &str, context: usize) -> Snippet {
    if text.is_empty() {
        return Snippet {
            segments: vec![SnippetSegment {
                text: String::new(),
                highlighted: false,
            }],
        };
    }

    if query_lower.is_empty() {
        let snippet: String = text.chars().take(context * 2).collect();
        let snippet = normalize_snippet_text(&snippet).trim().to_string();
        return Snippet {
            segments: vec![SnippetSegment {
                text: snippet,
                highlighted: false,
            }],
        };
    }

    let text_chars: Vec<char> = text.chars().collect();
    let lowercase = text.to_lowercase();
    let pos = lowercase.find(query_lower);

    let (start_char, end_char) = match pos {
        Some(byte_idx) => {
            let start_char = lowercase[..byte_idx].chars().count();
            let match_len = query_lower.chars().count();
            let end_char = (start_char + match_len).min(text_chars.len());
            (start_char, end_char)
        }
        None => {
            let snippet: String = text_chars.iter().take(context * 2).collect();
            let snippet = normalize_snippet_text(&snippet).trim().to_string();
            return Snippet {
                segments: vec![SnippetSegment {
                    text: snippet,
                    highlighted: false,
                }],
            };
        }
    };

    let available_left = start_char;
    let available_right = text_chars.len() - end_char;

    let mut left_take = context.min(available_left);
    let mut right_take = context.min(available_right);

    if left_take < context {
        let redistribute = context - left_take;
        right_take = (right_take + redistribute).min(available_right);
    }

    if right_take < context {
        let redistribute = context - right_take;
        left_take = (left_take + redistribute).min(available_left);
    }

    let start_snip = start_char.saturating_sub(left_take);
    let end_snip = (end_char + right_take).min(text_chars.len());

    let mut segments = Vec::new();

    if start_snip > 0 {
        segments.push(SnippetSegment {
            text: "…".to_string(),
            highlighted: false,
        });
    }

    if start_snip < start_char {
        let segment: String = text_chars[start_snip..start_char].iter().collect();
        segments.push(SnippetSegment {
            text: normalize_snippet_text(&segment),
            highlighted: false,
        });
    }

    let matched: String = text_chars[start_char..end_char].iter().collect();
    segments.push(SnippetSegment {
        text: normalize_snippet_text(&matched).trim().to_string(),
        highlighted: true,
    });

    if end_char < end_snip {
        let segment: String = text_chars[end_char..end_snip].iter().collect();
        segments.push(SnippetSegment {
            text: normalize_snippet_text(&segment),
            highlighted: false,
        });
    }

    if end_snip < text_chars.len() {
        segments.push(SnippetSegment {
            text: "…".to_string(),
            highlighted: false,
        });
    }

    Snippet { segments }
}

fn normalize_snippet_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    let mut normalized = String::with_capacity(text.len());
    let mut last_was_space = false;

    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                normalized.push(' ');
                last_was_space = true;
            }
        } else {
            normalized.push(ch);
            last_was_space = false;
        }
    }

    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_with_match_normalizes_whitespace() {
        let text = "alpha\nbeta   gamma\t\ndelta";
        let snippet = snippet_from_text(text, "beta", 30);
        let combined: String = snippet
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect();

        assert_eq!(combined, "alpha beta gamma delta");
        assert!(
            snippet
                .segments
                .iter()
                .any(|segment| segment.highlighted && segment.text == "beta")
        );
    }

    #[test]
    fn snippet_without_match_normalizes_whitespace() {
        let text = "foo\n\nbar\tbaz";
        let snippet = snippet_from_text(text, "", 20);

        assert_eq!(snippet.segments.len(), 1);
        assert_eq!(snippet.segments[0].text, "foo bar baz");
        assert!(!snippet.segments[0].highlighted);
    }

    #[test]
    fn snippet_balances_context_when_room_on_both_sides() {
        let prefix = "a".repeat(80);
        let suffix = "b".repeat(80);
        let text = format!("{prefix}match{suffix}");
        let snippet = snippet_from_text(&text, "match", 20);

        let combined: String = snippet
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect();

        let match_idx = combined.find("match").unwrap();
        let total_len = combined.len();

        assert!(match_idx > total_len / 4, "match too close to start");
        assert!(match_idx < (total_len * 3) / 4, "match too close to end");
    }
}
