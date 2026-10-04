//! The production [`Doer`]: Go's `http.Client` for the configured proxy, sending one
//! request per call (Go clones the client with `CheckRedirect` returning
//! `ErrUseLastResponse`; the store follows redirects itself, re-checking auth and the
//! rate limiter on every hop).

use futures_util::StreamExt;
use futures_util::future::BoxFuture;

use super::auth::Headers;
use super::client::{Doer, DoerResponse};

pub struct WreqDoer {
    client: wreq::Client,
}

impl WreqDoer {
    /// `client` must not follow redirects (`cpa_exec::proxy::GoClients` clients do not).
    pub fn new(client: wreq::Client) -> Self {
        Self { client }
    }
}

impl Doer for WreqDoer {
    fn get(&self, url: &str, headers: &Headers) -> BoxFuture<'_, Result<DoerResponse, String>> {
        let url = url.to_owned();
        let mut go = cpa_exec::proxy::GoHeaders::new();
        for (name, values) in headers {
            for value in values {
                go.add_raw(name, value.clone());
            }
        }
        Box::pin(async move {
            let request = self.client.get(&url).redirect(wreq::redirect::Policy::none());
            // Go's transport asks for gzip itself and then decodes it transparently.
            let (request, auto_gzip) = go.apply(request, None);
            let response = request
                .send()
                .await
                .map_err(|e| crate::hosthttp::transport_cause(&e, cpa_exec::xai_url::parse(&url).ok().as_ref()))?;
            let status = response.status().as_u16();
            let gzip = auto_gzip
                && response
                    .headers()
                    .get(http::header::CONTENT_ENCODING)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
            let mut out = Headers::new();
            for (name, value) in response.headers() {
                if gzip && (name == http::header::CONTENT_ENCODING || name == http::header::CONTENT_LENGTH) {
                    continue;
                }
                out.entry(cpa_exec::proxy::canonical_header(name.as_str()))
                    .or_default()
                    .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
            }
            let stream = response.bytes_stream().map(|r| r.map_err(std::io::Error::other));
            let body = if gzip {
                let reader = tokio::io::BufReader::new(tokio_util::io::StreamReader::new(stream));
                let mut decoder = async_compression::tokio::bufread::GzipDecoder::new(reader);
                decoder.multiple_members(true);
                tokio_util::io::ReaderStream::new(decoder).boxed()
            } else {
                stream.boxed()
            };
            Ok(DoerResponse {
                status,
                headers: out,
                body: body
                    .map(|chunk| chunk.map_err(|e| crate::hosthttp::go_read_error(&e)))
                    .boxed(),
            })
        })
    }
}
