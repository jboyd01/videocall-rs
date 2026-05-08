// use serde::{Deserialize, Serialize};
// use chrono::{DateTime, Utc};
// use std::collections::HashMap;
// #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
// pub struct FileInfo {
//     pub mimetype: Option<String>,
//     pub size: Option<u64>,
// }
// #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
// pub struct ImageInfo {
//     pub mimetype: Option<String>,
//     pub size: Option<u64>,
//     pub w: Option<u32>,
//     pub h: Option<u32>,
// }
// #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
// pub enum MessageContent {
//     Text {
//         body: String,
//         formatted_body: Option<String>,
//     },
//     Image {
//         url: String,
//         body: String,
//         info: Option<ImageInfo>,
//     },
//     File {
//         url: String,
//         body: String,
//         filename: String,
//         info: Option<FileInfo>,
//     },
//     Notice {
//         body: String,
//     },
// }
// #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
// pub struct Attachment {
//     pub blob_id: String,
//     pub name: String,
//     pub content_type: String,
//     pub size: u64,
//     #[serde(skip_serializing_if = "Option::is_none")]
//     pub extracted_text: Option<String>, // Extracted text from document for search indexing
//     #[serde(skip_serializing_if = "Option::is_none")]
//     pub extracted_files: Option<Vec<serde_json::Value>>, // Extracted files array for compressed archives
//     #[serde(skip_serializing_if = "Option::is_none")]
//     pub data_url: Option<String>, // Data URL for local storage mode (base64 encoded)
// }
// #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
// pub struct Mentions {
//     #[serde(skip_serializing_if = "Option::is_none")]
//     pub user_ids: Option<HashMap<String, bool>>,
//     #[serde(skip_serializing_if = "Option::is_none")]
//     pub group_ids: Option<HashMap<String, bool>>,
// }
// #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
// pub enum MessageType {
//     Text,
//     Image,
//     File,
//     Notice,
// }
// #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
// pub struct Message {
//     pub id: String,
//     pub room_id: String,
//     pub sender: String,
//     pub content: MessageContent,
//     pub timestamp: DateTime<Utc>,
//     pub edited_at: Option<DateTime<Utc>>,
//     pub reply_to: Option<String>,
//     pub reactions: HashMap<String, Vec<String>>,
//     pub message_type: MessageType,
//     pub attachments: Option<Vec<Attachment>>,
//     pub mentions: Option<Mentions>,
//     pub thread_id: Option<String>,
//     pub thread_message_count: Option<u32>,
//     pub thread_unread_count: Option<u32>,
//     pub is_pinned: bool,
// }
// #[derive(Clone, PartialEq, Debug)]
// pub enum ServiceResult<T, E>
// where
//     T: Clone + PartialEq,
//     E: Clone + PartialEq,
// {
//     /// The initial state before any request has been made.
//     Initial,
//     /// The request is currently in progress.
//     Loading,
//     /// The request completed successfully and holds the data.
//     Success(T),
//     /// The request failed and holds an error.
//     Error(E),
// }
//
// /// A specific error type for our service.
#[derive(Clone, PartialEq, Debug)]
pub enum ServiceError {
    NetworkError(String),
    ParseError(String),
    NotFound,
    Unknown(String),
}
use serde_json::json;
use reqwest;

pub async fn get_messages() -> Result<Vec<serde_json::Value>, ServiceError> {
    let url = "https://127.0.0.1:8443/jmap";
    let token="Bearer eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1c2VyLWYyMDRkNWViLTcwMzEtNGJkMy1hMGI4LTI5NjBiMGI1ODkzZSIsInByZWZlcnJlZF91c2VybmFtZSI6ImFkbWluIiwiZXhwIjoxNzc4MjI0NTk2LCJpYXQiOjE3NzgxMzgxOTYsImlzcyI6IjAuMC4wLjA6ODQ0MyIsImVtYWlsIjpudWxsLCJuYW1lIjpudWxsLCJ0ZW5hbnRfaWQiOm51bGwsImF1dGhfdHlwZSI6InVzZXIiLCJqdGkiOm51bGwsImd1ZXN0X2NvbnZlcnNhdGlvbl9pZCI6bnVsbCwiaW52aXRlX2lkIjpudWxsfQ.ILTRBNir_zOoPLTcOHjuVKCI4fJtXu6c9tIF_MBvImk";
    let payload = json!({
        "using": ("urn:ietf:params:jmap:core", "urn:ietf:params:jmap:chat"),
        "methodCalls": ((
                "ChatMessage/query",
                json!({
                    "accountId": "acc1",
                    "conversationId": "conv-bc476382-e85a-4a79-991f-ff58cb5f7548",
                    "limit": 20,
                    "position": -1,
                    "preview": false
                }),
                "0"
            ), (
                "ChatMessage/get",
                json!({
                    "#ids": json!({
                        "name": "ChatMessage/query",
                        "path": "/ids",
                        "resultOf": "0"
                    }),
                    "accountId": "acc1"
                }),
                "1"
            ))
    });

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("accept", "*/*".parse().unwrap());
    headers.insert("authorization", token.parse().unwrap());

    let client = reqwest::Client::new();
    let response = client
        .post(url)
        .headers(headers)
        .json(&payload)
        .send()
        .await
        .map_err(|e| ServiceError::NetworkError(e.to_string()))?;

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| ServiceError::ParseError(e.to_string()))?;

    let list = body["methodResponses"][1][1]["list"]
        .as_array()
        .ok_or_else(|| ServiceError::ParseError("missing list field".to_string()))?;

    let messages = list.to_vec();

    Ok(messages)
}
