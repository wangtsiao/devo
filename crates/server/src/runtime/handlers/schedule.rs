//! Native `session/schedule/*` handlers and background wake loop.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;

use crate::ProtocolErrorCode;
use crate::SuccessResponse;
use crate::runtime::ServerRuntime;

impl ServerRuntime {
    pub(crate) async fn handle_native_session_schedule_list(
        &self,
        request_id: serde_json::Value,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let params: devo_protocol::native::rpc_schedule::SessionScheduleListParams =
            match serde_json::from_value(params) {
                Ok(params) => params,
                Err(error) => {
                    return self.error_response(
                        request_id,
                        ProtocolErrorCode::InvalidParams,
                        format!("invalid session/schedule/list params: {error}"),
                    );
                }
            };
        match self
            .schedule_store
            .list(params.session_id.as_ref(), params.cwd.as_deref())
        {
            Ok(jobs) => serde_json::to_value(SuccessResponse {
                id: request_id,
                result: devo_protocol::native::rpc_schedule::SessionScheduleListResult { jobs },
            })
            .expect("serialize session/schedule/list"),
            Err(error) => self.error_response(
                request_id,
                ProtocolErrorCode::InternalError,
                format!("failed to list schedules: {error}"),
            ),
        }
    }

    pub(crate) async fn handle_native_session_schedule_upsert(
        self: &Arc<Self>,
        request_id: serde_json::Value,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let params: devo_protocol::native::rpc_schedule::SessionScheduleUpsertParams =
            match serde_json::from_value(params) {
                Ok(params) => params,
                Err(error) => {
                    return self.error_response(
                        request_id,
                        ProtocolErrorCode::InvalidParams,
                        format!("invalid session/schedule/upsert params: {error}"),
                    );
                }
            };
        let job = match self.schedule_store.upsert(params) {
            Ok(job) => job,
            Err(error) => {
                let message = error.to_string();
                if message.starts_with("job not found:") {
                    return self.error_response(
                        request_id,
                        ProtocolErrorCode::InvalidParams,
                        message,
                    );
                }
                return self.error_response(
                    request_id,
                    ProtocolErrorCode::InternalError,
                    format!("failed to upsert schedule: {error}"),
                );
            }
        };
        self.broadcast_notification(
            devo_protocol::native::event::ServerNotification::SessionScheduleChanged {
                session_id: Some(job.session_id),
                job: Box::new(job.clone()),
                change: devo_protocol::native::event::SessionScheduleChange::Upserted,
            },
        )
        .await;
        serde_json::to_value(SuccessResponse {
            id: request_id,
            result: devo_protocol::native::rpc_schedule::SessionScheduleUpsertResult { job },
        })
        .expect("serialize session/schedule/upsert")
    }

    pub(crate) async fn handle_native_session_schedule_update(
        self: &Arc<Self>,
        request_id: serde_json::Value,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let params: devo_protocol::native::rpc_schedule::SessionScheduleUpdateParams =
            match serde_json::from_value(params) {
                Ok(params) => params,
                Err(error) => {
                    return self.error_response(
                        request_id,
                        ProtocolErrorCode::InvalidParams,
                        format!("invalid session/schedule/update params: {error}"),
                    );
                }
            };
        let job = match self.schedule_store.update(&params.job_id, params.action) {
            Ok(job) => job,
            Err(error) => {
                let message = error.to_string();
                if message.starts_with("job not found:") {
                    return self.error_response(
                        request_id,
                        ProtocolErrorCode::InvalidParams,
                        message,
                    );
                }
                return self.error_response(
                    request_id,
                    ProtocolErrorCode::InternalError,
                    format!("failed to update schedule: {error}"),
                );
            }
        };
        self.broadcast_notification(
            devo_protocol::native::event::ServerNotification::SessionScheduleChanged {
                session_id: Some(job.session_id),
                job: Box::new(job.clone()),
                change: devo_protocol::native::event::SessionScheduleChange::Updated,
            },
        )
        .await;
        serde_json::to_value(SuccessResponse {
            id: request_id,
            result: devo_protocol::native::rpc_schedule::SessionScheduleUpdateResult { job },
        })
        .expect("serialize session/schedule/update")
    }

    pub(crate) async fn handle_native_session_schedule_delete(
        self: &Arc<Self>,
        request_id: serde_json::Value,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let params: devo_protocol::native::rpc_schedule::SessionScheduleDeleteParams =
            match serde_json::from_value(params) {
                Ok(params) => params,
                Err(error) => {
                    return self.error_response(
                        request_id,
                        ProtocolErrorCode::InvalidParams,
                        format!("invalid session/schedule/delete params: {error}"),
                    );
                }
            };
        let job = match self.schedule_store.delete(&params.job_id) {
            Ok(job) => job,
            Err(error) => {
                let message = error.to_string();
                if message.starts_with("job not found:") {
                    return self.error_response(
                        request_id,
                        ProtocolErrorCode::InvalidParams,
                        message,
                    );
                }
                return self.error_response(
                    request_id,
                    ProtocolErrorCode::InternalError,
                    format!("failed to delete schedule: {error}"),
                );
            }
        };
        self.broadcast_notification(
            devo_protocol::native::event::ServerNotification::SessionScheduleChanged {
                session_id: Some(job.session_id),
                job: Box::new(job),
                change: devo_protocol::native::event::SessionScheduleChange::Deleted,
            },
        )
        .await;
        serde_json::to_value(SuccessResponse {
            id: request_id,
            result: devo_protocol::native::rpc_schedule::SessionScheduleDeleteResult {},
        })
        .expect("serialize session/schedule/delete")
    }

    pub(crate) async fn handle_native_session_heartbeat_command(
        self: &Arc<Self>,
        request_id: serde_json::Value,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let params: devo_protocol::native::rpc_schedule::SessionHeartbeatCommandParams =
            match serde_json::from_value(params) {
                Ok(params) => params,
                Err(error) => {
                    return self.error_response(
                        request_id,
                        ProtocolErrorCode::InvalidParams,
                        format!("invalid session/heartbeat/command params: {error}"),
                    );
                }
            };
        let result = match crate::heartbeat_command::run_heartbeat_command(
            &self.schedule_store,
            params,
        ) {
            Ok(result) => result,
            Err(error) => {
                return self.error_response(
                    request_id,
                    ProtocolErrorCode::InvalidParams,
                    error.to_string(),
                );
            }
        };
        if let Some(job) = result.job.as_ref() {
            let change = match result.action {
                devo_protocol::native::rpc_schedule::HeartbeatCommandAction::Clear => {
                    devo_protocol::native::event::SessionScheduleChange::Deleted
                }
                devo_protocol::native::rpc_schedule::HeartbeatCommandAction::Set => {
                    devo_protocol::native::event::SessionScheduleChange::Upserted
                }
                devo_protocol::native::rpc_schedule::HeartbeatCommandAction::Pause
                | devo_protocol::native::rpc_schedule::HeartbeatCommandAction::Resume => {
                    devo_protocol::native::event::SessionScheduleChange::Updated
                }
                devo_protocol::native::rpc_schedule::HeartbeatCommandAction::Status => {
                    // status is read-only; no broadcast
                    return serde_json::to_value(SuccessResponse {
                        id: request_id,
                        result,
                    })
                    .expect("serialize session/heartbeat/command");
                }
            };
            self.broadcast_notification(
                devo_protocol::native::event::ServerNotification::SessionScheduleChanged {
                    session_id: Some(job.session_id),
                    job: Box::new(job.clone()),
                    change,
                },
            )
            .await;
        }
        serde_json::to_value(SuccessResponse {
            id: request_id,
            result,
        })
        .expect("serialize session/heartbeat/command")
    }

    pub(crate) fn start_schedule_wake_loop(self: &Arc<Self>) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(15));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(error) = runtime.dispatch_due_schedule_jobs().await {
                    tracing::warn!(error = %error, "schedule wake loop failed");
                }
            }
        });
    }

    async fn dispatch_due_schedule_jobs(self: &Arc<Self>) -> anyhow::Result<()> {
        let due = self.schedule_store.claim_due(Utc::now())?;
        for job in due {
            let prompt = job
                .prompt
                .clone()
                .or_else(|| job.instruction.clone())
                .unwrap_or_else(|| "Scheduled heartbeat.".to_string());
            let input = vec![devo_protocol::native::item::UserInput::Text { text: prompt }];
            let idempotency_key = format!("schedule:{}:{}", job.job_id.as_str(), Uuid::now_v7());
            let session_id = job.session_id;
            match job.delivery_mode {
                devo_protocol::native::rpc_schedule::ScheduleDeliveryMode::FollowUp
                | devo_protocol::native::rpc_schedule::ScheduleDeliveryMode::Steer => {
                    // Follow-up always queues (or starts when idle). Steer uses
                    // the same admission path for Phase F; a dedicated mid-turn
                    // steer inject can land once wake has more session context.
                    let params = serde_json::json!({
                        "sessionId": session_id,
                        "input": input,
                        "idempotencyKey": idempotency_key,
                    });
                    let _ = self
                        .handle_session_queue_push(
                            /*connection_id*/ 0,
                            serde_json::json!(null),
                            params,
                        )
                        .await;
                }
            }
        }
        Ok(())
    }
}
