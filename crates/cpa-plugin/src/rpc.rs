//! RPC calls over a plugin client and the registration contract
//! (internal/pluginhost/rpc_client.go, rpc_schema.go).

use std::sync::Arc;

use bytes::Bytes;

use crate::abi::{self, Envelope, method};
use crate::api::PluginMetadata;
use crate::client::{GuardError, GuardedClient};
use crate::go_struct;
use crate::gojson::{self, GoJson};

/// A failed plugin call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The plugin answered with an error envelope (Go `rpcError`).
    Rpc { code: String, message: String, status: u16 },
    /// The client is closed, the call failed below RPC, or the response did not decode.
    Other(String),
    /// Host code panicked; the caller fuses the plugin.
    Panic(String),
}

impl CallError {
    /// Go `clienterror.HTTPStatusFromError`: the status a plugin error asks for, if any.
    pub fn status(&self) -> u16 {
        match self {
            CallError::Rpc { status, .. } => *status,
            _ => 0,
        }
    }

    pub fn code(&self) -> &str {
        match self {
            CallError::Rpc { code, .. } => code,
            _ => "",
        }
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Rpc { message, .. } => f.write_str(message),
            CallError::Other(message) => f.write_str(message),
            CallError::Panic(message) => write!(f, "plugin panic: {message}"),
        }
    }
}

impl std::error::Error for CallError {}

impl From<GuardError> for CallError {
    fn from(e: GuardError) -> Self {
        match e {
            GuardError::Panic(p) => CallError::Panic(p),
            other => CallError::Other(other.to_string()),
        }
    }
}

/// Go `callPlugin[T]`: encode, call, decode the envelope and the result.
pub async fn call<T: GoJson, R: GoJson>(
    client: &Arc<GuardedClient>,
    method: &str,
    request: &R,
) -> Result<T, CallError> {
    call_raw(client, method, gojson::to_vec(request)).await
}

/// [`call`] with an already encoded request.
pub async fn call_raw<T: GoJson>(client: &Arc<GuardedClient>, method: &str, request: Vec<u8>) -> Result<T, CallError> {
    let raw = client.call(method, request).await?;
    decode_response(method, &raw)
}

/// `rpc<X>Request`: a request struct's fields followed by `host_callback_id`
/// (omitted when empty), as Go's embedding wrappers encode.
pub fn encode_with_callback<R: gojson::GoStruct>(request: &R, callback_id: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = gojson::ObjWriter::begin(&mut out);
    w.embed(request);
    w.field("host_callback_id", &callback_id.to_owned(), true);
    w.end();
    out
}

/// Go `decodeEnvelopeResult` after the envelope decode in `callPlugin`.
pub fn decode_response<T: GoJson>(method: &str, raw: &[u8]) -> Result<T, CallError> {
    let envelope =
        Envelope::parse(raw).map_err(|e| CallError::Other(format!("decode plugin envelope {method}: {e}")))?;
    decode_envelope(method, envelope)
}

pub fn decode_envelope<T: GoJson>(method: &str, envelope: Envelope) -> Result<T, CallError> {
    if !envelope.ok {
        let Some(error) = envelope.error else {
            return Err(CallError::Other("plugin call failed".into()));
        };
        let message = error.message.trim();
        return Err(CallError::Rpc {
            code: error.code.trim().to_owned(),
            message: if message.is_empty() {
                "plugin call failed".into()
            } else {
                message.to_owned()
            },
            status: u16::try_from(error.http_status).unwrap_or(0),
        });
    }
    match envelope.result {
        None => Ok(T::default()),
        Some(result) => T::decode(&result).map_err(|e| CallError::Other(format!("decode plugin result {method}: {e}"))),
    }
}

go_struct! {
    /// `rpcLifecycleRequest`.
    pub struct LifecycleRequest("pluginhost.rpcLifecycleRequest") {
        "config_yaml" => config_yaml: Bytes,
        "schema_version" => schema_version: u32,
    }
}

go_struct! {
    /// `rpcCapabilities`.
    pub struct RpcCapabilities("pluginhost.rpcCapabilities") {
        "model_registrar" => model_registrar: bool,
        "model_provider" => model_provider: bool,
        "auth_provider" => auth_provider: bool,
        "frontend_auth_provider" => frontend_auth_provider: bool,
        "frontend_auth_provider_exclusive" => frontend_auth_provider_exclusive: bool,
        "scheduler" => scheduler: bool,
        "scheduler_across_priorities" omitempty => scheduler_across_priorities: bool,
        "model_router" => model_router: bool,
        "executor" => executor: bool,
        "executor_model_scope" => executor_model_scope: String,
        "executor_input_formats" omitempty => executor_input_formats: Vec<String>,
        "executor_output_formats" omitempty => executor_output_formats: Vec<String>,
        "request_translator" => request_translator: bool,
        "request_normalizer" => request_normalizer: bool,
        "request_interceptor" => request_interceptor: bool,
        "request_lifecycle_plugin" => request_lifecycle_plugin: bool,
        "response_translator" => response_translator: bool,
        "response_before_translator" => response_before_translator: bool,
        "response_after_translator" => response_after_translator: bool,
        "response_interceptor" => response_interceptor: bool,
        "response_stream_interceptor" => stream_chunk_interceptor: bool,
        "websocket_response_observer" => websocket_response_observer: bool,
        "thinking_applier" => thinking_applier: bool,
        "usage_plugin" => usage_plugin: bool,
        "command_line_plugin" => command_line_plugin: bool,
        "management_api" => management_api: bool,
        "quota_provider" => quota_provider: bool,
    }
}

go_struct! {
    /// `rpcRegistration`.
    pub struct Registration("pluginhost.rpcRegistration") {
        "schema_version" => schema_version: u32,
        "metadata" => metadata: PluginMetadata,
        "capabilities" => capabilities: RpcCapabilities,
    }
}

go_struct! {
    pub struct IdentifierResponse("pluginhost.rpcIdentifierResponse") {
        "identifier" => identifier: String,
    }
}

go_struct! {
    /// `rpcEmptyResponse`: encodes `{}`, decodes anything.
    pub struct Empty("pluginhost.rpcEmptyResponse") {}
}

/// `ExecutorModelScope`.
pub const SCOPE_BOTH: &str = "both";
pub const SCOPE_STATIC: &str = "static";
pub const SCOPE_OAUTH: &str = "oauth";

/// The host-side view of a registered plugin (Go `pluginapi.Plugin` built by
/// `registerRPCPlugin`). Every capability is an RPC adapter over the same client.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plugin {
    pub metadata: PluginMetadata,
    /// Negotiated schema; a plugin that omits it speaks schema 1.
    pub schema_version: u32,
    pub caps: RpcCapabilities,
    /// `auth.identifier`, asked once at registration (empty on failure).
    pub auth_identifier: String,
    /// `quota.identifier`, asked once at registration.
    pub quota_identifier: String,
}

impl Plugin {
    /// Go `normalizedExecutorModelScope`.
    pub fn executor_model_scope(&self) -> &str {
        if !self.caps.executor {
            return SCOPE_BOTH;
        }
        match self.caps.executor_model_scope.as_str() {
            s @ (SCOPE_STATIC | SCOPE_OAUTH | SCOPE_BOTH) => s,
            _ => SCOPE_BOTH,
        }
    }

    pub fn allows_static_models(&self) -> bool {
        !self.caps.executor || matches!(self.executor_model_scope(), SCOPE_STATIC | SCOPE_BOTH)
    }

    pub fn allows_oauth_models(&self) -> bool {
        !self.caps.executor || matches!(self.executor_model_scope(), SCOPE_OAUTH | SCOPE_BOTH)
    }

    /// Go `validPlugin`: complete metadata and at least one capability.
    pub fn is_valid(&self) -> bool {
        let m = &self.metadata;
        let filled = |s: &str| !s.trim().is_empty();
        if !(filled(&m.name) && filled(&m.version) && filled(&m.author) && filled(&m.github_repository)) {
            return false;
        }
        let c = &self.caps;
        c.model_registrar
            || c.model_provider
            || c.auth_provider
            || c.frontend_auth_provider
            || c.scheduler
            || c.model_router
            || c.executor
            || c.request_translator
            || c.request_normalizer
            || c.request_interceptor
            || c.request_lifecycle_plugin
            || c.response_translator
            || c.response_before_translator
            || c.response_after_translator
            || c.response_interceptor
            || c.stream_chunk_interceptor
            || c.websocket_response_observer
            || c.thinking_applier
            || c.usage_plugin
            || c.command_line_plugin
            || c.management_api
            || c.quota_provider
    }
}

/// Go `registerRPCPlugin`: `plugin.register` or `plugin.reconfigure`, then the identifier
/// calls registration makes eagerly.
pub async fn register(client: &Arc<GuardedClient>, method: &str, config_yaml: &[u8]) -> Result<Plugin, CallError> {
    let request = LifecycleRequest {
        config_yaml: Bytes::copy_from_slice(config_yaml),
        schema_version: abi::SCHEMA_VERSION,
    };
    let resp: Registration = call(client, method, &request).await?;
    if resp.schema_version > abi::SCHEMA_VERSION {
        return Err(CallError::Other(format!(
            "plugin schema version {} is not supported",
            resp.schema_version
        )));
    }
    let mut caps = resp.capabilities;
    caps.frontend_auth_provider_exclusive &= caps.frontend_auth_provider;
    caps.scheduler_across_priorities &= caps.scheduler;
    let mut plugin = Plugin {
        metadata: resp.metadata,
        schema_version: resp.schema_version.max(1),
        caps,
        ..Default::default()
    };
    if plugin.caps.auth_provider {
        plugin.auth_identifier = identifier(client, method::AUTH_IDENTIFIER).await;
    }
    if plugin.caps.quota_provider {
        plugin.quota_identifier = identifier(client, method::QUOTA_IDENTIFIER).await;
    }
    Ok(plugin)
}

/// Go `callPluginIdentifier`: trimmed, empty on any failure.
pub async fn identifier(client: &Arc<GuardedClient>, method: &str) -> String {
    match call::<IdentifierResponse, _>(client, method, &Empty {}).await {
        Ok(resp) => resp.identifier.trim().to_owned(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_response_follows_go_rpc_errors() {
        let err = decode_response::<Empty>(
            "m",
            br#"{"ok":false,"error":{"code":" c ","message":"  ","http_status":429}}"#,
        )
        .unwrap_err();
        assert_eq!(
            err,
            CallError::Rpc {
                code: "c".into(),
                message: "plugin call failed".into(),
                status: 429
            }
        );
        assert_eq!(
            decode_response::<Empty>("m", br#"{"ok":false}"#).unwrap_err(),
            CallError::Other("plugin call failed".into())
        );
        assert!(
            decode_response::<IdentifierResponse>("m", b"nope")
                .unwrap_err()
                .to_string()
                .starts_with("decode plugin envelope m: ")
        );
        let id: IdentifierResponse = decode_response("m", br#"{"ok":true}"#).unwrap();
        assert_eq!(id.identifier, "");
    }

    #[test]
    fn lifecycle_request_matches_go_bytes() {
        let req = LifecycleRequest {
            config_yaml: Bytes::from_static(b"enabled: true\n"),
            schema_version: 6,
        };
        assert_eq!(
            gojson::to_vec(&req),
            br#"{"config_yaml":"ZW5hYmxlZDogdHJ1ZQo=","schema_version":6}"#
        );
        assert_eq!(gojson::to_vec(&Empty {}), b"{}");
    }
}
