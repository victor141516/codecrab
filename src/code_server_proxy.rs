use std::{net::SocketAddr, str::FromStr};

use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use hyper::upgrade::OnUpgrade;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
};
use uuid::Uuid;

use crate::server::ServerState;

pub(crate) async fn proxy_root(
    State(state): State<ServerState>,
    Path(instance_id): Path<Uuid>,
    mut request: Request,
) -> Response {
    let Some(target) = state.inner.code_server.target(instance_id) else {
        return (StatusCode::NOT_FOUND, "code-server instance not found").into_response();
    };
    match proxy_request(target, instance_id, String::new(), &mut request).await {
        Ok(response) => response,
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            format!("code-server proxy failed: {error:#}"),
        )
            .into_response(),
    }
}

pub(crate) async fn proxy(
    State(state): State<ServerState>,
    Path((instance_id, tail)): Path<(Uuid, String)>,
    mut request: Request,
) -> Response {
    let Some(target) = state.inner.code_server.target(instance_id) else {
        return (StatusCode::NOT_FOUND, "code-server instance not found").into_response();
    };
    match proxy_request(target, instance_id, tail, &mut request).await {
        Ok(response) => response,
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            format!("code-server proxy failed: {error:#}"),
        )
            .into_response(),
    }
}

async fn proxy_request(
    target: SocketAddr,
    instance_id: Uuid,
    tail: String,
    request: &mut Request,
) -> Result<Response> {
    let query = request
        .uri()
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let path = if tail.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", tail.trim_start_matches('/'))
    };
    *request.uri_mut() = Uri::from_str(&format!("http://{target}{path}{query}"))?;

    let upgrade = is_upgrade(request).then(|| hyper::upgrade::on(&mut *request));
    let connector = HttpConnector::new();
    let client: Client<HttpConnector, Body> =
        Client::builder(TokioExecutor::new()).build(connector);
    let backend = client
        .request(std::mem::replace(request, Request::new(Body::empty())))
        .await
        .context("cannot contact code-server")?;
    let (mut parts, body) = backend.into_parts();
    if let Some(location) = parts
        .headers
        .get("location")
        .and_then(|value| value.to_str().ok())
        .filter(|location| location.starts_with('/'))
    {
        parts.headers.insert(
            "location",
            HeaderValue::from_str(&format!("/code-server/{instance_id}{}", location))?,
        );
    }
    let mut response = Response::from_parts(parts, Body::new(body));
    if let Some(browser_upgrade) = upgrade {
        let backend_upgrade = hyper::upgrade::on(&mut response);
        tokio::spawn(async move {
            if let Err(error) = bridge_upgrades(browser_upgrade, backend_upgrade).await {
                eprintln!("code-server WebSocket proxy stopped: {error:#}");
            }
        });
    }
    Ok(response)
}

fn is_upgrade(request: &Request) -> bool {
    request
        .headers()
        .get("upgrade")
        .and_then(|value| value.to_str().ok())
        .is_some()
}

async fn bridge_upgrades(browser: OnUpgrade, backend: OnUpgrade) -> Result<()> {
    let browser = TokioIo::new(browser.await.context("browser upgrade failed")?);
    let backend = TokioIo::new(backend.await.context("code-server upgrade failed")?);
    let (mut browser_read, mut browser_write) = tokio::io::split(browser);
    let (mut backend_read, mut backend_write) = tokio::io::split(backend);
    let browser_to_backend = tokio::io::copy(&mut browser_read, &mut backend_write);
    let backend_to_browser = tokio::io::copy(&mut backend_read, &mut browser_write);
    tokio::try_join!(browser_to_backend, backend_to_browser)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn echo_upstream_uri(request: Request) -> Response {
        let mut response = request.uri().to_string().into_response();
        response
            .headers_mut()
            .insert("location", HeaderValue::from_static("/redirect"));
        response
    }

    #[test]
    fn upgrade_detection_requires_an_upgrade_header() {
        let plain = Request::new(Body::empty());
        assert!(!is_upgrade(&plain));
        let mut upgrade = Request::new(Body::empty());
        upgrade
            .headers_mut()
            .insert("upgrade", HeaderValue::from_static("websocket"));
        assert!(is_upgrade(&upgrade));
    }

    #[tokio::test]
    async fn proxy_strips_the_instance_prefix_and_rewrites_root_redirects() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().fallback(echo_upstream_uri))
                .await
                .unwrap();
        });
        let instance_id = Uuid::new_v4();
        let mut request = Request::builder()
            .uri(format!("/code-server/{instance_id}/workspace/file?line=7"))
            .body(Body::empty())
            .unwrap();

        let response = proxy_request(address, instance_id, "workspace/file".into(), &mut request)
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("location")
                .unwrap()
                .to_str()
                .unwrap(),
            format!("/code-server/{instance_id}/redirect")
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body, "/workspace/file?line=7");
        server.abort();
    }
}
