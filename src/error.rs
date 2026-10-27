//! Error types for JSON endpoints and HTML pages.

use askama::Template;
use askama_web::WebTemplate;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// JSON error returned by `/api/*` and `/model/*`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "Not signed in.".to_string(),
        }
    }

    pub fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: "Not found.".to_string(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub fn unprocessable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: message.into(),
        }
    }

    pub fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "Something went wrong.".to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "error.html")]
pub struct ErrorTemplate {
    pub status: u16,
    pub message: &'static str,
}

/// HTML error page, rendered with the site shell.
#[derive(Debug)]
pub struct PageError {
    status: StatusCode,
    message: &'static str,
}

impl PageError {
    pub fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: "Page not found.",
        }
    }

    pub fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "Something went wrong.",
        }
    }
}

impl IntoResponse for PageError {
    fn into_response(self) -> Response {
        let page = ErrorTemplate {
            status: self.status.as_u16(),
            message: self.message,
        };
        (self.status, page).into_response()
    }
}
