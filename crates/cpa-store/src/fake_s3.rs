//! An in-memory S3 for tests: path-style buckets and objects, `?location`,
//! ListObjectsV2 with `encoding-type=url` and continuation tokens. Every request must
//! carry a SigV4 signature that matches the request as received and the bucket region.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::sigv4;

pub(crate) const ACCESS: &str = "AKIAFAKE";
pub(crate) const SECRET: &str = "fake/secret+key";

pub(crate) struct State {
    pub region: String,
    pub location: Location,
    pub page_size: usize,
    pub buckets: Mutex<HashMap<String, BTreeMap<String, Vec<u8>>>>,
    /// `METHOD /path?query` per request.
    pub log: Mutex<Vec<String>>,
}

/// How `GET ?location` answers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Location {
    Answer,
    /// 403 `AccessDenied` with `x-amz-bucket-region`, as AWS answers a key without
    /// `s3:GetBucketLocation`.
    Denied,
    /// 403 `AccessDenied` with no region at all.
    DeniedBare,
    /// 400 `AuthorizationHeaderMalformed` naming the region in the XML body.
    Malformed,
}

pub(crate) struct FakeS3 {
    pub addr: SocketAddr,
    pub state: Arc<State>,
}

fn error(status: StatusCode, code: &str) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{code}</Code><Message>{code} message</Message></Error>"
    );
    (status, body).into_response()
}

fn percent_decode(text: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", text.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// S3's `encoding-type=url` (Go `url.QueryEscape`).
fn s3_encode(key: &str) -> String {
    url::form_urlencoded::byte_serialize(key.as_bytes()).collect()
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

impl FakeS3 {
    pub(crate) async fn start(region: &str, location: Location, page_size: usize) -> Self {
        let state = Arc::new(State {
            region: region.to_owned(),
            location,
            page_size,
            buckets: Mutex::default(),
            log: Mutex::default(),
        });
        let shared = state.clone();
        let app = axum::Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let state = shared.clone();
            async move { handle(&state, method, uri, headers, body) }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { addr, state }
    }

    pub(crate) fn object(&self, bucket: &str, key: &str) -> Option<Vec<u8>> {
        self.state.buckets.lock().unwrap().get(bucket)?.get(key).cloned()
    }

    pub(crate) fn put(&self, bucket: &str, key: &str, data: &[u8]) {
        self.state
            .buckets
            .lock()
            .unwrap()
            .entry(bucket.to_owned())
            .or_default()
            .insert(key.to_owned(), data.to_vec());
    }
}

/// Recomputes the signature from what arrived; `Err` is the S3 error code.
fn verify(method: &Method, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> Result<String, &'static str> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let authorization = header("authorization");
    let payload = header("x-amz-content-sha256");
    if payload != sigv4::sha256_hex(body) {
        return Err("XAmzContentSHA256Mismatch");
    }
    let credential = authorization
        .split("Credential=")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .ok_or("AccessDenied")?;
    let mut scope = credential.split('/');
    if scope.next() != Some(ACCESS) {
        return Err("InvalidAccessKeyId");
    }
    let region = scope.nth(1).ok_or("AccessDenied")?.to_owned();
    let signed: Vec<String> = authorization
        .split("SignedHeaders=")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .ok_or("AccessDenied")?
        .split(';')
        .map(str::to_owned)
        .collect();
    let signed_values: Vec<(String, String)> = signed
        .iter()
        .filter(|name| !matches!(name.as_str(), "host" | "x-amz-date"))
        .map(|name| (name.clone(), header(name)))
        .collect();
    let query: Vec<(String, String)> = uri
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    let path = percent_decode(uri.path());
    let date = header("x-amz-date");
    let now = chrono::NaiveDateTime::parse_from_str(&date, "%Y%m%dT%H%M%SZ")
        .map_err(|_| "AccessDenied")?
        .and_utc();
    let request = sigv4::Request {
        method: method.as_str(),
        path: &path,
        query: &query,
        headers: &signed_values,
        host: &header("host"),
        payload_sha256: &payload,
    };
    let (_, expected) = sigv4::sign(&request, ACCESS, SECRET, &region, now);
    if expected != authorization {
        return Err("SignatureDoesNotMatch");
    }
    Ok(region)
}

fn handle(state: &State, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    state.log.lock().unwrap().push(format!(
        "{method} {}",
        uri.path_and_query().map(|p| p.as_str()).unwrap_or_default()
    ));
    let region = match verify(&method, &uri, &headers, &body) {
        Ok(region) => region,
        Err(code) => return error(StatusCode::FORBIDDEN, code),
    };
    let path = percent_decode(uri.path());
    let path = path.trim_start_matches('/');
    let (bucket, key) = match path.split_once('/') {
        Some((bucket, key)) => (bucket.to_owned(), key.to_owned()),
        None => (path.to_owned(), String::new()),
    };
    let query: HashMap<String, String> = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .into_owned()
        .collect();
    let mut buckets = state.buckets.lock().unwrap();
    if key.is_empty() && method == Method::GET && query.contains_key("location") {
        match state.location {
            Location::Answer => {}
            Location::DeniedBare => return error(StatusCode::FORBIDDEN, "AccessDenied"),
            Location::Denied => {
                let mut response = error(StatusCode::FORBIDDEN, "AccessDenied");
                response
                    .headers_mut()
                    .insert("x-amz-bucket-region", state.region.parse().unwrap());
                return response;
            }
            Location::Malformed => {
                let body = format!(
                    "<Error><Code>AuthorizationHeaderMalformed</Code><Message>wrong region</Message><Region>{}</Region></Error>",
                    state.region
                );
                return (StatusCode::BAD_REQUEST, body).into_response();
            }
        }
        if !buckets.contains_key(&bucket) {
            return error(StatusCode::NOT_FOUND, "NoSuchBucket");
        }
        let xmlns = "http://s3.amazonaws.com/doc/2006-03-01/";
        let body = if state.region == "us-east-1" {
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<LocationConstraint xmlns=\"{xmlns}\"/>")
        } else {
            // Legacy eu-west-1 buckets answer `EU`; requests are still signed for
            // eu-west-1.
            let shown = if state.region == "eu-west-1" {
                "EU"
            } else {
                &state.region
            };
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<LocationConstraint xmlns=\"{xmlns}\">{shown}</LocationConstraint>"
            )
        };
        return (StatusCode::OK, body).into_response();
    }
    // Bucket creation is signed for us-east-1 like minio's MakeBucket; everything else
    // must use the bucket's region.
    let creating = key.is_empty() && method == Method::PUT;
    if !creating && region != state.region {
        return error(StatusCode::BAD_REQUEST, "AuthorizationHeaderMalformed");
    }
    if creating {
        buckets.entry(bucket).or_default();
        return StatusCode::OK.into_response();
    }
    let Some(objects) = buckets.get_mut(&bucket) else {
        return if method == Method::HEAD {
            StatusCode::NOT_FOUND.into_response()
        } else {
            error(StatusCode::NOT_FOUND, "NoSuchBucket")
        };
    };
    if key.is_empty() {
        return match method {
            Method::HEAD => StatusCode::OK.into_response(),
            Method::GET if query.get("list-type").map(String::as_str) == Some("2") => {
                let prefix = query.get("prefix").cloned().unwrap_or_default();
                let after = query.get("continuation-token").cloned().unwrap_or_default();
                let encode = query.get("encoding-type").map(String::as_str) == Some("url");
                let matching: Vec<&String> = objects
                    .keys()
                    .filter(|k| k.starts_with(&prefix) && k.as_str() > after.as_str())
                    .collect();
                let page: Vec<&&String> = matching.iter().take(state.page_size).collect();
                let truncated = matching.len() > page.len();
                let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult>");
                xml.push_str(&format!("<Name>{bucket}</Name><KeyCount>{}</KeyCount>", page.len()));
                for key in &page {
                    let shown = if encode { s3_encode(key) } else { xml_escape(key) };
                    xml.push_str(&format!("<Contents><Key>{shown}</Key><Size>1</Size></Contents>"));
                }
                xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
                if truncated && let Some(last) = page.last() {
                    xml.push_str(&format!(
                        "<NextContinuationToken>{}</NextContinuationToken>",
                        xml_escape(last)
                    ));
                }
                xml.push_str("</ListBucketResult>");
                (StatusCode::OK, xml).into_response()
            }
            _ => error(StatusCode::NOT_IMPLEMENTED, "NotImplemented"),
        };
    }
    match method {
        Method::PUT => {
            objects.insert(key, body.to_vec());
            StatusCode::OK.into_response()
        }
        Method::GET => match objects.get(&key) {
            Some(data) => (StatusCode::OK, data.clone()).into_response(),
            None => error(StatusCode::NOT_FOUND, "NoSuchKey"),
        },
        Method::HEAD => match objects.get(&key) {
            Some(_) => StatusCode::OK.into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
        Method::DELETE => {
            objects.remove(&key);
            StatusCode::NO_CONTENT.into_response()
        }
        _ => error(StatusCode::NOT_IMPLEMENTED, "NotImplemented"),
    }
}
