use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

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
