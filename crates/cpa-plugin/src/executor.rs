//! Plugin executors at the RPC level (internal/pluginhost/adapters_executors.go,
//! rpc_client_stream.go, executor_route.go): format negotiation, the executor's provider,
//! and `executor.execute` / `execute_stream` / `count_tokens` / `http_request`.
//! Translation around the call, usage reporting and dispatch live in the server.

use bytes::Bytes;
use cpa_core::format::Format;
use futures_util::stream::BoxStream;

use crate::abi::method;
use crate::api::{ExecutorHttpRequest, ExecutorHttpResponse, ExecutorRequest, ExecutorResponse, ExecutorStreamChunk};
use crate::callbacks::{ContextGuard, RequestScope};
use crate::gojson::{GoStruct, Header, ObjWriter};
use crate::host::{Host, Record, normalize_provider};
use crate::rpc::{self, CallError};
use crate::streams::{Chunk, Reader};

/// Go `normalizeExecutorFormatName`: aliases map to translator formats; anything else
/// is kept as written (trimmed); empty and `none` mean no format.
pub fn normalize_format(raw: &str) -> String {
    match raw.trim().to_lowercase().as_str() {
        "" | "none" => String::new(),
        "chat-completions" | "chat_completions" | "openai-chat-completions" | "openai_chat_completions" => {
            "openai".into()
        }
        "responses" | "openai-responses" | "openai_responses" => "openai-response".into(),
        "anthropic" => "claude".into(),
        _ => raw.trim().to_owned(),
    }
}

/// Go `normalizeExecutorFormats`: normalized, de-duplicated, empty dropped.
pub fn normalize_formats(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for format in raw.iter().map(|f| normalize_format(f)) {
        if !format.is_empty() && !out.contains(&format) {
            out.push(format);
        }
    }
    out
}

/// Go `sdktranslator.HasRequestTransformer` / `HasResponseTransformer`: a registered
/// pair between two named formats.
pub fn translator_pair_exists(client: &str, upstream: &str) -> bool {
    match (Format::parse(client), Format::parse(upstream)) {
        (Some(c), Some(u)) => cpa_translate::pair(c, u).is_some(),
        _ => false,
    }
}

/// One plugin executor (Go `executorAdapter`).
#[derive(Debug, Clone)]
pub struct Adapter {
    pub record: Record,
    pub provider: String,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

/// Formats chosen for one call (Go `preparedExecutorCall`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    /// The format the request arrives in.
    pub input_requested: String,
    /// The format the client wants back.
    pub requested: String,
    /// The format the plugin receives.
    pub input: String,
    /// The format the plugin answers in (`ExecutorRequest.Format`).
    pub output: String,
}

impl Adapter {
    /// Go `selectExecutorInputFormat`.
    pub fn select_input(&self, requested: &str) -> Result<String, String> {
        if self.inputs.is_empty() {
            return Err(format!("plugin executor {} declares no input formats", self.provider));
        }
        if self.inputs.iter().any(|f| f == requested) {
            return Ok(requested.to_owned());
        }
        self.inputs
            .iter()
            .find(|f| requested.is_empty() || translator_pair_exists(requested, f))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "plugin executor {} does not support input format {requested:?}",
                    self.provider
                )
            })
    }

    /// Go `selectExecutorOutputFormat`. `plugin_response_translator` is whether any
    /// plugin offers response translation (Go `hasResponseTranslator`).
    pub fn select_output(
        &self,
        requested: &str,
        input: &str,
        plugin_response_translator: bool,
    ) -> Result<String, String> {
        if self.outputs.is_empty() {
            return Err(format!("plugin executor {} declares no output formats", self.provider));
        }
        let available = |from: &str, to: &str| {
            from.is_empty()
                || to.is_empty()
                || from == to
                || translator_pair_exists(to, from)
                || plugin_response_translator
        };
        if self.outputs.iter().any(|f| f == requested) {
            return Ok(requested.to_owned());
        }
        if self.outputs.iter().any(|f| f == input) && available(input, requested) {
            return Ok(input.to_owned());
        }
        self.outputs
            .iter()
            .find(|f| requested.is_empty() || available(f, requested))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "plugin executor {} does not support output format {requested:?}",
                    self.provider
                )
            })
    }

    /// Go `prepareExecutorCall` without the translation itself. `source` and `response`
    /// are the request's source and response formats (empty when unset).
    pub fn negotiate(
        &self,
        source: &str,
        response: &str,
        plugin_response_translator: bool,
    ) -> Result<Negotiated, String> {
        let input_requested = if source.is_empty() {
            "openai".to_owned()
        } else {
            normalize_format(source)
        };
        let requested_raw = if response.is_empty() { source } else { response };
        let requested = if requested_raw.is_empty() {
            "openai".to_owned()
        } else {
            normalize_format(requested_raw)
        };
        let input = self.select_input(&input_requested)?;
        let output = self.select_output(&requested, &input, plugin_response_translator)?;
        Ok(Negotiated {
            input_requested,
            requested,
            input,
            output,
        })
    }
}

/// A plugin executor's stream: headers, then chunks until the plugin closes it. Dropping
/// it releases the stream and its callback context.
pub struct PluginStream {
    pub headers: Header,
    pub chunks: BoxStream<'static, Chunk>,
}

crate::go_struct! {
    /// `rpcExecutorStreamResponse`.
    pub struct StreamResponse("pluginhost.rpcExecutorStreamResponse") {
        "headers" omitempty => headers: Header,
        "chunks" omitempty => chunks: Vec<ExecutorStreamChunk>,
    }
}

impl Host {
    /// Go `executorProvider`: the provider the plugin registered models for, else its
    /// `executor.identifier`, lowercased.
    pub async fn executor_provider(&self, record: &Record) -> Option<String> {
        if !self.record_current(record) {
            return None;
        }
        let registered = self
            .state()
            .model_providers
            .get(&record.id)
            .cloned()
            .unwrap_or_default();
        let provider = if registered.is_empty() {
            rpc::identifier(&record.client, method::EXECUTOR_IDENTIFIER).await
        } else {
            registered
        };
        let provider = normalize_provider(&provider);
        (!provider.is_empty()).then_some(provider)
    }

    /// Go `executorAdapterForPlugin`.
    pub async fn executor_adapter(&self, plugin_id: &str) -> Result<Adapter, String> {
        let plugin_id = plugin_id.trim();
        if plugin_id.is_empty() {
            return Err("target executor plugin id is required".into());
        }
        let Some(record) = self.active_records().into_iter().find(|r| r.id == plugin_id) else {
            return Err(format!("plugin executor {plugin_id} not found"));
        };
        if self.is_fused(&record.id) {
            return Err(format!("plugin executor {plugin_id} is unavailable"));
        }
        if !record.plugin.caps.executor {
            return Err(format!("plugin {plugin_id} does not declare an executor"));
        }
        let Some(provider) = self.executor_provider(&record).await else {
            return Err(format!("plugin executor {plugin_id} has no provider identifier"));
        };
        Ok(self.adapter_for(record, provider))
    }

    pub(crate) fn adapter_for(&self, record: Record, provider: String) -> Adapter {
        Adapter {
            inputs: normalize_formats(&record.plugin.caps.executor_input_formats),
            outputs: normalize_formats(&record.plugin.caps.executor_output_formats),
            record,
            provider,
        }
    }

    /// Go `executorPluginReady`: an active, unfused executor plugin that serves static
    /// models and can take a request in `source_format`.
    pub async fn executor_plugin_ready(&self, plugin_id: &str, source_format: &str) -> bool {
        let plugin_id = plugin_id.trim();
        let Some(record) = self
            .active_records()
            .into_iter()
            .find(|r| r.id == plugin_id && !self.is_fused(&r.id))
        else {
            return false;
        };
        if !record.plugin.caps.executor || !record.plugin.allows_static_models() {
            return false;
        }
        let Some(provider) = self.executor_provider(&record).await else {
            return false;
        };
        let adapter = self.adapter_for(record, provider);
        adapter
            .negotiate(source_format, source_format, self.has_response_translator())
            .is_ok()
    }

    fn adapter_usable(&self, adapter: &Adapter) -> Result<(), CallError> {
        if self.is_fused(&adapter.record.id) || !self.record_current(&adapter.record) {
            return Err(CallError::Other(format!(
                "plugin executor {} is unavailable",
                adapter.provider
            )));
        }
        Ok(())
    }

    /// `executor.execute` with a callback context.
    pub async fn executor_execute(
        &self,
        adapter: &Adapter,
        req: &ExecutorRequest,
        scope: &RequestScope,
    ) -> Result<ExecutorResponse, CallError> {
        self.adapter_usable(adapter)?;
        self.call_with_callback(&adapter.record, method::EXECUTOR_EXECUTE, req, scope)
            .await
    }

    /// `executor.count_tokens` with a callback context.
    pub async fn executor_count_tokens(
        &self,
        adapter: &Adapter,
        req: &ExecutorRequest,
        scope: &RequestScope,
    ) -> Result<ExecutorResponse, CallError> {
        self.adapter_usable(adapter)?;
        self.call_with_callback(&adapter.record, method::EXECUTOR_COUNT_TOKENS, req, scope)
            .await
    }

    /// `executor.http_request` with a callback context.
    pub async fn executor_http_request(
        &self,
        adapter: &Adapter,
        req: &ExecutorHttpRequest,
        scope: &RequestScope,
    ) -> Result<ExecutorHttpResponse, CallError> {
        self.adapter_usable(adapter)?;
        self.call_with_callback(&adapter.record, method::EXECUTOR_HTTP_REQUEST, req, scope)
            .await
    }

    /// Go `rpcPluginAdapter.ExecuteStream`: opens a host stream and a callback context,
    /// sends `stream_id` and `host_callback_id`. Inline chunks in the answer are the whole
    /// stream; otherwise chunks arrive through `host.stream.emit` until the plugin closes
    /// the stream, and the callback context stays open until then.
    pub async fn executor_execute_stream(
        &self,
        adapter: &Adapter,
        req: &ExecutorRequest,
        scope: &RequestScope,
    ) -> Result<PluginStream, CallError> {
        self.adapter_usable(adapter)?;
        let reader = self.inner.callbacks.streams.open();
        let guard = self.open_callback(&adapter.record, scope);
        let mut raw = Vec::new();
        let mut w = ObjWriter::begin(&mut raw);
        w.embed(req);
        w.field("stream_id", &reader.id().to_owned(), true);
        w.field("host_callback_id", &guard.id().to_owned(), true);
        w.end();
        let resp: StreamResponse = self
            .call_raw(&adapter.record, method::EXECUTOR_EXECUTE_STREAM, raw)
            .await?;
        if !resp.chunks.is_empty() {
            drop((reader, guard));
            let chunks: Vec<Chunk> = resp.chunks.into_iter().map(|c| Chunk::data(c.payload)).collect();
            return Ok(PluginStream {
                headers: resp.headers,
                chunks: Box::pin(futures_util::stream::iter(chunks)),
            });
        }
        Ok(PluginStream {
            headers: resp.headers,
            chunks: bridged(reader, guard),
        })
    }
}

/// Chunks from the bridge; the reader and callback context drop with the stream.
fn bridged(reader: Reader, guard: ContextGuard) -> BoxStream<'static, Chunk> {
    Box::pin(futures_util::stream::unfold(
        (reader, guard),
        |(reader, guard)| async move {
            let chunk = reader.next().await?;
            Some((chunk, (reader, guard)))
        },
    ))
}

/// Request bytes an executor plugin receives, for tests and logs.
pub fn encode_request(req: &ExecutorRequest) -> Bytes {
    let mut out = Vec::new();
    let mut w = ObjWriter::begin(&mut out);
    req.encode_fields(&mut w);
    w.end();
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(inputs: &[&str], outputs: &[&str]) -> Adapter {
        let mut record_plugin = crate::rpc::Plugin::default();
        record_plugin.caps.executor_input_formats = inputs.iter().map(|s| s.to_string()).collect();
        record_plugin.caps.executor_output_formats = outputs.iter().map(|s| s.to_string()).collect();
        Adapter {
            inputs: normalize_formats(&record_plugin.caps.executor_input_formats),
            outputs: normalize_formats(&record_plugin.caps.executor_output_formats),
            provider: "p".into(),
            record: crate::host::Record {
                id: "x".into(),
                path: Default::default(),
                version: String::new(),
                priority: 0,
                plugin: record_plugin,
                client: crate::client::GuardedClient::new(std::sync::Arc::new(Never), Default::default()),
            },
        }
    }

    struct Never;
    impl crate::client::PluginClient for Never {
        fn call(&self, _: &str, _: &[u8]) -> Result<Bytes, String> {
            Err("unused".into())
        }
        fn shutdown(&self) {}
    }

    /// Go `selectExecutorInputFormat` / `selectExecutorOutputFormat`.
    #[test]
    fn negotiation_follows_go() {
        assert_eq!(
            normalize_formats(&[
                "Chat-Completions".into(),
                "openai".into(),
                "none".into(),
                " Custom ".into()
            ]),
            ["openai", "Custom"]
        );
        let a = adapter(&["chat-completions"], &["chat-completions"]);
        let n = a.negotiate("claude", "", false).unwrap();
        assert_eq!(
            (n.input.as_str(), n.output.as_str()),
            ("openai", "openai"),
            "claude->openai request and openai->claude response pairs exist"
        );
        assert!(
            a.negotiate("custom", "", false).is_err(),
            "no translator from an unknown format"
        );
        let n = a.negotiate("", "", false).unwrap();
        assert_eq!(n.requested, "openai");
        let b = adapter(&["anthropic"], &["responses"]);
        let n = b.negotiate("openai-response", "openai-response", false).unwrap();
        assert_eq!((n.input.as_str(), n.output.as_str()), ("claude", "openai-response"));
        assert_eq!(
            adapter(&[], &["openai"]).select_input("openai"),
            Err("plugin executor p declares no input formats".into())
        );
        // An unregistered response pair is still available when a plugin translates.
        let c = adapter(&["openai"], &["custom"]);
        assert!(c.negotiate("openai", "", false).is_err());
        assert_eq!(c.negotiate("openai", "", true).unwrap().output, "custom");
    }
}
