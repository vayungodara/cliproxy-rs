//! Test-only scripted HTTP upstream and the Go-generated Codex fixtures
//! (`tests/fixtures/codex_go.json`, produced by `tests/reference/codex/main.go`).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;
use serde_json::Value;

pub(crate) static GO: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../tests/fixtures/codex_go.json")).expect("fixture JSON"));

#[derive(Debug, Clone)]
pub(crate) struct Captured {
    pub method: String,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Captured {
    pub fn header(&self, name: &str) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default()
    }
}

#[derive(Clone)]
pub(crate) struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

pub(crate) fn json(status: u16, body: &str) -> Reply {
    Reply {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: body.into(),
    }
}

type Script = Arc<Mutex<HashMap<String, VecDeque<Reply>>>>;

pub(crate) struct Mock {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Captured>>>,
    script: Script,
}

impl Mock {
    pub async fn start() -> Self {
        let requests: Arc<Mutex<Vec<Captured>>> = Arc::default();
        let script: Script = Arc::default();
        let (r, s) = (requests.clone(), script.clone());
        let app = axum::Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let (r, s) = (r.clone(), s.clone());
            async move {
                r.lock().unwrap().push(Captured {
                    method: method.to_string(),
                    path: uri.path().to_owned(),
                    headers,
                    body,
                });
                let reply = s.lock().unwrap().get_mut(uri.path()).and_then(VecDeque::pop_front);
                match reply {
                    Some(reply) => {
                        let mut response = (StatusCode::from_u16(reply.status).unwrap(), reply.body).into_response();
                        for (k, v) in reply.headers {
                            response
                                .headers_mut()
                                .insert(axum::http::HeaderName::try_from(k).unwrap(), v.parse().unwrap());
                        }
                        response
                    }
                    None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, requests, script }
    }

    pub fn script(&self, path: &str, replies: Vec<Reply>) {
        self.script.lock().unwrap().insert(path.into(), replies.into());
    }

    pub fn take(&self) -> Vec<Captured> {
        std::mem::take(&mut self.requests.lock().unwrap())
    }
}

/// One recorded capture event, owned.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tap {
    Request {
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        body: String,
        account: [String; 5],
    },
    Metadata(u16, Vec<(String, String)>),
    Chunk(String),
    Error(String),
    WsRequest {
        url: String,
        headers: Vec<(String, String)>,
        body: String,
        account: [String; 5],
    },
    WsHandshake(u16),
    WsResponse(String),
    WsError(String, String),
}

/// A capture observer that keeps every event (the request log's view of the attempt).
#[derive(Default)]
pub(crate) struct Wiretap(pub std::sync::Mutex<Vec<Tap>>);

impl Wiretap {
    pub fn taps(&self) -> Vec<Tap> {
        self.0.lock().unwrap().clone()
    }
}

impl cpa_core::exec::CaptureObserver for Wiretap {
    fn record(&self, event: cpa_core::exec::CaptureEvent<'_>) {
        use cpa_core::exec::CaptureEvent::*;
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let account = |i: &cpa_core::exec::UpstreamRequest<'_>| {
            [i.provider, i.auth_id, i.auth_label, i.auth_type, i.auth_value].map(str::to_owned)
        };
        let tap = match event {
            Request(i) => Tap::Request {
                url: i.url.into(),
                method: i.method.into(),
                headers: i.headers.to_vec(),
                body: text(i.body),
                account: account(&i),
            },
            ResponseMetadata(status, headers) => Tap::Metadata(status, headers.to_vec()),
            ResponseChunk(bytes) => Tap::Chunk(text(bytes)),
            ResponseError(error) => Tap::Error(error.into()),
            WebsocketRequest(i) => Tap::WsRequest {
                url: i.url.into(),
                headers: i.headers.to_vec(),
                body: text(i.body),
                account: account(&i),
            },
            WebsocketHandshake(status, _) => Tap::WsHandshake(status),
            WebsocketResponse(bytes) => Tap::WsResponse(text(bytes)),
            WebsocketError { stage, error } => Tap::WsError(stage.into(), error.into()),
        };
        self.0.lock().unwrap().push(tap);
    }
}
