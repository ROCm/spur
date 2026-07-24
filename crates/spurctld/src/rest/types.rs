// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use axum::http::StatusCode;
use axum::response::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize)]
pub struct ApiResponse<T: Serialize> {
    pub meta: ApiMeta,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ApiError>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(flatten)]
    pub data: T,
}

#[derive(Serialize)]
pub struct ApiMeta {
    #[serde(rename = "Slurm")]
    pub slurm: ApiVersion,
}

#[derive(Serialize)]
pub struct ApiVersion {
    pub version: ApiVersionInfo,
    pub release: String,
}

#[derive(Serialize)]
pub struct ApiVersionInfo {
    pub major: u32,
    pub minor: u32,
    pub micro: u32,
}

#[derive(Serialize)]
pub struct ApiError {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_number: Option<i32>,
}

pub fn meta() -> ApiMeta {
    ApiMeta {
        slurm: ApiVersion {
            version: ApiVersionInfo {
                major: 0,
                minor: 0,
                micro: 42,
            },
            release: "spur 0.1.0".into(),
        },
    }
}

impl<T: Serialize> ApiResponse<T> {
    pub fn ok(data: T) -> Json<Self> {
        Json(Self {
            meta: meta(),
            errors: Vec::new(),
            warnings: Vec::new(),
            data,
        })
    }
}

pub type RestError = (StatusCode, Json<ApiResponse<serde_json::Value>>);

pub fn error_response(msg: &str) -> RestError {
    api_error_response(StatusCode::INTERNAL_SERVER_ERROR, msg)
}

pub fn bad_request_response(msg: &str) -> RestError {
    api_error_response(StatusCode::BAD_REQUEST, msg)
}

pub fn not_found_response(msg: &str) -> RestError {
    api_error_response(StatusCode::NOT_FOUND, msg)
}

pub fn unavailable_response(msg: &str) -> RestError {
    api_error_response(StatusCode::SERVICE_UNAVAILABLE, msg)
}

pub fn api_error_response(status: StatusCode, msg: &str) -> RestError {
    (
        status,
        Json(ApiResponse {
            meta: meta(),
            errors: vec![ApiError {
                error: msg.to_string(),
                error_number: None,
            }],
            warnings: Vec::new(),
            data: serde_json::json!({}),
        }),
    )
}

// -- Request/response data types --

#[derive(Serialize)]
pub struct PingData {
    pub ping: Vec<PingInfo>,
}

#[derive(Serialize)]
pub struct PingInfo {
    pub hostname: String,
    pub pinged: String,
    pub latency: u64,
    pub mode: String,
}

#[derive(Deserialize)]
pub struct JobsQuery {
    pub user: Option<String>,
    pub partition: Option<String>,
    pub state: Option<String>,
    pub account: Option<String>,
    pub name: Option<String>,
}

#[derive(Serialize)]
pub struct JobsData {
    pub jobs: Vec<serde_json::Value>,
}

#[derive(Serialize)]
pub struct NodesData {
    pub nodes: Vec<serde_json::Value>,
}

#[derive(Serialize)]
pub struct PartitionsData {
    pub partitions: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct SubmitRequest {
    pub job: SubmitJobFields,
}

#[derive(Deserialize)]
pub struct SubmitJobFields {
    pub name: Option<String>,
    pub user: Option<String>,
    pub partition: Option<String>,
    pub account: Option<String>,
    pub nodes: Option<u32>,
    pub ntasks: Option<u32>,
    pub cpus_per_task: Option<u32>,
    pub time_limit: Option<String>,
    pub script: Option<String>,
    #[serde(default)]
    pub environment: HashMap<String, String>,
    #[serde(default)]
    pub gres: Vec<String>,
    /// GPU requests ("4" or "mi300x:4"); at most one may be set.
    pub gpus: Option<String>,
    pub gpus_per_node: Option<String>,
    pub gpus_per_task: Option<String>,
}

#[derive(Serialize)]
pub struct SubmitResponse {
    pub job_id: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_job_fields_deserialize_user() {
        let body: SubmitRequest =
            serde_json::from_str(r#"{"job":{"user":"alice","account":"research"}}"#).unwrap();
        assert_eq!(body.job.user.as_deref(), Some("alice"));
        assert_eq!(body.job.account.as_deref(), Some("research"));
    }
}
