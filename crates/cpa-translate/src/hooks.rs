//! Plugin translator hooks (sdk/translator plugin_hooks.go and the hook branches of
//! registry.go). The registry entry points call them: [`crate::translate_request`],
//! [`crate::translate_request_envelope`], and every registered pair's `non_stream` and
//! `stream` (so executors that hold a [`crate::Pair`] get them too). Executors that pass
//! a response through without a registered pair use [`unregistered_non_stream`] and
//! [`wrap_stream`], as Go's `TranslateNonStream` and `TranslateStream` run the hooks for
//! any pair.
//!
//! With no hooks installed, or none active, every entry point takes its old path after
//! one atomic load: no lock, copy or parse. The plugin host keeps that flag current
//! ([`set_plugin_hooks_active`]) as plugins load, unload, reload and fuse.
//!
//! A stream reads the flag once, when its translator is built (Go reads the hooks on
//! every `TranslateStream` call), so a translator plugin loaded mid-stream applies from
//! the next stream.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use cpa_common::json as gj;

use crate::stream::GoStream;
use crate::{Error, ResponseCtx};

/// One response-side hook call's inputs. `from` is the upstream format and `to` the
/// client's, as Go's registry passes them.
pub struct Response<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub model: &'a str,
    pub original_request: &'a [u8],
    pub translated_request: &'a [u8],
    pub stream: bool,
}

/// Go `translator.PluginHooks`. Each method returns `None` for "unchanged" (Go returns
/// a copy of its input), so a hook no plugin offers costs no copy.
pub trait Hooks: Send + Sync {
    /// Go `NormalizeRequest`: every request normalizer, chained.
    fn normalize_request(&self, from: &str, to: &str, model: &str, body: &[u8], stream: bool) -> Option<Vec<u8>>;
    /// Go `TranslateRequest`: the first request translator that answers.
    fn translate_request(&self, from: &str, to: &str, model: &str, body: &[u8], stream: bool) -> Option<Vec<u8>>;
    /// Go `NormalizeResponseBefore`: chained, before translation.
    fn normalize_response_before(&self, t: &Response<'_>, body: &[u8]) -> Option<Vec<u8>>;
    /// Go `TranslateResponse`: the first response translator that answers.
    fn translate_response(&self, t: &Response<'_>, body: &[u8]) -> Option<Vec<u8>>;
    /// Go `NormalizeResponseAfter`: chained, after translation.
    fn normalize_response_after(&self, t: &Response<'_>, body: &[u8]) -> Option<Vec<u8>>;
}

static INSTALLED: AtomicBool = AtomicBool::new(false);
/// Whether a plugin offers any hook. While false (Go: every hook returns its input),
/// the entry points skip the hooks without touching `HOOKS`.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOKS: RwLock<Option<Arc<dyn Hooks>>> = RwLock::new(None);

/// Go `translator.SetPluginHooks`.
/// Clearing the slot also clears the activity flag, so the entry points go back to
/// their one-load path.
pub fn set_plugin_hooks(hooks: Option<Arc<dyn Hooks>>) {
    let mut slot = HOOKS.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    INSTALLED.store(hooks.is_some(), Ordering::Release);
    if hooks.is_none() {
        ACTIVE.store(false, Ordering::Release);
    }
    *slot = hooks;
}

/// Go `translator.HasPluginHooks`.
pub fn has_plugin_hooks() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

/// Set by the owner of the installed hooks whenever the set of plugins offering one
/// changes. One plugin host owns the flag: with two hosts installing hooks, whichever
/// published last would win. The server runs one host.
pub fn set_plugin_hooks_active(active: bool) {
    ACTIVE.store(active, Ordering::Release);
}

/// Whether the installed hooks are in use: a plugin offers one.
pub fn plugin_hooks_active() -> bool {
    ACTIVE.load(Ordering::Acquire) && INSTALLED.load(Ordering::Acquire)
}

/// The installed hooks when a plugin offers any.
pub(crate) fn current() -> Option<Arc<dyn Hooks>> {
    if !ACTIVE.load(Ordering::Acquire) {
        return None;
    }
    HOOKS.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
}

/// Go registry `NormalizeRequest`: the plugin request normalizers, or `body` as is.
pub fn normalize_request(from: &str, to: &str, model: &str, body: Vec<u8>, stream: bool) -> Vec<u8> {
    match current() {
        Some(h) => h.normalize_request(from, to, model, &body, stream).unwrap_or(body),
        None => body,
    }
}

/// Go `configurationUpdates`: the raw Responses `configuration_update` input items.
fn configuration_updates(body: &[u8]) -> Vec<Vec<u8>> {
    let input = gj::get(body, "input");
    let mut updates = vec![];
    if input.is_array() {
        input.each(|_, item| {
            if &*item.get("type").bytes() == b"configuration_update" {
                updates.push(item.raw().to_vec());
            }
            true
        });
    }
    updates
}

/// The normalizer step of Go `TranslateRequestEnvelope`: the normalized body, and
/// whether a normalizer changed the Responses configuration updates
/// (`ConfigurationUpdatesChanged`).
pub(crate) fn normalize_tracked(
    hooks: &dyn Hooks,
    from: &str,
    to: &str,
    model: &str,
    body: Vec<u8>,
    stream: bool,
) -> (Vec<u8>, bool) {
    match hooks.normalize_request(from, to, model, &body, stream) {
        Some(out) => {
            let changed = configuration_updates(&body) != configuration_updates(&out);
            (out, changed)
        }
        None => (body, false),
    }
}

fn response<'a>(from: &'a str, to: &'a str, ctx: &'a ResponseCtx<'_>, stream: bool) -> Response<'a> {
    Response {
        from,
        to,
        model: ctx.model,
        original_request: ctx.original_request,
        translated_request: ctx.translated_request,
        stream,
    }
}

/// Go `TranslateNonStream` for a registered pair: `native` between the before and after
/// normalizers. A failed or empty translation skips the after normalizers.
// ponytail: Go skips them only for a nil result, so an empty non-nil one (an
// OpenAI-to-OpenAI passthrough of an empty body) still reaches them there. Native
// translators here return no nil marker; one would be the upgrade (M6-0083).
pub(crate) fn non_stream(
    upstream: &str,
    client: &str,
    ctx: &ResponseCtx<'_>,
    body: &[u8],
    native: impl FnOnce(&[u8]) -> Result<Vec<u8>, Error>,
) -> Result<Vec<u8>, Error> {
    let Some(h) = current() else {
        return native(body);
    };
    let t = response(upstream, client, ctx, false);
    let before = h.normalize_response_before(&t, body);
    let out = native(before.as_deref().unwrap_or(body))?;
    if out.is_empty() {
        return Ok(out);
    }
    Ok(h.normalize_response_after(&t, &out).unwrap_or(out))
}

/// Go `TranslateNonStream` without a registered pair: `None` when no hooks are active
/// (the caller keeps its passthrough); otherwise the before normalizers, a plugin
/// response translator if one answers, and the after normalizers.
pub fn unregistered_non_stream(upstream: &str, client: &str, ctx: &ResponseCtx<'_>, body: &[u8]) -> Option<Vec<u8>> {
    let h = current()?;
    let t = response(upstream, client, ctx, false);
    let body = h.normalize_response_before(&t, body).unwrap_or_else(|| body.to_vec());
    let body = h.translate_response(&t, &body).unwrap_or(body);
    Some(h.normalize_response_after(&t, &body).unwrap_or(body))
}

/// Go `TranslateStream`'s hooks around one stream's Go-shaped translator (`None`: no
/// registered pair). Without active hooks `inner` comes back as is; otherwise each
/// chunk goes through the before normalizers, `inner` (or a plugin response
/// translator, else unchanged), then the after normalizers.
pub fn wrap_stream(
    upstream: &str,
    client: &str,
    ctx: &ResponseCtx<'_>,
    inner: Option<Box<dyn GoStream>>,
) -> Option<Box<dyn GoStream>> {
    let Some(hooks) = current() else {
        return inner;
    };
    Some(Box::new(Hooked {
        inner,
        hooks,
        from: upstream.to_owned(),
        to: client.to_owned(),
        model: ctx.model.to_owned(),
        original_request: ctx.original_request.to_vec(),
        translated_request: ctx.translated_request.to_vec(),
    }))
}

/// [`wrap_stream`] for a registered pair.
pub(crate) fn stream(
    upstream: &str,
    client: &str,
    ctx: &ResponseCtx<'_>,
    inner: Box<dyn GoStream>,
) -> Box<dyn GoStream> {
    wrap_stream(upstream, client, ctx, Some(inner)).expect("some inner stream stays some")
}

struct Hooked {
    inner: Option<Box<dyn GoStream>>,
    hooks: Arc<dyn Hooks>,
    from: String,
    to: String,
    model: String,
    original_request: Vec<u8>,
    translated_request: Vec<u8>,
}

impl GoStream for Hooked {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let t = Response {
            from: &self.from,
            to: &self.to,
            model: &self.model,
            original_request: &self.original_request,
            translated_request: &self.translated_request,
            stream: true,
        };
        let body = self.hooks.normalize_response_before(&t, line);
        let body = body.as_deref().unwrap_or(line);
        let outputs = match &mut self.inner {
            Some(inner) => {
                let outputs = inner.line(body)?;
                // Retained tool failures are never normalized.
                if inner.tool_input_failed() {
                    return Ok(outputs);
                }
                outputs
            }
            None => vec![self.hooks.translate_response(&t, body).unwrap_or_else(|| body.to_vec())],
        };
        Ok(outputs
            .into_iter()
            .map(|out| self.hooks.normalize_response_after(&t, &out).unwrap_or(out))
            .collect())
    }

    fn tool_input_failed(&self) -> bool {
        self.inner.as_ref().is_some_and(|i| i.tool_input_failed())
    }

    fn finalize_tool_input(&mut self) -> Vec<Vec<u8>> {
        self.inner.as_mut().map(|i| i.finalize_tool_input()).unwrap_or_default()
    }

    fn can_finalize_response_stream(&self) -> bool {
        self.inner.as_ref().is_some_and(|i| i.can_finalize_response_stream())
    }
}
