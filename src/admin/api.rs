//! Isolated admin routes, body limits, Origin/CSRF guards and security headers.

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{
        ConnectInfo, DefaultBodyLimit, Path, Request, State,
        rejection::{JsonRejection, PathRejection},
    },
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{CONTENT_TYPE, SET_COOKIE},
    },
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{any, get, post},
};
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::Duration,
};
use uuid::Uuid;

use super::{
    AdminError, AdminService,
    ceremony::{EnrollmentFinish, EnrollmentInput, LoginInput, LoginVerify},
    crypto::{self, PREAUTH_COOKIE, SESSION_COOKIE, Token},
    moderation::HideInput,
};

#[derive(Clone)]
struct Routes {
    service: Option<AdminService>,
    static_dir: Option<PathBuf>,
}
#[derive(Clone, Copy)]
struct Peer(IpAddr);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

pub(super) fn router(service: Option<AdminService>, static_dir: Option<PathBuf>) -> Router {
    let state = Routes {
        service,
        static_dir,
    };
    Router::new()
        .route("/admin", get(|| async { Redirect::temporary("/admin/") }))
        .route("/admin/", get(index))
        .route("/admin/index.html", get(index))
        .route("/admin/{*path}", any(|| async { StatusCode::NOT_FOUND }))
        .route("/api/admin/login/options", get(options))
        .route("/api/admin/login", post(login))
        .route("/api/admin/login/verify", post(verify))
        .route("/api/admin/login/totp", post(totp_login))
        .route("/api/admin/totp/status", get(totp_status))
        .route("/api/admin/totp/setup/start", post(totp_start))
        .route("/api/admin/totp/setup/confirm", post(totp_confirm))
        .route("/api/admin/totp/disable", post(totp_disable))
        .route("/api/admin/enrollment/start", post(enroll_start))
        .route("/api/admin/enrollment/finish", post(enroll_finish))
        .route("/api/admin/session", get(session))
        .route("/api/admin/logout", post(logout))
        .route("/api/admin/strokes/hide", post(hide))
        .route("/api/admin/operations/{id}", get(operation))
        .route(
            "/api/admin/{*path}",
            any(|| async { StatusCode::NOT_FOUND }),
        )
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

fn exact_origin(headers: &HeaderMap, expected: &str) -> bool {
    let mut origins = headers.get_all("origin").iter();
    let matches = origins.next().and_then(|value| value.to_str().ok()) == Some(expected);
    matches && origins.next().is_none()
}

/// The trusted reverse proxy (Caddy) is the only thing that may present a
/// forwarded address. From any non-loopback peer we never read forwarded
/// headers, so spoofing them cannot change whose requests get rate-limited.
fn client_ip(request: &Request) -> Option<IpAddr> {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()?
        .0
        .ip();
    if !peer.is_loopback() {
        return Some(peer);
    }
    let forwarded = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .and_then(|value| value.trim().parse().ok());
    Some(forwarded.unwrap_or(peer))
}

async fn guard(State(state): State<Routes>, mut request: Request, next: Next) -> Response {
    let is_api = request.uri().path().starts_with("/api/admin/");
    let allowed = match &state.service {
        // The entry contains no secrets and may explain disabled administration.
        // API access still fails closed, including all enrollment/login/session routes.
        None if !is_api => Ok(()),
        None => Err(AdminError::Disabled),
        Some(service) => {
            let changing = !matches!(*request.method(), Method::GET | Method::HEAD);
            let wrong_origin = (changing || request.headers().contains_key("origin"))
                && !exact_origin(request.headers(), &service.0.origin);
            let cross_site_get = is_api
                && request
                    .headers()
                    .get("sec-fetch-site")
                    .is_some_and(|site| site != "same-origin" && site != "none");
            if wrong_origin || cross_site_get {
                Err(AdminError::Forbidden)
            } else if is_api {
                match client_ip(&request) {
                    None => Err(AdminError::Unavailable),
                    Some(ip) => {
                        let rate = service
                            .0
                            .rate
                            .check("http/global".into(), 600, Duration::from_secs(60))
                            .and_then(|_| {
                                service.0.rate.check(
                                    format!("http/ip/{ip}"),
                                    120,
                                    Duration::from_secs(60),
                                )
                            });
                        request.extensions_mut().insert(Peer(ip));
                        rate
                    }
                }
            } else {
                Ok(())
            }
        }
    };
    let mut response = match allowed {
        Ok(()) => next.run(request).await,
        Err(error) => error.into_response(),
    };
    let headers = response.headers_mut();
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    headers.insert(
        "x-robots-tag",
        HeaderValue::from_static("noindex, nofollow"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static(
            "publickey-credentials-get=(self), publickey-credentials-create=(self)",
        ),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    let websocket_origin = state
        .service
        .as_ref()
        .map(|service| service.0.origin.replacen("https://", "wss://", 1))
        .unwrap_or_default();
    let policy = format!(
        "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' {websocket_origin}; img-src 'self' data:; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
    );
    headers.insert(
        "content-security-policy",
        HeaderValue::from_str(&policy).unwrap_or_else(|_| {
            HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'")
        }),
    );
    response
}

fn enabled(state: &Routes) -> Result<&AdminService, AdminError> {
    state.service.as_ref().ok_or(AdminError::Disabled)
}
fn body<T>(input: Result<Json<T>, JsonRejection>) -> Result<T, AdminError> {
    input.map(|Json(input)| input).map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            AdminError::TooLarge
        } else {
            AdminError::Invalid
        }
    })
}
fn cookie_response<T: Serialize>(
    body: T,
    name: &str,
    token: &Token,
    max_age: u32,
) -> Result<Response, AdminError> {
    let mut response = Json(body).into_response();
    response.headers_mut().append(
        SET_COOKIE,
        HeaderValue::from_str(&crypto::set_cookie(name, token, max_age))
            .map_err(|_| AdminError::Unavailable)?,
    );
    Ok(response)
}

async fn totp_login(
    State(state): State<Routes>,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
    input: Result<Json<super::totp::LoginInput>, JsonRejection>,
) -> Result<Response, AdminError> {
    let service = enabled(&state)?;
    let result = service.totp_login(&headers, body(input)?, peer.0).await;
    if matches!(result, Err(AdminError::Unauthorized)) {
        service.audit_failure("login_totp_failed").await?;
    }
    let (reply, token) = result?;
    cookie_response(reply, SESSION_COOKIE, &token, 8 * 60 * 60)
}
async fn totp_status(
    State(state): State<Routes>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AdminError> {
    Ok(Json(enabled(&state)?.totp_status(&headers).await?))
}
async fn totp_start(
    State(state): State<Routes>,
    headers: HeaderMap,
    input: Result<Json<Empty>, JsonRejection>,
) -> Result<impl IntoResponse, AdminError> {
    body(input)?;
    Ok(Json(enabled(&state)?.totp_start(&headers).await?))
}
async fn totp_confirm(
    State(state): State<Routes>,
    headers: HeaderMap,
    input: Result<Json<super::totp::CodeInput>, JsonRejection>,
) -> Result<StatusCode, AdminError> {
    enabled(&state)?
        .totp_confirm(&headers, body(input)?)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn totp_disable(
    State(state): State<Routes>,
    headers: HeaderMap,
    input: Result<Json<Empty>, JsonRejection>,
) -> Result<StatusCode, AdminError> {
    body(input)?;
    enabled(&state)?.totp_disable(&headers).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn index(State(state): State<Routes>) -> Result<Response, AdminError> {
    let directory = state.static_dir.ok_or(AdminError::Disabled)?;
    let html = tokio::fs::read(directory.join("admin/index.html"))
        .await
        .map_err(|_| AdminError::Disabled)?;
    Ok((
        [(CONTENT_TYPE, "text/html; charset=utf-8")],
        Body::from(html),
    )
        .into_response())
}
async fn options(State(state): State<Routes>, headers: HeaderMap) -> Result<Response, AdminError> {
    let (reply, token) = enabled(&state)?.options(&headers).await?;
    cookie_response(reply, PREAUTH_COOKIE, &token, 300)
}
async fn login(
    State(state): State<Routes>,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
    input: Result<Json<LoginInput>, JsonRejection>,
) -> Result<Response, AdminError> {
    let service = enabled(&state)?;
    let result = service.login_start(&headers, body(input)?, peer.0).await;
    if matches!(result, Err(AdminError::Unauthorized)) {
        service.audit_failure("login_failed").await?;
    }
    let (reply, token) = result?;
    cookie_response(reply, PREAUTH_COOKIE, &token, 300)
}
async fn verify(
    State(state): State<Routes>,
    headers: HeaderMap,
    input: Result<Json<LoginVerify>, JsonRejection>,
) -> Result<Response, AdminError> {
    let service = enabled(&state)?;
    let result = service.login_finish(&headers, body(input)?).await;
    if matches!(result, Err(AdminError::Unauthorized)) {
        service.audit_failure("mfa_failed").await?;
    }
    let (reply, token) = result?;
    let mut response = cookie_response(reply, SESSION_COOKIE, &token, 8 * 3600)?;
    response.headers_mut().append(
        SET_COOKIE,
        HeaderValue::from_str(&crypto::clear_cookie(PREAUTH_COOKIE))
            .map_err(|_| AdminError::Unavailable)?,
    );
    Ok(response)
}
async fn enroll_start(
    State(state): State<Routes>,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
    input: Result<Json<EnrollmentInput>, JsonRejection>,
) -> Result<Response, AdminError> {
    let service = enabled(&state)?;
    let result = service
        .enrollment_start(&headers, body(input)?, peer.0)
        .await;
    if matches!(result, Err(AdminError::Unauthorized)) {
        service.audit_failure("enrollment_failed").await?;
    }
    let (reply, token) = result?;
    cookie_response(reply, PREAUTH_COOKIE, &token, 300)
}
async fn enroll_finish(
    State(state): State<Routes>,
    headers: HeaderMap,
    input: Result<Json<EnrollmentFinish>, JsonRejection>,
) -> Result<Response, AdminError> {
    let service = enabled(&state)?;
    let result = service.enrollment_finish(&headers, body(input)?).await;
    if matches!(result, Err(AdminError::Unauthorized)) {
        service.audit_failure("enrollment_failed").await?;
    }
    Ok(Json(result?).into_response())
}
async fn session(State(state): State<Routes>, headers: HeaderMap) -> Result<Response, AdminError> {
    let (_, _, reply) = enabled(&state)?.session(&headers, false).await?;
    Ok(Json(reply).into_response())
}
async fn logout(
    State(state): State<Routes>,
    headers: HeaderMap,
    input: Result<Json<Empty>, JsonRejection>,
) -> Result<Response, AdminError> {
    body(input)?;
    enabled(&state)?.logout(&headers).await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().append(
        SET_COOKIE,
        HeaderValue::from_str(&crypto::clear_cookie(SESSION_COOKIE))
            .map_err(|_| AdminError::Unavailable)?,
    );
    Ok(response)
}
async fn hide(
    State(state): State<Routes>,
    headers: HeaderMap,
    input: Result<Json<HideInput>, JsonRejection>,
) -> Result<Response, AdminError> {
    let service = enabled(&state)?;
    let (_, proof, _) = service.session(&headers, true).await?;
    let input = body(input)?.validate()?;
    Ok(Json(service.0.writer.admin_hide(proof, input).await?).into_response())
}
async fn operation(
    State(state): State<Routes>,
    headers: HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<Response, AdminError> {
    let Path(id) = path.map_err(|_| AdminError::Invalid)?;
    Ok(Json(enabled(&state)?.operation(&headers, id).await?).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn peer_request(peer: &str, forwarded: Option<&str>) -> Request {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("/api/admin/session")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        if let Some(value) = forwarded {
            request
                .headers_mut()
                .insert("x-forwarded-for", HeaderValue::from_str(value).unwrap());
        }
        request
    }

    #[test]
    fn loopback_proxy_uses_leftmost_forwarded_ip_and_non_loopback_ignores_header() {
        assert_eq!(
            client_ip(&peer_request(
                "127.0.0.1:8080",
                Some("198.51.100.7, 10.0.0.1")
            )),
            Some("198.51.100.7".parse().unwrap())
        );
        assert_eq!(
            client_ip(&peer_request("[::1]:8080", Some(" 198.51.100.9 "))),
            Some("198.51.100.9".parse().unwrap())
        );
        // A non-loopback peer (e.g. someone bypassing the proxy) cannot spoof.
        assert_eq!(
            client_ip(&peer_request("203.0.113.5:8080", Some("198.51.100.7"))),
            Some("203.0.113.5".parse().unwrap())
        );
    }

    #[test]
    fn malformed_or_missing_forwarded_ip_falls_back_to_direct_peer() {
        assert_eq!(
            client_ip(&peer_request("127.0.0.1:8080", None)),
            Some("127.0.0.1".parse().unwrap())
        );
        for malformed in ["", "not-an-ip", ",198.51.100.7"] {
            assert_eq!(
                client_ip(&peer_request("127.0.0.1:8080", Some(malformed))),
                Some("127.0.0.1".parse().unwrap()),
                "unexpected result for {malformed:?}"
            );
        }
    }
}
