//! Valolysis HTTP API client.

use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use ureq::Agent;

const USER_AGENT: &str = concat!("valolysis-desktop/", env!("CARGO_PKG_VERSION"));
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
pub enum ApiError {
    /// The session is missing, expired or revoked.
    Unauthorized,
    /// Quota or rate limit; retry later.
    RateLimited(String),
    /// The server refused this request permanently (validation, size, ...).
    Rejected(u16, String),
    /// Network or server trouble; retry with backoff.
    Transient(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "sign-in required"),
            Self::RateLimited(message) => write!(f, "{message}"),
            Self::Rejected(status, message) => write!(f, "{message} (HTTP {status})"),
            Self::Transient(message) => write!(f, "{message}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, ApiError>;

#[derive(Clone, Debug, Deserialize)]
pub struct User {
    pub email: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Profile {
    pub user: User,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DesktopSession {
    pub token: String,
    pub user: User,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedUpload {
    pub job_id: String,
    pub upload_id: String,
    pub part_size: u64,
    pub part_count: u32,
    /// Presigned URLs for uploading parts straight to R2
    #[serde(default)]
    pub parts: Option<Vec<DirectPart>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectPart {
    pub part_number: u32,
    pub size: u64,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub status: String,
    pub error_code: Option<String>,
    pub rounds: Option<u64>,
    pub kills: Option<u64>,
    /// `Some(false)` for unrated and other non-competitive games.
    #[serde(default)]
    pub competitive: Option<bool>,
}

pub struct UploadRequest<'a> {
    pub size: u64,
    pub sha256: &'a str,
    pub source_replay_id: Option<&'a str>,
    pub redact_names: bool,
    pub redact_pids: bool,
    pub publish: bool,
}

pub struct Api {
    base: String,
    agent: Agent,
}

enum Payload<'a> {
    None,
    Json(Value),
    Bytes(&'a [u8]),
}

impl Api {
    pub fn new(base: &str) -> Self {
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(300)))
            .user_agent(USER_AGENT)
            .build()
            .into();
        Self {
            base: base.to_owned(),
            agent,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        payload: Payload<'_>,
    ) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let authorization = token.map(|token| format!("Bearer {token}"));
        macro_rules! with_auth {
            ($builder:expr) => {{
                let builder = $builder.header("accept", "application/json");
                match &authorization {
                    Some(value) => builder.header("authorization", value.as_str()),
                    None => builder,
                }
            }};
        }
        let result = match (method, payload) {
            ("GET", _) => with_auth!(self.agent.get(&url)).call(),
            ("DELETE", _) => with_auth!(self.agent.delete(&url)).call(),
            ("POST", Payload::Json(body)) => with_auth!(self.agent.post(&url))
                .content_type("application/json")
                .send(body.to_string()),
            ("POST", _) => with_auth!(self.agent.post(&url)).send_empty(),
            ("PUT", Payload::Bytes(bytes)) => with_auth!(self.agent.put(&url))
                .content_type("application/octet-stream")
                .send(bytes),
            _ => unreachable!("unsupported request shape"),
        };
        let mut response =
            result.map_err(|error| ApiError::Transient(format!("network error: {error}")))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_string()
            .unwrap_or_default();
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let message = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unexpected response")
            .to_owned();
        match status {
            200..=299 => Ok(value),
            401 => Err(ApiError::Unauthorized),
            429 => Err(ApiError::RateLimited(message)),
            400..=499 => Err(ApiError::Rejected(status, message)),
            _ => Err(ApiError::Transient(format!(
                "server error {status}: {message}"
            ))),
        }
    }

    fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
        serde_json::from_value(value)
            .map_err(|error| ApiError::Transient(format!("unexpected response: {error}")))
    }

    /// Redeems the one-time code from the browser sign-in with the PKCE verifier.
    pub fn desktop_token(&self, code: &str, verifier: &str) -> Result<DesktopSession> {
        Self::parse(self.call(
            "POST",
            "/v1/auth/desktop/token",
            None,
            Payload::Json(json!({ "code": code, "codeVerifier": verifier })),
        )?)
    }

    pub fn profile(&self, token: &str) -> Result<Profile> {
        Self::parse(self.call("GET", "/v1/auth/me", Some(token), Payload::None)?)
    }

    pub fn logout(&self, token: &str) -> Result<()> {
        self.call("POST", "/v1/auth/logout", Some(token), Payload::None)
            .map(|_| ())
    }

    pub fn create_upload(&self, token: &str, request: &UploadRequest<'_>) -> Result<CreatedUpload> {
        let mut body = json!({
            "sourceSizeBytes": request.size,
            "sourceSha256": request.sha256,
            "redactNames": request.redact_names,
            "redactPids": request.redact_pids,
            "publish": request.publish,
        });
        if let Some(id) = request.source_replay_id {
            body["sourceReplayId"] = Value::String(id.to_owned());
        }
        Self::parse(self.call("POST", "/v1/uploads", Some(token), Payload::Json(body))?)
    }

    pub fn upload_part(&self, token: &str, upload_id: &str, part: u32, bytes: &[u8]) -> Result<()> {
        let path = format!("/v1/uploads/parts/{part}?uploadId={}", encode(upload_id));
        self.call("PUT", &path, Some(token), Payload::Bytes(bytes))
            .map(|_| ())
    }

    /// Completes an upload. Direct uploads pass the `(part number, ETag)`
    /// pairs R2 returned; proxied uploads pass none.
    pub fn complete_upload(
        &self,
        token: &str,
        upload_id: &str,
        parts: &[(u32, String)],
    ) -> Result<()> {
        let mut body = json!({ "uploadId": upload_id });
        if !parts.is_empty() {
            body["parts"] = parts
                .iter()
                .map(|(number, etag)| json!({ "partNumber": number, "etag": etag }))
                .collect();
        }
        self.call(
            "POST",
            "/v1/uploads/complete",
            Some(token),
            Payload::Json(body),
        )
        .map(|_| ())
    }

    /// PUTs one part to its presigned R2 URL and returns the ETag. The URL
    /// carries its own authorization, so no session token is sent.
    pub fn upload_direct_part(&self, url: &str, bytes: &[u8]) -> Result<String> {
        let mut response = self
            .agent
            .put(url)
            .send(bytes)
            .map_err(|error| ApiError::Transient(format!("network error: {error}")))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let detail = response
                .body_mut()
                .with_config()
                .limit(4096)
                .read_to_string()
                .unwrap_or_default();
            // Expired or rejected presigned URLs are retried with fresh ones.
            return Err(ApiError::Transient(format!(
                "storage rejected part (HTTP {status}): {}",
                detail.chars().take(200).collect::<String>()
            )));
        }
        response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(|etag| etag.trim_matches('"').to_owned())
            .ok_or_else(|| ApiError::Transient("storage returned no ETag".into()))
    }

    pub fn abort_upload(&self, token: &str, upload_id: &str) -> Result<()> {
        let path = format!("/v1/uploads?uploadId={}", encode(upload_id));
        self.call("DELETE", &path, Some(token), Payload::None)
            .map(|_| ())
    }

    pub fn job(&self, token: &str, job_id: &str) -> Result<Job> {
        Self::parse(self.call(
            "GET",
            &format!("/v1/replays/{}", encode(job_id)),
            Some(token),
            Payload::None,
        )?)
    }
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
pub fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::encode;

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode("a b/c?d=e"), "a%20b%2Fc%3Fd%3De");
        assert_eq!(encode("0f1e-AZ_~."), "0f1e-AZ_~.");
    }
}
