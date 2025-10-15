use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub role: MessageRole,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<OffsetDateTime>,
    #[serde(skip_serializing)]
    pub full_text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub uuid: String,
    pub label: String,
    #[serde(skip_serializing)]
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<OffsetDateTime>,
    pub updated_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_message_time: Option<OffsetDateTime>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing)]
    pub search_blob: String,
}

impl Session {
    pub fn preview(&self) -> Option<&Message> {
        self.messages.first()
    }

    pub fn title(&self) -> &str {
        &self.label
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SnippetSegment {
    pub text: String,
    pub highlighted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snippet {
    pub segments: Vec<SnippetSegment>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub session: Arc<Session>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_message: Option<Message>,
    pub score: i64,
    pub snippet: Snippet,
}

impl SearchResult {
    pub fn match_timestamp(&self) -> OffsetDateTime {
        self.matched_message
            .as_ref()
            .and_then(|m| m.timestamp)
            .or(self.session.latest_message_time)
            .unwrap_or(self.session.updated_at)
    }
}
