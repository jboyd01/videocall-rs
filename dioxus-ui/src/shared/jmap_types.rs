use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FileInfo {
    pub mimetype: Option<String>,
    pub size: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ImageInfo {
    pub mimetype: Option<String>,
    pub size: Option<u64>,
    pub w: Option<u32>,
    pub h: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum MessageContent {
    Text {
        body: String,
        formatted_body: Option<String>,
    },
    Image {
        url: String,
        body: String,
        info: Option<ImageInfo>,
    },
    File {
        url: String,
        body: String,
        filename: String,
        info: Option<FileInfo>,
    },
    Notice {
        body: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub blob_id: String,
    pub name: String,
    pub content_type: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted_text: Option<String>, // Extracted text from document for search indexing
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted_files: Option<Vec<serde_json::Value>>, // Extracted files array for compressed archives
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_url: Option<String>, // Data URL for local storage mode (base64 encoded)
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Mentions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_ids: Option<HashMap<String, bool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_ids: Option<HashMap<String, bool>>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum MessageType {
    Text,
    Image,
    File,
    Notice,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: String,
    pub room_id: String,
    pub sender: String,
    pub content: MessageContent,
    pub timestamp: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub reply_to: Option<String>,
    pub reactions: HashMap<String, Vec<String>>,
    pub message_type: MessageType,
    pub attachments: Option<Vec<Attachment>>,
    pub mentions: Option<Mentions>,
    pub thread_id: Option<String>,
    pub thread_message_count: Option<u32>,
    pub thread_unread_count: Option<u32>,
    pub is_pinned: bool,
}
#[derive(Clone, PartialEq, Debug)]
pub enum ServiceResult<T, E>
where
    T: Clone + PartialEq,
    E: Clone + PartialEq,
{
    /// The initial state before any request has been made.
    Initial,
    /// The request is currently in progress.
    Loading,
    /// The request completed successfully and holds the data.
    Success(T),
    /// The request failed and holds an error.
    Error(E),
}

// A specific error type for our service.
#[derive(Clone, PartialEq, Debug)]
pub enum ServiceError {
    NetworkError(String),
    ParseError(String),
    NotFound,
    Unknown(String),
}
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JmapRequest {
    pub using: Vec<String>,
    pub method_calls: Vec<(String, Value, String)>,
}
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JmapResponse {
    pub method_responses: Vec<(String, Value, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_ids: Option<HashMap<String, String>>,
    pub session_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_found: Option<Vec<String>>,
}
