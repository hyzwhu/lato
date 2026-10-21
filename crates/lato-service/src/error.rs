use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug)]
pub(crate) struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn missing() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "service.not_found",
            "resource not found",
        )
    }

    pub fn busy() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "service.session_busy",
            "session already has an active operation or is closing",
        )
    }

    pub fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service.stopping",
            "service is shutting down",
        )
    }

    pub fn invalid_json(status: StatusCode) -> Self {
        Self::new(
            status,
            "service.invalid_request",
            "invalid JSON request or request body exceeds the limit",
        )
    }

    pub fn runtime(error: impl std::fmt::Display) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "service.agent_error",
            error.to_string(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"apiVersion":"v1", "error":{"code":self.code,"message":self.message}})),
        )
            .into_response()
    }
}
