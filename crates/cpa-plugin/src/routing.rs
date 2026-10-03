//! Scheduler and model router capabilities (internal/pluginhost/scheduler.go,
//! model_router.go, executor_route.go).

use crate::abi::method;
use crate::api::{
    ModelRouteRequest, ModelRouteResponse, ROUTE_TARGET_EXECUTOR, ROUTE_TARGET_PROVIDER, ROUTE_TARGET_SELF,
    SCHEDULER_BUILTIN_FILL_FIRST, SCHEDULER_BUILTIN_ROUND_ROBIN, SchedulerPickRequest, SchedulerPickResponse,
};
use crate::callbacks::RequestScope;
use crate::host::{Host, Record};
use crate::rpc::CallError;

impl Host {
    /// Go `schedulerRecord`: the highest-priority active scheduler.
    fn scheduler_record(&self) -> Option<Record> {
        self.active_records()
            .into_iter()
            .find(|r| r.plugin.caps.scheduler && !self.is_fused(&r.id))
    }

    /// Go `HasScheduler`.
    pub fn has_scheduler(&self) -> bool {
        self.scheduler_record().is_some()
    }

    /// Go `SchedulerWantsAcrossPriorities`.
    pub fn scheduler_wants_across_priorities(&self) -> bool {
        self.scheduler_record()
            .is_some_and(|r| r.plugin.caps.scheduler_across_priorities)
    }

    /// Go `PickAuth`: `Ok(None)` lets the built-in scheduler choose (no scheduler, not
    /// handled, or an invalid answer); `Err` is the plugin's RPC failure, which Go
    /// surfaces as a selection error.
    pub async fn pick_auth(&self, mut req: SchedulerPickRequest) -> Result<Option<SchedulerPickResponse>, CallError> {
        let Some(record) = self.scheduler_record() else {
            return Ok(None);
        };
        if !self.record_current(&record) {
            return Ok(None);
        }
        req.plugin = record.plugin.metadata.clone();
        let resp: SchedulerPickResponse = match self.call(&record, method::SCHEDULER_PICK, &req).await {
            Ok(resp) => resp,
            Err(CallError::Panic(_)) => return Ok(None),
            Err(e) => {
                tracing::warn!(plugin_id = %record.id, "pluginhost: scheduler rejected auth pick: {e}");
                return Err(e);
            }
        };
        if !resp.handled {
            return Ok(None);
        }
        match normalize_scheduler_response(resp, &req) {
            Ok(resp) => Ok(Some(resp)),
            Err(reason) => {
                tracing::warn!(plugin_id = %record.id, "pluginhost: scheduler returned invalid response: {reason}");
                Ok(None)
            }
        }
    }

    /// Go `HasModelRouters[Except]`.
    pub fn has_model_routers(&self, skip: &str) -> bool {
        let skip = skip.trim();
        self.active_records()
            .iter()
            .any(|r| r.plugin.caps.model_router && !self.is_fused(&r.id) && r.id != skip)
    }

    /// Go `RouteModel[Except]`: the first router (by priority) that handles the request
    /// with a usable target. `available_providers` is the providers that currently have
    /// credentials (Go `AuthManager.AvailableProviders`); a provider target must be one
    /// of them. `executor_ready` reports whether a plugin executor can take the request
    /// (format negotiation, see [`crate::executor`]).
    pub async fn route_model(
        &self,
        mut req: ModelRouteRequest,
        skip: &str,
        available_providers: &[String],
        scope: &RequestScope,
    ) -> Option<ModelRouteResponse> {
        let skip = skip.trim();
        req.available_providers = available_providers.to_vec();
        for record in self.active_records() {
            if !record.plugin.caps.model_router || self.is_fused(&record.id) || record.id == skip {
                continue;
            }
            let mut next = req.clone();
            next.plugin = record.plugin.metadata.clone();
            next.plugin_id = record.id.clone();
            let resp: ModelRouteResponse = match self
                .call_with_callback(&record, method::MODEL_ROUTE, &next, scope)
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(plugin_id = %record.id, "pluginhost: model router failed: {e}");
                    continue;
                }
            };
            if !resp.handled {
                continue;
            }
            let Some(resp) = normalize_route_response(&record.id, resp.clone()) else {
                tracing::warn!(plugin_id = %record.id, target_kind = %resp.target_kind, target = %resp.target, "pluginhost: model router returned invalid target");
                continue;
            };
            match resp.target_kind.as_str() {
                ROUTE_TARGET_PROVIDER => {
                    if !available_providers.iter().any(|p| p == &resp.target) {
                        tracing::warn!(plugin_id = %record.id, target_provider = %resp.target, "pluginhost: model router returned unavailable provider");
                        continue;
                    }
                    return Some(resp);
                }
                _ => {
                    if !self.executor_plugin_ready(&resp.target, &next.source_format).await {
                        tracing::warn!(plugin_id = %record.id, target_plugin_id = %resp.target, "pluginhost: model router returned unavailable executor plugin");
                        continue;
                    }
                    return Some(resp);
                }
            }
        }
        None
    }
}

/// Go `normalizeSchedulerResponse`.
pub fn normalize_scheduler_response(
    mut resp: SchedulerPickResponse,
    req: &SchedulerPickRequest,
) -> Result<SchedulerPickResponse, &'static str> {
    resp.auth_id = resp.auth_id.trim().to_owned();
    resp.delegate_builtin = resp.delegate_builtin.trim().to_owned();
    resp.reject_code = resp.reject_code.trim().to_owned();
    resp.reject_reason = resp.reject_reason.trim().to_owned();
    if resp.reject {
        if resp.reject_code.is_empty() {
            resp.reject_code = "auth_unavailable".into();
        }
        if resp.reject_reason.is_empty() {
            resp.reject_reason = "scheduler rejected candidate selection".into();
        }
        return Ok(resp);
    }
    match (resp.auth_id.is_empty(), resp.delegate_builtin.is_empty()) {
        (true, true) => Err("missing auth id or delegate"),
        (false, _) => {
            if req.candidates.iter().any(|c| c.id.trim() == resp.auth_id) {
                Ok(resp)
            } else {
                Err("unknown auth id")
            }
        }
        (true, false) => {
            if matches!(
                resp.delegate_builtin.as_str(),
                SCHEDULER_BUILTIN_ROUND_ROBIN | SCHEDULER_BUILTIN_FILL_FIRST
            ) {
                Ok(resp)
            } else {
                Err("unknown delegate")
            }
        }
    }
}

/// Go `normalizeModelRouteResponse`.
pub fn normalize_route_response(router_id: &str, mut resp: ModelRouteResponse) -> Option<ModelRouteResponse> {
    resp.target_model = resp.target_model.trim().to_owned();
    resp.target = match resp.target_kind.as_str() {
        ROUTE_TARGET_SELF => router_id.trim().to_owned(),
        ROUTE_TARGET_EXECUTOR => resp.target.trim().to_owned(),
        ROUTE_TARGET_PROVIDER => resp.target.trim().to_lowercase(),
        _ => return None,
    };
    (!resp.target.is_empty()).then_some(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SchedulerAuthCandidate;

    #[test]
    fn scheduler_answers_are_validated_like_go() {
        let req = SchedulerPickRequest {
            candidates: vec![SchedulerAuthCandidate {
                id: " a ".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let pick = |r: SchedulerPickResponse| normalize_scheduler_response(r, &req);
        let base = SchedulerPickResponse {
            handled: true,
            ..Default::default()
        };
        assert_eq!(
            pick(SchedulerPickResponse {
                auth_id: " a".into(),
                ..base.clone()
            })
            .unwrap()
            .auth_id,
            "a"
        );
        assert_eq!(
            pick(SchedulerPickResponse {
                auth_id: "b".into(),
                ..base.clone()
            }),
            Err("unknown auth id")
        );
        assert_eq!(
            pick(SchedulerPickResponse {
                delegate_builtin: "random".into(),
                ..base.clone()
            }),
            Err("unknown delegate")
        );
        let rejected = pick(SchedulerPickResponse {
            reject: true,
            ..base.clone()
        })
        .unwrap();
        assert_eq!(rejected.reject_code, "auth_unavailable");
        assert_eq!(rejected.reject_reason, "scheduler rejected candidate selection");
        assert_eq!(pick(base), Err("missing auth id or delegate"));
    }

    #[test]
    fn route_targets_normalize_like_go() {
        let resp = |kind: &str, target: &str| ModelRouteResponse {
            handled: true,
            target_kind: kind.into(),
            target: target.into(),
            target_model: " m ".into(),
            ..Default::default()
        };
        let own = normalize_route_response("router", resp("self", "ignored")).unwrap();
        assert_eq!((own.target.as_str(), own.target_model.as_str()), ("router", "m"));
        assert_eq!(
            normalize_route_response("r", resp("provider", " Claude "))
                .unwrap()
                .target,
            "claude"
        );
        assert!(normalize_route_response("r", resp("executor", " ")).is_none());
        assert!(normalize_route_response("r", resp("other", "x")).is_none());
    }
}
