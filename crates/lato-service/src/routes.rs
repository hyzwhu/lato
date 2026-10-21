use std::{convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, Request, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use futures_util::stream;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::sync::broadcast;

use crate::{
    error::ApiError,
    state::{
        MAX_ACTIVE_OPERATIONS, MAX_OPERATIONS, MAX_SESSIONS, OperationRequest, ServiceState,
        SessionSlot,
    },
};

pub(crate) fn router(state: Arc<ServiceState>) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/info", get(info))
        .route("/v1/sessions", get(list_sessions).post(create_session))
        .route("/v1/sessions/resume", post(resume_session))
        .route("/v1/sessions/{session_id}", get(session_info))
        .route(
            "/v1/sessions/{session_id}/operations",
            post(submit_operation),
        )
        .route("/v1/sessions/{session_id}/cancel", post(cancel_operation))
        .route("/v1/sessions/{session_id}/close", post(close_session))
        .route("/v1/sessions/{session_id}/skills", get(skills))
        .route("/v1/sessions/{session_id}/workflows", get(workflows))
        .route(
            "/v1/sessions/{session_id}/workflow-runs",
            get(workflow_runs),
        )
        .route(
            "/v1/sessions/{session_id}/workflow-runs/pause",
            post(pause_workflow),
        )
        .route(
            "/v1/sessions/{session_id}/workflow-runs/resume",
            post(resume_workflow),
        )
        .route(
            "/v1/sessions/{session_id}/workflow-runs/stop",
            post(stop_workflow),
        )
        .route("/v1/sessions/{session_id}/events", get(events))
        .route("/v1/sessions/{session_id}/approvals", get(approvals))
        .route(
            "/v1/sessions/{session_id}/approvals/{approval_id}",
            post(resolve_approval),
        )
        .route("/v1/operations/{operation_id}", get(operation))
        .fallback(|| async { ApiError::missing() })
        .method_not_allowed_fallback(|| async {
            ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "service.method_not_allowed",
                "method not allowed",
            )
        })
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(
    State(state): State<Arc<ServiceState>>,
    request: Request,
    next: Next,
) -> Response {
    let valid = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|token| bool::from(token.as_bytes().ct_eq(state.token.as_bytes())));
    if !valid {
        let mut response = ApiError::new(
            StatusCode::UNAUTHORIZED,
            "service.unauthorized",
            "a valid bearer token is required",
        )
        .into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            "Bearer".parse().expect("static header"),
        );
        return response;
    }
    if let Err(error) = state.require_running() {
        return error.into_response();
    }
    let mut response = next.run(request).await;
    if (response.status().is_client_error() || response.status().is_server_error())
        && !response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|value| value.as_bytes().starts_with(b"application/json"))
    {
        response = ApiError::new(
            response.status(),
            "service.invalid_request",
            "invalid route parameters or request",
        )
        .into_response();
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header"),
    );
    response
}

async fn health() -> Json<Value> {
    Json(json!({"apiVersion":"v1","status":"ok"}))
}

async fn info(State(state): State<Arc<ServiceState>>) -> Result<Json<Value>, ApiError> {
    let configuration = state.rpc("lato/session/info", json!({})).await?;
    Ok(Json(
        json!({"apiVersion":"v1","agent":configuration["agent"],"limits":{
        "openSessions":MAX_SESSIONS,"activeOperations":MAX_ACTIVE_OPERATIONS,"operationHistory":MAX_OPERATIONS,"requestBytes":64*1024}}),
    ))
}

async fn list_sessions(State(state): State<Arc<ServiceState>>) -> Result<Json<Value>, ApiError> {
    let result = state.rpc("session/list", json!({})).await?;
    Ok(Json(
        json!({"apiVersion":"v1","sessions":result["sessions"]}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

async fn create_session(
    State(state): State<Arc<ServiceState>>,
    body: Result<Json<Empty>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    parse_body(body)?;
    open_session(state, None).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResumeRequest {
    session_id: String,
}

async fn resume_session(
    State(state): State<Arc<ServiceState>>,
    body: Result<Json<ResumeRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body = parse_body(body)?;
    if body.session_id.is_empty() || body.session_id.len() > 256 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "service.invalid_request",
            "invalid session ID",
        ));
    }
    open_session(state, Some(body.session_id)).await
}

async fn open_session(
    state: Arc<ServiceState>,
    resume: Option<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let _lifecycle = state.lifecycle.lock().await;
    state.require_running()?;
    {
        let registry = state.registry.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(id) = &resume
            && registry.sessions.contains_key(id)
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "service.session_open",
                "session is already open",
            ));
        }
        if registry.sessions.len() >= MAX_SESSIONS {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "service.capacity",
                "too many open sessions",
            ));
        }
    }
    let (method, params) = match resume {
        Some(id) => ("session/resume", json!({"sessionId":id})),
        None => ("session/new", json!({})),
    };
    let result = state.rpc(method, params).await?;
    let id = result["sessionId"]
        .as_str()
        .ok_or_else(|| ApiError::runtime("agent returned no session ID"))?
        .to_owned();
    let runtime = state
        .host
        .lock()
        .await
        .runtime_session(&id)
        .ok_or_else(ApiError::missing)?;
    if state.require_running().is_err() {
        let _ = state.rpc("session/close", json!({"sessionId":id})).await;
        return Err(ApiError::unavailable());
    }
    state
        .registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .sessions
        .insert(
            id.clone(),
            SessionSlot {
                runtime,
                operation: None,
                closing: false,
            },
        );
    state
        .events
        .push(&id, "session_opened", json!({"sessionId":id}));
    Ok((
        StatusCode::CREATED,
        Json(json!({"apiVersion":"v1","sessionId":id})),
    ))
}

async fn session_info(
    State(state): State<Arc<ServiceState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let registry = state.registry.lock().unwrap_or_else(|p| p.into_inner());
    let slot = registry.sessions.get(&id).ok_or_else(ApiError::missing)?;
    Ok(Json(
        json!({"apiVersion":"v1","sessionId":id,"activeOperationId":slot.operation,"closing":slot.closing}),
    ))
}

async fn submit_operation(
    State(state): State<Arc<ServiceState>>,
    Path(id): Path<String>,
    body: Result<Json<OperationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<crate::state::OperationView>), ApiError> {
    Ok((
        StatusCode::ACCEPTED,
        Json(state.submit(&id, parse_body(body)?)?),
    ))
}

async fn operation(
    State(state): State<Arc<ServiceState>>,
    Path(id): Path<String>,
) -> Result<Json<crate::state::OperationView>, ApiError> {
    Ok(Json(state.operation(&id)?))
}

async fn cancel_operation(
    State(state): State<Arc<ServiceState>>,
    Path(id): Path<String>,
    body: Result<Json<Empty>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    parse_body(body)?;
    Ok(Json(
        json!({"apiVersion":"v1","operation":state.cancel(&id)?}),
    ))
}

async fn close_session(
    State(state): State<Arc<ServiceState>>,
    Path(id): Path<String>,
    body: Result<Json<Empty>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    parse_body(body)?;
    let _lifecycle = state.lifecycle.lock().await;
    let runtime = {
        let mut registry = state.registry.lock().unwrap_or_else(|p| p.into_inner());
        let slot = registry
            .sessions
            .get_mut(&id)
            .ok_or_else(ApiError::missing)?;
        slot.closing = true;
        slot.runtime.clone()
    };
    state.cancel(&id)?;
    runtime
        .cancel_and_join(Duration::from_secs(5))
        .await
        .map_err(ApiError::runtime)?;
    state.rpc("session/close", json!({"sessionId":id})).await?;
    state
        .registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .sessions
        .remove(&id);
    state
        .events
        .push(&id, "session_closed", json!({"sessionId":id}));
    Ok(Json(json!({"apiVersion":"v1","closed":true})))
}

async fn skills(
    State(state): State<Arc<ServiceState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let skills = state.session(&id)?.list_skills().await;
    Ok(Json(
        json!({"apiVersion":"v1","skills":skills["skills"],"generation":skills["generation"]}),
    ))
}

async fn workflows(
    State(state): State<Arc<ServiceState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state.session(&id)?;
    let result = state
        .rpc("lato/session/workflows", json!({"sessionId":id}))
        .await?;
    Ok(Json(
        json!({"apiVersion":"v1","workflows":result["workflows"],"generation":result["generation"]}),
    ))
}

async fn workflow_runs(
    State(state): State<Arc<ServiceState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let runs = state.session(&id)?.workflow_runs().await;
    Ok(Json(json!({"apiVersion":"v1","runs":runs})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowTarget {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkflowResume {
    name: String,
    agent_budget: Option<u64>,
}

fn validate_workflow_name(name: &str) -> Result<(), ApiError> {
    if name.is_empty() || name.len() > 256 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "service.invalid_request",
            "invalid workflow display name",
        ));
    }
    Ok(())
}

async fn pause_workflow(
    State(state): State<Arc<ServiceState>>,
    Path(id): Path<String>,
    body: Result<Json<WorkflowTarget>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let body = parse_body(body)?;
    validate_workflow_name(&body.name)?;
    let run = state
        .session(&id)?
        .workflow_pause(&body.name)
        .await
        .map_err(ApiError::runtime)?;
    Ok(Json(json!({"apiVersion":"v1","run":run})))
}

async fn resume_workflow(
    State(state): State<Arc<ServiceState>>,
    Path(id): Path<String>,
    body: Result<Json<WorkflowResume>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let body = parse_body(body)?;
    validate_workflow_name(&body.name)?;
    if body.agent_budget.is_some_and(|b| b == 0 || b > 1024) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "service.invalid_request",
            "agentBudget must be between 1 and 1024",
        ));
    }
    let run = state
        .session(&id)?
        .workflow_resume(&body.name, body.agent_budget)
        .await
        .map_err(ApiError::runtime)?;
    Ok(Json(json!({"apiVersion":"v1","run":run})))
}

async fn stop_workflow(
    State(state): State<Arc<ServiceState>>,
    Path(id): Path<String>,
    body: Result<Json<WorkflowTarget>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let body = parse_body(body)?;
    validate_workflow_name(&body.name)?;
    let run = state
        .session(&id)?
        .workflow_stop(&body.name)
        .await
        .map_err(ApiError::runtime)?;
    Ok(Json(json!({"apiVersion":"v1","run":run})))
}

async fn approvals(
    State(state): State<Arc<ServiceState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state.session(&id)?;
    Ok(Json(
        json!({"apiVersion":"v1","approvals":state.approvals.list(&id)}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalDecision {
    allow: bool,
}

async fn resolve_approval(
    State(state): State<Arc<ServiceState>>,
    Path((session, id)): Path<(String, String)>,
    body: Result<Json<ApprovalDecision>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let decision = parse_body(body)?;
    state.session(&session)?;
    if !state.approvals.resolve(&session, &id, decision.allow) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "service.approval_closed",
            "approval is absent, expired, belongs to another session, or was already answered",
        ));
    }
    Ok(Json(json!({"apiVersion":"v1","accepted":true})))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    after: Option<u64>,
}

async fn events(
    State(state): State<Arc<ServiceState>>,
    Path(id): Path<String>,
    query: Result<Query<Cursor>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.session(&id)?;
    let query = query
        .map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "service.invalid_cursor",
                "invalid event cursor",
            )
        })?
        .0;
    let header = headers
        .get("last-event-id")
        .map(|h| {
            h.to_str()
                .ok()
                .and_then(|h| h.parse::<u64>().ok())
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "service.invalid_cursor",
                        "invalid Last-Event-ID header",
                    )
                })
        })
        .transpose()?;
    if query.after.is_some() && header.is_some() && query.after != header {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "service.invalid_cursor",
            "conflicting event cursors",
        ));
    }
    let (replay, receiver) = state.events.subscribe(query.after.or(header))?;
    let stop = state.stop.clone();
    let stream = stream::unfold(
        (replay, receiver, id, stop, false),
        |(mut replay, mut receiver, id, stop, finished)| async move {
            if finished {
                return None;
            }
            loop {
                if stop.is_cancelled() {
                    return None;
                }
                let event = if let Some(event) = replay.pop_front() {
                    event
                } else {
                    tokio::select! {
                        _ = stop.cancelled() => return None,
                        event = receiver.recv() => match event {
                            Ok(event) => event,
                            Err(broadcast::error::RecvError::Closed) => return None,
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                let event = Event::default().event("gap").data("{\"code\":\"service.event_gap\",\"message\":\"subscriber fell behind; inspect operation status and reconnect\"}");
                                return Some((Ok::<_,Infallible>(event),(replay,receiver,id,stop,true)));
                            }
                        }
                    }
                };
                if event.session_id != id {
                    continue;
                }
                let frame = Event::default()
                    .event("update")
                    .id(event.sequence.to_string())
                    .json_data(&event)
                    .expect("service events are serializable");
                return Some((
                    Ok::<_, Infallible>(frame),
                    (replay, receiver, id, stop, false),
                ));
            }
        },
    );
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}

fn parse_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|body| body.0)
        .map_err(|rejection| ApiError::invalid_json(rejection.status()))
}
