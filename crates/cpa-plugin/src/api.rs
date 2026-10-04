//! `sdk/pluginapi` request and response schemas, field for field. Wire names are the Go
//! field names unless Go declares a json tag. Host-only members (`HTTPClient`, handler
//! interfaces) are not part of the wire format and are left out.

use std::collections::BTreeMap;

use bytes::Bytes;

use crate::go_struct;
use crate::gojson::{GoJson, GoTime, Header, Metadata, Node, NonNil, NonNilBytes, RawJson, StringMap};

go_struct! {
    pub struct ConfigField("pluginapi.ConfigField") {
        "Name" => name: String,
        "Type" => field_type: String,
        "EnumValues" => enum_values: Vec<String>,
        "Description" => description: String,
    }
}

go_struct! {
    /// `pluginapi.Metadata`.
    pub struct PluginMetadata("pluginapi.Metadata") {
        "Name" => name: String,
        "Version" => version: String,
        "Author" => author: String,
        "GitHubRepository" => github_repository: String,
        "Logo" => logo: String,
        // A plugin's empty list stays `[]` (Go decodes it into a non-nil slice).
        "ConfigFields" => config_fields: Option<NonNil<Vec<ConfigField>>>,
    }
}

impl PluginMetadata {
    /// The declared configuration fields (none for nil or empty).
    pub fn config_field_list(&self) -> &[ConfigField] {
        self.config_fields.as_ref().map_or(&[], |fields| &fields.0)
    }
}

go_struct! {
    pub struct ThinkingSupport("pluginapi.ThinkingSupport") {
        "Min" => min: i64,
        "Max" => max: i64,
        "ZeroAllowed" => zero_allowed: bool,
        "DynamicAllowed" => dynamic_allowed: bool,
        "Levels" => levels: Vec<String>,
    }
}

go_struct! {
    pub struct ModelInfo("pluginapi.ModelInfo") {
        "ID" => id: String,
        "Object" => object: String,
        "Created" => created: i64,
        "OwnedBy" => owned_by: String,
        "Type" => model_type: String,
        "DisplayName" => display_name: String,
        "Name" => name: String,
        "Version" => version: String,
        "Description" => description: String,
        "InputTokenLimit" => input_token_limit: i64,
        "OutputTokenLimit" => output_token_limit: i64,
        "SupportedGenerationMethods" => supported_generation_methods: Vec<String>,
        "ContextLength" => context_length: i64,
        "MaxCompletionTokens" => max_completion_tokens: i64,
        "SupportedParameters" => supported_parameters: Vec<String>,
        "SupportedInputModalities" => supported_input_modalities: Vec<String>,
        "SupportedOutputModalities" => supported_output_modalities: Vec<String>,
        "Thinking" => thinking: Option<ThinkingSupport>,
        "UserDefined" => user_defined: bool,
    }
}

go_struct! {
    pub struct ModelAlias("pluginapi.ModelAlias") {
        "Name" => name: String,
        "Alias" => alias: String,
    }
}

go_struct! {
    pub struct HostConfigSummary("pluginapi.HostConfigSummary") {
        "AuthDir" => auth_dir: String,
        "ProxyURL" => proxy_url: String,
        "ForceModelPrefix" => force_model_prefix: bool,
        "OAuthModelAlias" => oauth_model_alias: BTreeMap<String, Vec<ModelAlias>>,
        "ExcludedModels" => excluded_models: BTreeMap<String, Vec<String>>,
    }
}

go_struct! {
    pub struct AuthData("pluginapi.AuthData") {
        "Provider" => provider: String,
        "ID" => id: String,
        "FileName" => file_name: String,
        "Label" => label: String,
        "Prefix" => prefix: String,
        "ProxyURL" => proxy_url: String,
        "Disabled" => disabled: bool,
        "StorageJSON" => storage_json: Bytes,
        "Metadata" => metadata: Metadata,
        "Attributes" => attributes: StringMap,
        "NextRefreshAfter" => next_refresh_after: GoTime,
    }
}

go_struct! {
    pub struct AuthParseRequest("pluginapi.AuthParseRequest") {
        "Provider" => provider: String,
        "Path" => path: String,
        "FileName" => file_name: String,
        "RawJSON" => raw_json: Bytes,
        "Host" => host: HostConfigSummary,
    }
}

go_struct! {
    pub struct AuthParseResponse("pluginapi.AuthParseResponse") {
        "Handled" => handled: bool,
        "Auth" => auth: AuthData,
        "Auths" => auths: Vec<AuthData>,
    }
}

go_struct! {
    pub struct AuthLoginStartRequest("pluginapi.AuthLoginStartRequest") {
        "Provider" => provider: String,
        "BaseURL" => base_url: String,
        "Host" => host: HostConfigSummary,
        "Metadata" => metadata: Metadata,
    }
}

go_struct! {
    pub struct AuthLoginStartResponse("pluginapi.AuthLoginStartResponse") {
        "Provider" => provider: String,
        "URL" => url: String,
        "State" => state: String,
        "ExpiresAt" => expires_at: GoTime,
        "Metadata" => metadata: Metadata,
    }
}

go_struct! {
    pub struct AuthLoginPollRequest("pluginapi.AuthLoginPollRequest") {
        "Provider" => provider: String,
        "State" => state: String,
        "Host" => host: HostConfigSummary,
        "Metadata" => metadata: Metadata,
    }
}

pub const AUTH_LOGIN_PENDING: &str = "pending";
pub const AUTH_LOGIN_SUCCESS: &str = "success";
pub const AUTH_LOGIN_ERROR: &str = "error";

go_struct! {
    pub struct AuthLoginPollResponse("pluginapi.AuthLoginPollResponse") {
        "Status" => status: String,
        "Message" => message: String,
        "Auth" => auth: AuthData,
        "Auths" => auths: Vec<AuthData>,
    }
}

go_struct! {
    pub struct AuthRefreshRequest("pluginapi.AuthRefreshRequest") {
        "AuthID" => auth_id: String,
        "AuthProvider" => auth_provider: String,
        "StorageJSON" => storage_json: Bytes,
        "Metadata" => metadata: Metadata,
        "Attributes" => attributes: StringMap,
        "Host" => host: HostConfigSummary,
    }
}

go_struct! {
    pub struct AuthRefreshResponse("pluginapi.AuthRefreshResponse") {
        "Auth" => auth: AuthData,
        "NextRefreshAfter" => next_refresh_after: GoTime,
    }
}

go_struct! {
    pub struct ModelRegistrationRequest("pluginapi.ModelRegistrationRequest") {
        "Plugin" => plugin: PluginMetadata,
    }
}

go_struct! {
    pub struct ModelRegistrationResponse("pluginapi.ModelRegistrationResponse") {
        "Provider" => provider: String,
        "Models" => models: Vec<ModelInfo>,
    }
}

go_struct! {
    pub struct StaticModelRequest("pluginapi.StaticModelRequest") {
        "Plugin" => plugin: PluginMetadata,
        "Host" => host: HostConfigSummary,
    }
}

go_struct! {
    pub struct AuthModelRequest("pluginapi.AuthModelRequest") {
        "Plugin" => plugin: PluginMetadata,
        "AuthID" => auth_id: String,
        "AuthProvider" => auth_provider: String,
        "StorageJSON" => storage_json: Bytes,
        "Metadata" => metadata: Metadata,
        "Attributes" => attributes: StringMap,
        "Host" => host: HostConfigSummary,
    }
}

go_struct! {
    pub struct ModelResponse("pluginapi.ModelResponse") {
        "Provider" => provider: String,
        "Models" => models: Vec<ModelInfo>,
        "AuthUpdate" => auth_update: AuthData,
    }
}

go_struct! {
    pub struct FrontendAuthRequest("pluginapi.FrontendAuthRequest") {
        "Method" => method: String,
        "Path" => path: String,
        "Headers" => headers: Header,
        "Query" => query: Header,
        /// Read with `io.ReadAll`, so never nil.
        "Body" => body: NonNilBytes,
    }
}

go_struct! {
    pub struct FrontendAuthResponse("pluginapi.FrontendAuthResponse") {
        "Authenticated" => authenticated: bool,
        "Principal" => principal: String,
        "Metadata" => metadata: StringMap,
    }
}

pub const SCHEDULER_BUILTIN_ROUND_ROBIN: &str = "round-robin";
pub const SCHEDULER_BUILTIN_FILL_FIRST: &str = "fill-first";

go_struct! {
    pub struct SchedulerOptions("pluginapi.SchedulerOptions") {
        "Headers" => headers: Header,
        "Metadata" => metadata: Metadata,
    }
}

go_struct! {
    pub struct SchedulerAuthCandidate("pluginapi.SchedulerAuthCandidate") {
        "ID" => id: String,
        "Provider" => provider: String,
        "Priority" => priority: i64,
        "Status" => status: String,
        "Attributes" => attributes: StringMap,
        "Metadata" => metadata: Metadata,
    }
}

go_struct! {
    pub struct SchedulerPickRequest("pluginapi.SchedulerPickRequest") {
        "Plugin" => plugin: PluginMetadata,
        "Provider" => provider: String,
        "Providers" => providers: Vec<String>,
        "Model" => model: String,
        "Stream" => stream: bool,
        "Options" => options: SchedulerOptions,
        "Candidates" => candidates: Vec<SchedulerAuthCandidate>,
    }
}

/// `pluginapi.SchedulerPickResponse`. Its custom `UnmarshalJSON` accepts the Go names
/// and snake_case alternatives; a present Go name wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchedulerPickResponse {
    pub auth_id: String,
    pub delegate_builtin: String,
    pub handled: bool,
    pub reject: bool,
    pub reject_reason: String,
    pub reject_code: String,
}

go_struct! {
    pub struct SchedulerPickRaw("pluginapi.rawResponse") {
        "AuthID" => auth_id: Option<String>,
        "auth_id" => alt_auth_id: Option<String>,
        "DelegateBuiltin" => delegate: Option<String>,
        "delegate_builtin" => alt_delegate: Option<String>,
        "Handled" => handled: Option<bool>,
        "handled" => alt_handled: Option<bool>,
        "Reject" => reject: Option<bool>,
        "reject" => alt_reject: Option<bool>,
        "RejectReason" => reject_reason: Option<String>,
        "reject_reason" => alt_reject_reason: Option<String>,
        "RejectCode" => reject_code: Option<String>,
        "reject_code" => alt_reject_code: Option<String>,
    }
}

impl GoJson for SchedulerPickResponse {
    const GO_TYPE: &'static str = "pluginapi.SchedulerPickResponse";
    fn encode(&self, out: &mut Vec<u8>) {
        let mut w = crate::gojson::ObjWriter::begin(out);
        w.field("AuthID", &self.auth_id, false);
        w.field("DelegateBuiltin", &self.delegate_builtin, false);
        w.field("Handled", &self.handled, false);
        w.field("Reject", &self.reject, false);
        w.field("RejectReason", &self.reject_reason, false);
        w.field("RejectCode", &self.reject_code, false);
        w.end();
    }
    fn decode_value(v: &crate::gojson::Node) -> Result<Self, crate::gojson::DecodeError> {
        let raw: SchedulerPickRaw = crate::gojson::decode_struct(v)?;
        Ok(Self {
            auth_id: raw.auth_id.or(raw.alt_auth_id).unwrap_or_default(),
            delegate_builtin: raw.delegate.or(raw.alt_delegate).unwrap_or_default(),
            handled: raw.handled.or(raw.alt_handled).unwrap_or_default(),
            reject: raw.reject.or(raw.alt_reject).unwrap_or_default(),
            reject_reason: raw.reject_reason.or(raw.alt_reject_reason).unwrap_or_default(),
            reject_code: raw.reject_code.or(raw.alt_reject_code).unwrap_or_default(),
        })
    }
    fn is_empty(&self) -> bool {
        false
    }
}

go_struct! {
    pub struct ModelRouteRequest("pluginapi.ModelRouteRequest") {
        "Plugin" => plugin: PluginMetadata,
        "PluginID" => plugin_id: String,
        "SourceFormat" => source_format: String,
        "RequestedModel" => requested_model: String,
        "Stream" => stream: bool,
        "Headers" => headers: Header,
        "Query" => query: Header,
        "Body" => body: Bytes,
        "Metadata" => metadata: Metadata,
        "AvailableProviders" => available_providers: Vec<String>,
    }
}

pub const ROUTE_TARGET_SELF: &str = "self";
pub const ROUTE_TARGET_EXECUTOR: &str = "executor";
pub const ROUTE_TARGET_PROVIDER: &str = "provider";

go_struct! {
    pub struct ModelRouteResponse("pluginapi.ModelRouteResponse") {
        "Handled" => handled: bool,
        "TargetKind" => target_kind: String,
        "Target" => target: String,
        "TargetModel" => target_model: String,
        "Reason" => reason: String,
    }
}

go_struct! {
    pub struct HostModelExecutionRequest("pluginapi.HostModelExecutionRequest") {
        "entry_protocol" => entry_protocol: String,
        "exit_protocol" => exit_protocol: String,
        "model" => model: String,
        "stream" => stream: bool,
        "body" => body: Bytes,
        "headers" => headers: Header,
        "query" => query: Header,
        "alt" => alt: String,
        "forced_provider" omitempty => forced_provider: String,
        "auth_id" omitempty => auth_id: String,
        "proxy_url" omitempty => proxy_url: String,
        "path" omitempty => path: String,
    }
}

go_struct! {
    pub struct HostModelExecutionResponse("pluginapi.HostModelExecutionResponse") {
        "status_code" => status_code: i64,
        "headers" => headers: Header,
        "body" => body: Bytes,
    }
}

go_struct! {
    pub struct HostModelStreamResponse("pluginapi.HostModelStreamResponse") {
        "status_code" => status_code: i64,
        "headers" => headers: Header,
        "stream_id" => stream_id: String,
    }
}

go_struct! {
    pub struct HostModelStreamReadRequest("pluginapi.HostModelStreamReadRequest") {
        "stream_id" => stream_id: String,
    }
}

go_struct! {
    pub struct HostModelStreamReadResponse("pluginapi.HostModelStreamReadResponse") {
        "payload" => payload: Bytes,
        "error" => error: String,
        "done" => done: bool,
    }
}

go_struct! {
    pub struct HostModelStreamCloseRequest("pluginapi.HostModelStreamCloseRequest") {
        "stream_id" => stream_id: String,
    }
}

go_struct! {
    pub struct HostRecentRequestEntry("pluginapi.HostRecentRequestEntry") {
        "time" => time: String,
        "success" => success: i64,
        "failed" => failed: i64,
    }
}

go_struct! {
    pub struct HostAuthFileEntry("pluginapi.HostAuthFileEntry") {
        "id" omitempty => id: String,
        "auth_index" omitempty => auth_index: String,
        "name" => name: String,
        "type" omitempty => auth_type: String,
        "provider" omitempty => provider: String,
        "label" omitempty => label: String,
        "status" omitempty => status: String,
        "status_message" omitempty => status_message: String,
        "disabled" omitempty => disabled: bool,
        "unavailable" omitempty => unavailable: bool,
        "runtime_only" omitempty => runtime_only: bool,
        "source" omitempty => source: String,
        "path" omitempty => path: String,
        "size" omitempty => size: i64,
        "modtime" omitempty => mod_time: GoTime,
        "updated_at" omitempty => updated_at: GoTime,
        "created_at" omitempty => created_at: GoTime,
        "last_refresh" omitempty => last_refresh: GoTime,
        "next_retry_after" omitempty => next_retry_after: GoTime,
        "email" omitempty => email: String,
        "project_id" omitempty => project_id: String,
        "account_type" omitempty => account_type: String,
        "account" omitempty => account: String,
        "priority" omitempty => priority: i64,
        "note" omitempty => note: String,
        "base_url" omitempty => base_url: String,
        "websockets" omitempty => websockets: bool,
        "success" omitempty => success: i64,
        "failed" omitempty => failed: i64,
        "recent_requests" omitempty => recent_requests: Vec<HostRecentRequestEntry>,
    }
}

go_struct! {
    pub struct HostAuthGetRequest("pluginapi.HostAuthGetRequest") {
        "auth_index" => auth_index: String,
    }
}

go_struct! {
    pub struct HostAuthGetResponse("pluginapi.HostAuthGetResponse") {
        "auth_index" => auth_index: String,
        "name" omitempty => name: String,
        "path" omitempty => path: String,
        "json" => json: RawJson,
    }
}

go_struct! {
    pub struct HostAuthGetRuntimeResponse("pluginapi.HostAuthGetRuntimeResponse") {
        "auth" => auth: HostAuthFileEntry,
    }
}

go_struct! {
    pub struct HostAuthSaveRequest("pluginapi.HostAuthSaveRequest") {
        "name" => name: String,
        "json" => json: RawJson,
    }
}

go_struct! {
    pub struct HostAuthSaveResponse("pluginapi.HostAuthSaveResponse") {
        "name" => name: String,
        "path" => path: String,
    }
}

pub const AFFINITY_BOUND: &str = "bound";
pub const AFFINITY_UNBOUND: &str = "unbound";
pub const AFFINITY_AMBIGUOUS: &str = "ambiguous";
pub const AFFINITY_UNSUPPORTED: &str = "unsupported";

go_struct! {
    pub struct HostAffinityLookupRequest("pluginapi.HostAffinityLookupRequest") {
        "provider" => provider: String,
        "model" => model: String,
        "session_id" => session_id: String,
    }
}

go_struct! {
    pub struct HostAffinityLookupResponse("pluginapi.HostAffinityLookupResponse") {
        "status" => status: String,
        "auth_index" omitempty => auth_index: String,
        "observed_at" omitempty => observed_at: GoTime,
        "disabled" omitempty => disabled: bool,
        "unavailable" omitempty => unavailable: bool,
    }
}

go_struct! {
    pub struct HttpWireProfile("pluginapi.HTTPWireProfile") {
        "http1_only" omitempty => http1_only: bool,
        "disable_auto_compression" omitempty => disable_auto_compression: bool,
        "header_profile" omitempty => header_profile: Vec<String>,
    }
}

go_struct! {
    pub struct HttpRequest("pluginapi.HTTPRequest") {
        "Method" => method: String,
        "URL" => url: String,
        "Headers" => headers: Header,
        "Body" => body: Bytes,
        "wire_profile" omitempty => wire_profile: Option<HttpWireProfile>,
    }
}

go_struct! {
    pub struct HttpResponse("pluginapi.HTTPResponse") {
        "StatusCode" => status_code: i64,
        "Headers" => headers: Header,
        "Body" => body: Bytes,
    }
}

go_struct! {
    /// `pluginapi.HTTPStreamChunk`. `Err` is a Go `error` interface: it encodes as `{}`
    /// or `null` and never decodes back, so only the payload crosses the wire.
    pub struct HttpStreamChunk("pluginapi.HTTPStreamChunk") {
        "Payload" => payload: Bytes,
    }
}

go_struct! {
    pub struct ExecutorHttpRequest("pluginapi.ExecutorHTTPRequest") {
        "AuthID" => auth_id: String,
        "AuthProvider" => auth_provider: String,
        "Method" => method: String,
        "URL" => url: String,
        "Headers" => headers: Header,
        "Body" => body: Bytes,
        "StorageJSON" => storage_json: Bytes,
        "Metadata" => metadata: Metadata,
        "Attributes" => attributes: StringMap,
    }
}

go_struct! {
    pub struct ExecutorHttpResponse("pluginapi.ExecutorHTTPResponse") {
        "StatusCode" => status_code: i64,
        "Headers" => headers: Header,
        "Body" => body: Bytes,
    }
}

go_struct! {
    pub struct ExecutorRequest("pluginapi.ExecutorRequest") {
        "AuthID" => auth_id: String,
        "AuthProvider" => auth_provider: String,
        "Model" => model: String,
        "Format" => format: String,
        "Stream" => stream: bool,
        "Alt" => alt: String,
        "Headers" => headers: Header,
        "Query" => query: Header,
        "OriginalRequest" => original_request: Bytes,
        "SourceFormat" => source_format: String,
        "Payload" => payload: Bytes,
        "Metadata" => metadata: Metadata,
        "StorageJSON" => storage_json: Bytes,
        "AuthMetadata" => auth_metadata: Metadata,
        "AuthAttributes" => auth_attributes: StringMap,
    }
}

go_struct! {
    pub struct ExecutorResponse("pluginapi.ExecutorResponse") {
        "Payload" => payload: Bytes,
        "Headers" => headers: Header,
        "Metadata" => metadata: Metadata,
    }
}

go_struct! {
    /// `pluginapi.ExecutorStreamChunk`; see [`HttpStreamChunk`] for `Err`.
    pub struct ExecutorStreamChunk("pluginapi.ExecutorStreamChunk") {
        "Payload" => payload: Bytes,
    }
}

go_struct! {
    pub struct RequestTransformRequest("pluginapi.RequestTransformRequest") {
        "FromFormat" => from_format: String,
        "ToFormat" => to_format: String,
        "Model" => model: String,
        "Stream" => stream: bool,
        "Body" => body: Bytes,
    }
}

go_struct! {
    pub struct ResponseTransformRequest("pluginapi.ResponseTransformRequest") {
        "FromFormat" => from_format: String,
        "ToFormat" => to_format: String,
        "Model" => model: String,
        "Stream" => stream: bool,
        "OriginalRequest" => original_request: Bytes,
        "TranslatedRequest" => translated_request: Bytes,
        "Body" => body: Bytes,
    }
}

go_struct! {
    pub struct RequestInterceptRequest("pluginapi.RequestInterceptRequest") {
        "RequestID" => request_id: String,
        "TraceID" => trace_id: String,
        "SourceFormat" => source_format: String,
        "ToFormat" => to_format: String,
        "Model" => model: String,
        "RequestedModel" => requested_model: String,
        "Stream" => stream: bool,
        "Headers" => headers: Header,
        "Body" => body: Bytes,
        "Metadata" => metadata: Metadata,
    }
}

go_struct! {
    pub struct RequestInterceptResponse("pluginapi.RequestInterceptResponse") {
        "path" omitempty => path: String,
        "Headers" => headers: Header,
        "Body" => body: Bytes,
        "ClearHeaders" => clear_headers: Vec<String>,
        "Terminate" => terminate: bool,
        "StatusCode" => status_code: i64,
        "ResponseHeaders" => response_headers: Header,
        "ResponseBody" => response_body: Bytes,
    }
}

pub const COMPLETION_SUCCEEDED: &str = "succeeded";
pub const COMPLETION_FAILED: &str = "failed";
pub const COMPLETION_REJECTED: &str = "rejected";
pub const COMPLETION_CANCELED: &str = "canceled";

go_struct! {
    pub struct RequestCompletion("pluginapi.RequestCompletion") {
        "RequestID" => request_id: String,
        "TraceID" => trace_id: String,
        "SourceFormat" => source_format: String,
        "Model" => model: String,
        "RequestedModel" => requested_model: String,
        "Stream" => stream: bool,
        "Outcome" => outcome: String,
        "StatusCode" => status_code: i64,
        "Error" => error: String,
        "StartedAt" => started_at: GoTime,
        "CompletedAt" => completed_at: GoTime,
        "Metadata" => metadata: Metadata,
    }
}

go_struct! {
    pub struct ResponseInterceptRequest("pluginapi.ResponseInterceptRequest") {
        "RequestID" => request_id: String,
        "SourceFormat" => source_format: String,
        "Model" => model: String,
        "RequestedModel" => requested_model: String,
        "Stream" => stream: bool,
        "RequestHeaders" => request_headers: Header,
        "ResponseHeaders" => response_headers: Header,
        "OriginalRequest" => original_request: Bytes,
        "RequestBody" => request_body: Bytes,
        "Body" => body: Bytes,
        "StatusCode" => status_code: i64,
        "Metadata" => metadata: Metadata,
    }
}

go_struct! {
    pub struct ResponseInterceptResponse("pluginapi.ResponseInterceptResponse") {
        "Headers" => headers: Header,
        "Body" => body: Bytes,
        "ClearHeaders" => clear_headers: Vec<String>,
    }
}

/// `pluginapi.StreamChunkHeaderInitIndex`.
pub const STREAM_CHUNK_HEADER_INIT_INDEX: i64 = -1;

go_struct! {
    pub struct StreamChunkInterceptRequest("pluginapi.StreamChunkInterceptRequest") {
        "RequestID" => request_id: String,
        "SourceFormat" => source_format: String,
        "Model" => model: String,
        "RequestedModel" => requested_model: String,
        "RequestHeaders" => request_headers: Header,
        "ResponseHeaders" => response_headers: Header,
        "OriginalRequest" => original_request: Bytes,
        "RequestBody" => request_body: Bytes,
        "Body" => body: Bytes,
        "HistoryChunks" => history_chunks: Vec<Bytes>,
        "ChunkIndex" => chunk_index: i64,
        "Metadata" => metadata: Metadata,
    }
}

go_struct! {
    pub struct StreamChunkInterceptResponse("pluginapi.StreamChunkInterceptResponse") {
        "Headers" => headers: Header,
        "Body" => body: Bytes,
        "ClearHeaders" => clear_headers: Vec<String>,
        "DropChunk" => drop_chunk: bool,
    }
}

go_struct! {
    pub struct WebSocketResponseEvent("pluginapi.WebSocketResponseEvent") {
        "RequestID" => request_id: String,
        "TraceID" => trace_id: String,
        "SourceFormat" => source_format: String,
        "Model" => model: String,
        "RequestedModel" => requested_model: String,
        "Provider" => provider: String,
        "AuthID" => auth_id: String,
        "AuthLabel" => auth_label: String,
        "AuthType" => auth_type: String,
        "EventType" => event_type: String,
        "Payload" => payload: Bytes,
        "Metadata" => metadata: Metadata,
    }
}

go_struct! {
    pub struct PayloadResponse("pluginapi.PayloadResponse") {
        "Body" => body: Bytes,
    }
}

go_struct! {
    pub struct ThinkingConfig("pluginapi.ThinkingConfig") {
        "Mode" => mode: String,
        "Budget" => budget: i64,
        "Level" => level: String,
    }
}

go_struct! {
    pub struct ThinkingApplyRequest("pluginapi.ThinkingApplyRequest") {
        "Provider" => provider: String,
        "Model" => model: ModelInfo,
        "Config" => config: ThinkingConfig,
        "Body" => body: Bytes,
    }
}

go_struct! {
    pub struct CommandLineRegistrationRequest("pluginapi.CommandLineRegistrationRequest") {
        "Plugin" => plugin: PluginMetadata,
    }
}

go_struct! {
    pub struct CommandLineFlag("pluginapi.CommandLineFlag") {
        "Name" => name: String,
        "Usage" => usage: String,
        "Type" => flag_type: String,
        "DefaultValue" => default_value: String,
    }
}

go_struct! {
    pub struct CommandLineRegistrationResponse("pluginapi.CommandLineRegistrationResponse") {
        "Flags" => flags: Vec<CommandLineFlag>,
    }
}

go_struct! {
    pub struct CommandLineFlagValue("pluginapi.CommandLineFlagValue") {
        "Name" => name: String,
        "Type" => flag_type: String,
        "Value" => value: String,
        "Set" => set: bool,
    }
}

go_struct! {
    pub struct CommandLineExecutionRequest("pluginapi.CommandLineExecutionRequest") {
        "Plugin" => plugin: PluginMetadata,
        "Program" => program: String,
        "Args" => args: Vec<String>,
        "ConfigPath" => config_path: String,
        "Host" => host: HostConfigSummary,
        "Flags" => flags: BTreeMap<String, CommandLineFlagValue>,
        "TriggeredFlags" => triggered_flags: BTreeMap<String, CommandLineFlagValue>,
    }
}

go_struct! {
    pub struct CommandLineExecutionResponse("pluginapi.CommandLineExecutionResponse") {
        "Stdout" => stdout: Bytes,
        "Stderr" => stderr: Bytes,
        "Auths" => auths: Vec<AuthData>,
        "ExitCode" => exit_code: i64,
    }
}

go_struct! {
    pub struct ManagementRegistrationRequest("pluginapi.ManagementRegistrationRequest") {
        "Plugin" => plugin: PluginMetadata,
        "BasePath" => base_path: String,
        "ResourceBasePath" => resource_base_path: String,
    }
}

go_struct! {
    /// `pluginapi.ManagementRoute` without its host-side `Handler`.
    pub struct ManagementRoute("pluginapi.ManagementRoute") {
        "Method" => method: String,
        "Path" => path: String,
        "Menu" => menu: String,
        "Description" => description: String,
    }
}

go_struct! {
    /// `pluginapi.ResourceRoute` without its host-side `Handler`.
    pub struct ResourceRoute("pluginapi.ResourceRoute") {
        "Path" => path: String,
        "Menu" => menu: String,
        "Description" => description: String,
    }
}

go_struct! {
    /// The `management.register` result (`rpcManagementRegistrationResponse`).
    pub struct ManagementRegistrationResponse("pluginhost.rpcManagementRegistrationResponse") {
        "routes" omitempty => routes: Vec<ManagementRoute>,
        "resources" omitempty => resources: Vec<ResourceRoute>,
    }
}

go_struct! {
    pub struct ManagementRequest("pluginapi.ManagementRequest") {
        "Method" => method: String,
        "Path" => path: String,
        "Headers" => headers: Header,
        "Query" => query: Header,
        "Body" => body: Bytes,
    }
}

go_struct! {
    pub struct ManagementResponse("pluginapi.ManagementResponse") {
        "StatusCode" => status_code: i64,
        "Headers" => headers: Header,
        "Body" => body: Bytes,
    }
}

go_struct! {
    pub struct UsageFailure("pluginapi.UsageFailure") {
        "StatusCode" => status_code: i64,
        "Body" => body: String,
    }
}

go_struct! {
    pub struct UsageDetail("pluginapi.UsageDetail") {
        "InputTokens" => input_tokens: i64,
        "OutputTokens" => output_tokens: i64,
        "ReasoningTokens" => reasoning_tokens: i64,
        "CachedTokens" => cached_tokens: i64,
        "CacheReadTokens" => cache_read_tokens: i64,
        "CacheCreationTokens" => cache_creation_tokens: i64,
        "TotalTokens" => total_tokens: i64,
    }
}

go_struct! {
    pub struct UsageRecord("pluginapi.UsageRecord") {
        "RequestID" => request_id: String,
        "TraceID" => trace_id: String,
        "Provider" => provider: String,
        "BaseURL" => base_url: String,
        "ExecutorType" => executor_type: String,
        "Model" => model: String,
        "Alias" => alias: String,
        "APIKey" => api_key: String,
        "SessionID" => session_id: String,
        "ParentSessionID" => parent_session_id: String,
        "AuthID" => auth_id: String,
        "AuthIndex" => auth_index: String,
        "AuthType" => auth_type: String,
        "Source" => source: String,
        "ReasoningEffort" => reasoning_effort: String,
        "ServiceTier" => service_tier: String,
        "ResponseServiceTier" => response_service_tier: String,
        "ResponseModel" => response_model: String,
        "Generate" => generate: bool,
        "Stream" => stream: bool,
        "RequestedAt" => requested_at: GoTime,
        /// `time.Duration` in nanoseconds.
        "Latency" => latency_ns: i64,
        /// `time.Duration` in nanoseconds.
        "TTFT" => ttft_ns: i64,
        "Failed" => failed: bool,
        "Failure" => failure: UsageFailure,
        "Detail" => detail: UsageDetail,
        "ResponseHeaders" => response_headers: Header,
    }
}

go_struct! {
    pub struct QuotaDescribeRequest("pluginapi.QuotaDescribeRequest") {
        "plugin" omitempty => plugin: PluginMetadata,
    }
}

go_struct! {
    pub struct QuotaDescribeResponse("pluginapi.QuotaDescribeResponse") {
        "supported_providers" omitempty => supported_providers: Vec<String>,
        "display_name" omitempty => display_name: String,
        "supports_reset" omitempty => supports_reset: bool,
    }
}

go_struct! {
    pub struct QuotaFetchRequest("pluginapi.QuotaFetchRequest") {
        "auth_index" => auth_index: String,
        "auth_id" => auth_id: String,
        "provider" => provider: String,
        "storage_json" omitempty => storage_json: Bytes,
        "metadata" omitempty => metadata: Metadata,
        "attributes" omitempty => attributes: StringMap,
        "host" omitempty => host: HostConfigSummary,
    }
}

go_struct! {
    pub struct QuotaResetRequest("pluginapi.QuotaResetRequest") {
        "auth_index" => auth_index: String,
        "auth_id" => auth_id: String,
        "provider" => provider: String,
        "storage_json" omitempty => storage_json: Bytes,
        "metadata" omitempty => metadata: Metadata,
        "attributes" omitempty => attributes: StringMap,
        "host" omitempty => host: HostConfigSummary,
    }
}

go_struct! {
    pub struct QuotaResetResponse("pluginapi.QuotaResetResponse") {
        "success" => success: bool,
        "message" omitempty => message: String,
    }
}

go_struct! {
    pub struct QuotaMetric("pluginapi.QuotaMetric") {
        "key" => key: String,
        "label" => label: String,
        "value" => value: f64,
        "unit" omitempty => unit: String,
        "format" omitempty => format: String,
        "currency" omitempty => currency: String,
    }
}

// Quota responses carry custom `UnmarshalJSON` methods that also accept snake_case keys.
// Go keeps the camelCase value unless it is the zero value and the snake_case one is not.

fn alt<T: PartialEq + Default>(camel: T, snake: Option<T>) -> T {
    match snake {
        Some(snake) if camel == T::default() && snake != T::default() => snake,
        _ => camel,
    }
}

go_struct! {
    pub struct QuotaSubscription("pluginapi.QuotaSubscription") {
        "plan" omitempty => plan: String,
        "tierName" omitempty => tier_name: String,
        "tierId" omitempty => tier_id: String,
    }
}

go_struct! {
    pub struct QuotaSubscriptionAlt("pluginapi.QuotaSubscription") {
        "tier_name" => tier_name: Option<String>,
        "tier_id" => tier_id: Option<String>,
    }
}

go_struct! {
    pub struct QuotaBucket("pluginapi.QuotaBucket") {
        "window" omitempty => window: String,
        "remainingFraction" => remaining_fraction: f64,
        "resetTime" omitempty => reset_time: String,
        "description" omitempty => description: String,
    }
}

go_struct! {
    pub struct QuotaBucketAlt("pluginapi.QuotaBucket") {
        "remainingFraction" => remaining_fraction: Option<f64>,
        "remaining_fraction" => alt_remaining_fraction: Option<f64>,
        "reset_time" => reset_time: Option<String>,
    }
}

go_struct! {
    pub struct QuotaGroup("pluginapi.QuotaGroup") {
        "displayName" omitempty => display_name: String,
        "buckets" omitempty => buckets: Vec<QuotaBucket>,
    }
}

go_struct! {
    pub struct QuotaGroupAlt("pluginapi.QuotaGroup") {
        "display_name" => display_name: Option<String>,
    }
}

go_struct! {
    pub struct QuotaFetchResponse("pluginapi.QuotaFetchResponse") {
        "subscription" omitempty => subscription: Option<QuotaSubscription>,
        "summary" omitempty => summary: Vec<QuotaMetric>,
        "serverTimeOffsetMs" omitempty => server_time_offset_ms: i64,
        "groups" omitempty => groups: Vec<QuotaGroup>,
    }
}

go_struct! {
    pub struct QuotaFetchResponseAlt("pluginapi.QuotaFetchResponse") {
        "server_time_offset_ms" => server_time_offset_ms: Option<i64>,
    }
}

/// Applies the snake_case fallbacks of the quota `UnmarshalJSON` methods to a decoded
/// `quota.fetch` result. `raw` is the result object the plugin returned.
pub fn quota_fetch_from_value(raw: &Node) -> Result<QuotaFetchResponse, crate::gojson::DecodeError> {
    let mut out = QuotaFetchResponse::decode(raw)?;
    let top: QuotaFetchResponseAlt = crate::gojson::decode_struct(raw)?;
    out.server_time_offset_ms = alt(out.server_time_offset_ms, top.server_time_offset_ms);
    if let (Some(sub), Some(raw_sub)) = (out.subscription.as_mut(), raw.member("subscription")) {
        let a: QuotaSubscriptionAlt = crate::gojson::decode_struct(raw_sub)?;
        sub.tier_name = alt(std::mem::take(&mut sub.tier_name), a.tier_name);
        sub.tier_id = alt(std::mem::take(&mut sub.tier_id), a.tier_id);
    }
    if let Some(Node::Array(groups)) = raw.member("groups") {
        for (group, raw_group) in out.groups.iter_mut().zip(groups.iter()) {
            let a: QuotaGroupAlt = crate::gojson::decode_struct(raw_group)?;
            group.display_name = alt(std::mem::take(&mut group.display_name), a.display_name);
            if let Some(Node::Array(buckets)) = raw_group.member("buckets") {
                for (bucket, raw_bucket) in group.buckets.iter_mut().zip(buckets.iter()) {
                    let a: QuotaBucketAlt = crate::gojson::decode_struct(raw_bucket)?;
                    // A present camelCase pointer wins even when zero.
                    bucket.remaining_fraction = a.remaining_fraction.or(a.alt_remaining_fraction).unwrap_or_default();
                    bucket.reset_time = alt(std::mem::take(&mut bucket.reset_time), a.reset_time);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gojson::{from_slice, to_vec};

    #[test]
    fn scheduler_pick_accepts_both_spellings() {
        let r: SchedulerPickResponse =
            from_slice(br#"{"auth_id":"snake","AuthID":"go","handled":true,"reject_code":"x"}"#).unwrap();
        assert_eq!(r.auth_id, "go");
        assert!(r.handled);
        assert_eq!(r.reject_code, "x");
    }

    #[test]
    fn quota_fetch_accepts_snake_case() {
        let raw = crate::gojson::parse(
            br#"{"subscription": {"tier_name": "Pro", "tierId": "t1"}, "server_time_offset_ms": 5,
            "groups": [{"display_name": "G", "buckets": [{"remaining_fraction": 0.25, "reset_time": "soon"},
            {"remainingFraction": 0, "remaining_fraction": 0.5}]}]}"#,
        )
        .unwrap();
        let r = quota_fetch_from_value(&raw).unwrap();
        assert_eq!(r.server_time_offset_ms, 5);
        let sub = r.subscription.unwrap();
        assert_eq!((sub.tier_name.as_str(), sub.tier_id.as_str()), ("Pro", "t1"));
        assert_eq!(r.groups[0].display_name, "G");
        assert_eq!(r.groups[0].buckets[0].remaining_fraction, 0.25);
        assert_eq!(r.groups[0].buckets[0].reset_time, "soon");
        assert_eq!(
            r.groups[0].buckets[1].remaining_fraction, 0.0,
            "a present camelCase pointer wins"
        );
    }

    #[test]
    fn model_info_round_trips_go_field_names() {
        let m = ModelInfo {
            id: "m".into(),
            thinking: Some(ThinkingSupport {
                levels: vec!["low".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let raw = to_vec(&m);
        assert!(raw.starts_with(br#"{"ID":"m","Object":"","Created":0,"#));
        assert_eq!(from_slice::<ModelInfo>(&raw).unwrap(), m);
    }
}
