use axum::body;
use axum::body::Body;
use axum::http::HeaderMap;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header;
use axum::http::header::HeaderValue;
use axum::response::Response;

use crate::header_policy;

const MAX_BODY_BYTES: i64 = 10 * 1024 * 1024;

pub(crate) async fn forward(
    http: &reqwest::Client,
    upstream_base_url: &str,
    request: Request<Body>,
    authorization: &str,
    chatgpt_account_id: Option<&str>,
) -> Result<Response, StatusCode> {
    let (parts, body) = request.into_parts();
    let wants_event_stream = request_accepts_event_stream(&parts.headers);

    let path_and_query = parts
        .uri
        .path_and_query()
        .map(axum::http::uri::PathAndQuery::as_str)
        .unwrap_or_else(|| parts.uri.path());
    let base = upstream_base_url.trim().trim_end_matches('/');
    let upstream_url = format!("{base}{path_and_query}");

    let limit = match usize::try_from(MAX_BODY_BYTES) {
        Ok(value) => value,
        Err(_) => usize::MAX,
    };
    let body_bytes = body::to_bytes(body, limit)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let mut headers = header_policy::forward_request_headers(&parts.headers);
    let auth =
        HeaderValue::from_str(authorization).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    headers.insert(header::AUTHORIZATION, auth);
    if let Some(chatgpt_account_id) = chatgpt_account_id {
        let account_id = HeaderValue::from_str(chatgpt_account_id)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let _ = headers.insert("ChatGPT-Account-ID", account_id);
    }

    let response = http
        .request(parts.method, upstream_url)
        .headers(headers)
        .body(body_bytes)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let status = response.status();
    let headers = header_policy::forward_response_headers(response.headers());
    let body = if wants_event_stream {
        Body::from_stream(response.bytes_stream())
    } else {
        let response_body = response
            .bytes()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        Body::from(response_body)
    };

    let mut out = Response::new(body);
    *out.status_mut() = status;
    out.headers_mut().extend(headers);
    Ok(out)
}

fn request_accepts_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"))
}
