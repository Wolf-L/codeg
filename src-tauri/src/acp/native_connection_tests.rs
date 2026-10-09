//! Native extension regression tests. Included as a child of connection.rs.
//! Only in-memory ACP peers are used; no agent processes or user files.
//! Assertions express the contract, including regressions awaiting host fixes.

use super::*;
use crate::acp::manager::ConnectionManager;
use crate::acp::native_session::{capabilities, NativeOperation};
use crate::acp::types::EventEnvelope;
use crate::web::event_bridge::WebEventBroadcaster;
use serde_json::{json, Value};
use tokio::sync::broadcast;

const SID: &str = "native-session";
const TURN: &str = "queue-turn";
const QUEUE_TURN: &str = "_session/queue/turn";
const QUEUE_CHANGED: &str = "_session/queue/changed";

fn advertised_caps() -> Value {
    let meta = json!({
        "queue": {
            "version": 1, "method": "_session/queue",
            "actions": ["list", "add", "update", "delete", "reorder", "start"],
            "changedNotification": QUEUE_CHANGED, "turnNotification": QUEUE_TURN
        },
        "archive": {
            "version": 1, "archiveMethod": "_session/archive",
            "unarchiveMethod": "_session/unarchive"
        }
    });
    capabilities(meta.as_object())
}

struct QueueFixture {
    state: Arc<RwLock<SessionState>>,
    emitter: EventEmitter,
    events: broadcast::Receiver<Arc<EventEnvelope>>,
}

impl QueueFixture {
    fn new() -> Self {
        let mut state = SessionState::new(
            "native-test".into(),
            AgentType::Codex,
            None,
            "test-window".into(),
            None,
        );
        state.external_id = Some(SID.into());
        state.status = ConnectionStatus::Connected;
        state.native_capabilities = advertised_caps();
        let events = state.event_stream().subscribe();
        Self {
            state: Arc::new(RwLock::new(state)),
            emitter: EventEmitter::test_web_only(Arc::new(WebEventBroadcaster::new())),
            events,
        }
    }

    async fn notify(&self, method: &str, params: Value) -> bool {
        apply_native_queue_notification(
            &self.state,
            &self.emitter,
            AgentType::Codex,
            &UntypedMessage::new(method, params).unwrap(),
        )
        .await
    }

    async fn turn(&self, id: &str, status: &str) {
        assert!(
            self.notify(
                QUEUE_TURN,
                json!({
                    "sessionId": SID, "turn": {"id": id, "status": status}
                })
            )
            .await
        );
    }

    fn take_events(&mut self) -> Vec<AcpEvent> {
        let mut events = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(event) => events.push(event.payload.clone()),
                Err(broadcast::error::TryRecvError::Empty) => return events,
                Err(error) => panic!("event stream lost evidence: {error}"),
            }
        }
    }

    async fn assert_untouched(&mut self) {
        let state = self.state.read().await;
        assert_eq!(state.status, ConnectionStatus::Connected);
        assert!(state.native_queue_turn_id.is_none());
        assert!(!state.native_queue_pending);
        assert_eq!(state.native_queue_revision, 0);
        assert_eq!(state.turns_completed, 0);
        assert!(!state.agent_initiated_turn);
        drop(state);
        assert!(self.take_events().is_empty());
    }
}

#[tokio::test]
async fn native_queue_wrong_or_missing_session_never_changes_state() {
    for method in [QUEUE_CHANGED, QUEUE_TURN] {
        for sid in [
            Some(json!("another-session")),
            Some(Value::Null),
            Some(json!(7)),
            None,
        ] {
            let mut fixture = QueueFixture::new();
            let mut params = json!({"turn":{"id":TURN,"status":"inProgress"}});
            if let Some(sid) = sid {
                params["sessionId"] = sid;
            }
            fixture.notify(method, params).await;
            fixture.assert_untouched().await;
        }
    }
}

#[tokio::test]
async fn native_queue_unknown_capability_versions_never_enable_notifications() {
    for cap in [
        json!({}),
        json!({"queue":{"version":2}}),
        json!({"queue":{"version":"1"}}),
    ] {
        let mut fixture = QueueFixture::new();
        fixture.state.write().await.native_capabilities = capabilities(cap.as_object());
        for method in [QUEUE_CHANGED, QUEUE_TURN] {
            fixture
                .notify(
                    method,
                    json!({"sessionId":SID,"turn":{"id":TURN,"status":"inProgress"}}),
                )
                .await;
            fixture.assert_untouched().await;
        }
    }
}

#[tokio::test]
async fn native_queue_requires_the_exact_advertised_notification_method() {
    // Version alone cannot authorize an omitted or unknown notification method.
    for (field, method) in [
        ("turnNotification", QUEUE_TURN),
        ("changedNotification", QUEUE_CHANGED),
    ] {
        for value in [Value::Null, json!("_future/queue/event")] {
            let mut fixture = QueueFixture::new();
            let mut meta = advertised_caps();
            meta["queue"][field] = value;
            fixture.state.write().await.native_capabilities = capabilities(meta.as_object());
            fixture
                .notify(
                    method,
                    json!({"sessionId":SID,"turn":{"id":TURN,"status":"inProgress"}}),
                )
                .await;
            fixture.assert_untouched().await;
        }
    }
}

#[tokio::test]
async fn native_queue_wrong_agent_and_unknown_method_are_not_consumed() {
    let mut fixture = QueueFixture::new();
    let notification = UntypedMessage::new(
        QUEUE_TURN,
        json!({
            "sessionId":SID,"turn":{"id":TURN,"status":"inProgress"}
        }),
    )
    .unwrap();
    assert!(
        !apply_native_queue_notification(
            &fixture.state,
            &fixture.emitter,
            AgentType::ClaudeCode,
            &notification
        )
        .await
    );
    assert!(
        !fixture
            .notify("_session/queue/future", notification.params().clone())
            .await
    );
    fixture.assert_untouched().await;
}

#[tokio::test]
async fn native_queue_changed_is_invalidation_not_proof_of_empty_queue() {
    let mut fixture = QueueFixture::new();
    assert!(
        fixture
            .notify(QUEUE_CHANGED, json!({"sessionId":SID}))
            .await
    );
    let state = fixture.state.read().await;
    assert!(state.native_queue_pending);
    assert!(state.native_queue_turn_id.is_none());
    assert_eq!(state.status, ConnectionStatus::Connected);
    assert_eq!(state.native_queue_revision, 1);
    drop(state);
    assert!(fixture.take_events().is_empty());
}

#[tokio::test]
async fn native_queue_autonomous_turn_completes_fails_or_interrupts_once() {
    for (terminal, reason) in [
        ("completed", "end_turn"),
        ("failed", "error"),
        ("interrupted", "cancelled"),
    ] {
        let mut fixture = QueueFixture::new();
        fixture.turn(TURN, "inProgress").await;
        fixture.turn(TURN, "inProgress").await;
        {
            let state = fixture.state.read().await;
            assert_eq!(state.native_queue_turn_id.as_deref(), Some(TURN));
            assert_eq!(state.status, ConnectionStatus::Prompting);
            assert!(state.agent_initiated_turn);
            assert_eq!(
                state.native_queue_revision, 1,
                "duplicate opener is idempotent"
            );
        }
        emit_with_state(
            &fixture.state,
            &fixture.emitter,
            AcpEvent::ContentDelta {
                text: "autonomous answer".into(),
                parent_tool_use_id: None,
            },
        )
        .await;
        fixture.turn(TURN, terminal).await;
        fixture.turn(TURN, terminal).await;
        {
            let state = fixture.state.read().await;
            assert_eq!(state.status, ConnectionStatus::Connected);
            assert!(state.native_queue_turn_id.is_none());
            assert!(!state.agent_initiated_turn);
            assert_eq!(state.turns_completed, 1);
            assert_eq!(state.native_queue_revision, 2);
            assert_eq!(
                state.last_assistant_text.as_deref(),
                Some("autonomous answer")
            );
            assert!(
                state.native_queue_pending,
                "only a fresh list proves no more queued work"
            );
        }
        let events = fixture.take_events();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    AcpEvent::StatusChanged {
                        status: ConnectionStatus::Prompting
                    }
                ))
                .count(),
            1
        );
        let ends: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AcpEvent::TurnComplete {
                    session_id,
                    stop_reason,
                    ..
                } => Some((session_id.as_str(), stop_reason.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(ends, vec![(SID, reason)]);
    }
}

#[tokio::test]
async fn native_queue_nonmatching_terminal_cannot_end_active_turn() {
    let mut fixture = QueueFixture::new();
    fixture.turn(TURN, "inProgress").await;
    fixture.take_events();
    for terminal in ["completed", "failed", "interrupted"] {
        fixture.turn("old-turn", terminal).await;
    }
    let state = fixture.state.read().await;
    assert_eq!(state.native_queue_turn_id.as_deref(), Some(TURN));
    assert_eq!(state.status, ConnectionStatus::Prompting);
    assert_eq!(state.turns_completed, 0);
    assert_eq!(state.native_queue_revision, 1);
    drop(state);
    assert!(fixture.take_events().is_empty());
}

#[tokio::test]
async fn native_queue_late_duplicate_terminal_cannot_end_next_queue_turn() {
    let mut fixture = QueueFixture::new();
    fixture.turn("old-turn", "inProgress").await;
    fixture.turn("old-turn", "completed").await;
    fixture.turn(TURN, "inProgress").await;
    fixture.take_events();
    fixture.turn("old-turn", "completed").await;
    assert_eq!(
        fixture.state.read().await.native_queue_turn_id.as_deref(),
        Some(TURN)
    );
    assert_eq!(fixture.state.read().await.turns_completed, 1);
    assert!(fixture.take_events().is_empty());
}

#[tokio::test]
async fn native_queue_malformed_or_unknown_turn_status_cannot_open_turn() {
    for turn in [
        Value::Null,
        json!({}),
        json!({"id":4,"status":"inProgress"}),
        json!({"id":"","status":"inProgress"}),
        json!({"id":"  ","status":"inProgress"}),
        json!({"id":TURN,"status":"future"}),
        json!({"id":TURN}),
    ] {
        let mut fixture = QueueFixture::new();
        fixture
            .notify(QUEUE_TURN, json!({"sessionId":SID,"turn":turn}))
            .await;
        fixture.assert_untouched().await;
    }
}

async fn install_live_card(fixture: &QueueFixture) {
    emit_with_state(
        &fixture.state,
        &fixture.emitter,
        AcpEvent::PermissionRequest {
            request_id: "new-turn-permission".into(),
            tool_call: json!({"toolCallId":"new-tool"}),
            options: vec![],
            queued: 0,
        },
    )
    .await;
    emit_with_state(
        &fixture.state,
        &fixture.emitter,
        AcpEvent::ContentDelta {
            text: "new turn in flight".into(),
            parent_tool_use_id: None,
        },
    )
    .await;
}

fn old_host_complete() -> AcpEvent {
    AcpEvent::TurnComplete {
        session_id: SID.into(),
        stop_reason: "end_turn".into(),
        agent_type: AgentType::Codex.to_string(),
    }
}

#[tokio::test]
async fn native_queue_old_host_response_preserves_new_turn_and_permission() {
    let mut fixture = QueueFixture::new();
    fixture.state.write().await.turn_in_flight = true; // the host RPC is still outstanding
    fixture.turn(TURN, "inProgress").await;
    install_live_card(&fixture).await;
    fixture.take_events();
    drain_permissions_then_emit(
        &PendingPermissions::default(),
        &fixture.state,
        &fixture.emitter,
        old_host_complete(),
    )
    .await;
    let state = fixture.state.read().await;
    assert_eq!(state.status, ConnectionStatus::Prompting);
    assert_eq!(state.turns_completed, 0);
    assert!(state.pending_permission.is_some());
    assert!(state.live_message.is_some());
    drop(state);
    assert!(fixture.take_events().is_empty());
}

#[tokio::test]
async fn native_queue_takeover_does_not_merge_old_host_output_into_new_answer() {
    let fixture = QueueFixture::new();
    fixture.state.write().await.turn_in_flight = true;
    emit_with_state(
        &fixture.state,
        &fixture.emitter,
        AcpEvent::ContentDelta {
            text: "old host answer".into(),
            parent_tool_use_id: None,
        },
    )
    .await;
    // The queued turn starts while the completed host turn's RPC response is
    // still in transit. Its content must start a distinct live message.
    fixture.turn(TURN, "inProgress").await;
    emit_with_state(
        &fixture.state,
        &fixture.emitter,
        AcpEvent::ContentDelta {
            text: "new queue answer".into(),
            parent_tool_use_id: None,
        },
    )
    .await;
    drain_permissions_then_emit(
        &PendingPermissions::default(),
        &fixture.state,
        &fixture.emitter,
        old_host_complete(),
    )
    .await;
    fixture.turn(TURN, "completed").await;
    let state = fixture.state.read().await;
    assert_eq!(
        state.last_assistant_text.as_deref(),
        Some("new queue answer"),
        "the new autonomous turn must not inherit the prior host's live buffer"
    );
    assert!(
        !state.turn_in_flight,
        "the superseded host gate must eventually be released"
    );
}

#[tokio::test]
async fn native_queue_failure_before_old_response_is_not_overwritten_as_success() {
    let mut fixture = QueueFixture::new();
    fixture.state.write().await.turn_in_flight = true;
    fixture.turn(TURN, "inProgress").await;
    fixture.turn(TURN, "failed").await;
    fixture.take_events();
    assert!(fixture.state.read().await.last_turn_ended_abnormally);
    // Presence-only fencing forgets the superseded response as soon as the
    // native turn ends. The old RPC must not publish another completion.
    drain_permissions_then_emit(
        &PendingPermissions::default(),
        &fixture.state,
        &fixture.emitter,
        old_host_complete(),
    )
    .await;
    assert!(
        fixture.state.read().await.last_turn_ended_abnormally,
        "the old host success must not relabel a failed queue turn as successful"
    );
    assert!(!fixture
        .take_events()
        .iter()
        .any(|e| matches!(e, AcpEvent::TurnComplete { .. })));
}

#[tokio::test]
async fn native_queue_start_while_old_host_waits_for_permission_lock_is_not_cleared() {
    let mut fixture = QueueFixture::new();
    fixture.state.write().await.turn_in_flight = true;
    let perms = PendingPermissions::default();
    let lock = perms.lock().await;
    let mut old_response = Box::pin(drain_permissions_then_emit(
        &perms,
        &fixture.state,
        &fixture.emitter,
        old_host_complete(),
    ));
    // Poll exactly to the held mutex; no sleep or scheduler-dependent race.
    assert!(futures::poll!(old_response.as_mut()).is_pending());
    fixture.turn(TURN, "inProgress").await;
    install_live_card(&fixture).await;
    drop(lock);
    old_response.await;
    let state = fixture.state.read().await;
    assert_eq!(
        state.status,
        ConnectionStatus::Prompting,
        "recheck ownership after waiting for the permission mutex"
    );
    assert_eq!(state.turns_completed, 0);
    assert!(state.pending_permission.is_some());
    assert!(state.live_message.is_some());
    drop(state);
    assert!(!fixture
        .take_events()
        .iter()
        .any(|event| matches!(event, AcpEvent::TurnComplete { .. })));
}

#[tokio::test]
async fn native_queue_session_started_after_load_does_not_downgrade_active_turn() {
    let fixture = QueueFixture::new();
    fixture.turn(TURN, "inProgress").await;
    install_live_card(&fixture).await;
    emit_with_state(
        &fixture.state,
        &fixture.emitter,
        AcpEvent::SessionStarted {
            session_id: SID.into(),
        },
    )
    .await;
    let state = fixture.state.read().await;
    assert_eq!(state.status, ConnectionStatus::Prompting);
    assert_eq!(state.native_queue_turn_id.as_deref(), Some(TURN));
    assert!(state.pending_permission.is_some());
    assert!(state.live_message.is_some());
}

#[tokio::test]
async fn native_queue_changing_session_drops_previous_sessions_queue_fence() {
    let fixture = QueueFixture::new();
    fixture.turn(TURN, "inProgress").await;
    emit_with_state(
        &fixture.state,
        &fixture.emitter,
        AcpEvent::SessionStarted {
            session_id: "replacement-session".into(),
        },
    )
    .await;
    let state = fixture.state.read().await;
    assert_eq!(state.external_id.as_deref(), Some("replacement-session"));
    assert!(
        state.native_queue_turn_id.is_none(),
        "old-session turn cannot keep the replacement busy forever"
    );
    assert!(!state.native_queue_pending);
    assert!(!state.agent_initiated_turn);
    assert_eq!(state.status, ConnectionStatus::Connected);
}

// Real ACP serialization/dispatch with a scripted peer. Handlers hand their
// responders to the test, so withheld replies cannot block other requests.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_session/queue", response = Value)]
#[serde(transparent)]
struct NativeQueueRequest(Value);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_session/archive", response = Value)]
#[serde(transparent)]
struct NativeArchiveRequest(Value);

struct NativeIncoming {
    method: &'static str,
    params: Value,
    responder: Responder<Value>,
}

type NativeReply = oneshot::Receiver<Result<Value, AcpError>>;

struct NativePeer {
    commands: mpsc::UnboundedSender<(
        NativeOperation,
        Value,
        oneshot::Sender<Result<Value, AcpError>>,
    )>,
    incoming: mpsc::UnboundedReceiver<NativeIncoming>,
    client: tokio::task::JoinHandle<()>,
    agent: tokio::task::JoinHandle<()>,
}

impl NativePeer {
    fn start(state: Arc<RwLock<SessionState>>) -> Self {
        let (client_end, agent_end) = agent_client_protocol::Channel::duplex();
        let (incoming_tx, incoming) = mpsc::unbounded_channel();
        let archive_tx = incoming_tx.clone();
        let (commands, mut commands_rx) = mpsc::unbounded_channel();
        let agent = tokio::spawn(async move {
            let _ = Agent
                .builder()
                .on_receive_request(
                    async move |request: NativeQueueRequest,
                                responder: Responder<Value>,
                                _cx: ConnectionTo<Client>| {
                        let _ = incoming_tx.send(NativeIncoming {
                            method: "_session/queue",
                            params: request.0,
                            responder,
                        });
                        Ok(())
                    },
                    on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: NativeArchiveRequest,
                                responder: Responder<Value>,
                                _cx: ConnectionTo<Client>| {
                        let _ = archive_tx.send(NativeIncoming {
                            method: "_session/archive",
                            params: request.0,
                            responder,
                        });
                        Ok(())
                    },
                    on_receive_request!(),
                )
                .connect_with(agent_end, async |_cx: ConnectionTo<Client>| {
                    std::future::pending::<Result<(), agent_client_protocol::Error>>().await
                })
                .await;
        });
        let client = tokio::spawn(async move {
            let _ = Client
                .builder()
                .connect_with(client_end, async move |cx: ConnectionTo<Agent>| {
                    while let Some((operation, params, reply)) = commands_rx.recv().await {
                        dispatch_native_operation(
                            &cx,
                            &SessionId::new(SID),
                            &state,
                            operation,
                            params,
                            reply,
                        );
                    }
                    Ok(())
                })
                .await;
        });
        Self {
            commands,
            incoming,
            client,
            agent,
        }
    }

    fn call(&self, operation: NativeOperation, params: Value) -> NativeReply {
        let (tx, rx) = oneshot::channel();
        assert!(self.commands.send((operation, params, tx)).is_ok());
        rx
    }

    async fn request(&mut self) -> NativeIncoming {
        tokio::time::timeout(std::time::Duration::from_secs(2), self.incoming.recv())
            .await
            .expect("native request must reach peer")
            .expect("peer remains alive")
    }

    fn assert_no_request(&mut self) {
        assert!(
            matches!(
                self.incoming.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ),
            "rejected operation must not reach the wire"
        );
    }
}

impl Drop for NativePeer {
    fn drop(&mut self) {
        self.client.abort();
        self.agent.abort();
    }
}

async fn reply(rx: NativeReply) -> Result<Value, AcpError> {
    tokio::time::timeout(std::time::Duration::from_secs(2), rx)
        .await
        .expect("dispatch must settle")
        .expect("dispatch must answer caller")
}

fn queue_add() -> Value {
    json!({"action":"add","clientUserMessageId":"user-1","input":[{"type":"text","text":"queued prompt"}]})
}

#[tokio::test]
async fn native_dispatch_rejects_unknown_caps_and_injected_identity_before_wire() {
    for (caps, params) in [
        (json!({}), json!({"action":"list"})),
        (
            json!({"queue":{"version":2,"method":"_session/queue","actions":["list"]}}),
            json!({"action":"list"}),
        ),
        (
            json!({"queue":{"version":1,"method":"_future/queue","actions":["list"]}}),
            json!({"action":"list"}),
        ),
        (advertised_caps(), json!({"action":"future"})),
        (
            advertised_caps(),
            json!({"action":"list","sessionId":"attacker-session"}),
        ),
        (advertised_caps(), json!({"action":"list","sessionId":SID})),
    ] {
        let fixture = QueueFixture::new();
        fixture.state.write().await.native_capabilities = capabilities(caps.as_object());
        let mut peer = NativePeer::start(fixture.state.clone());
        assert!(reply(peer.call(NativeOperation::Queue, params))
            .await
            .is_err());
        peer.assert_no_request();
        let state = fixture.state.read().await;
        assert!(!state.native_mutation_in_flight);
        assert!(
            !state.native_recovery_required,
            "pre-dispatch rejection has no uncertain side effect"
        );
    }
}

#[tokio::test]
async fn native_dispatch_injects_bound_session_and_preserves_native_refusal() {
    let fixture = QueueFixture::new();
    let mut peer = NativePeer::start(fixture.state.clone());
    let rx = peer.call(NativeOperation::Queue, queue_add());
    let request = peer.request().await;
    assert_eq!(request.method, "_session/queue");
    assert_eq!(request.params["sessionId"], SID);
    assert!(fixture.state.read().await.native_mutation_in_flight);
    let refusal = json!({"status":"unavailable","reason":"native queue unavailable"});
    request.responder.respond(refusal.clone()).unwrap();
    assert_eq!(reply(rx).await.unwrap(), refusal);
    let state = fixture.state.read().await;
    assert!(!state.native_queue_pending);
    assert!(!state.native_recovery_required);
    assert!(!state.native_mutation_in_flight);
}

#[tokio::test]
async fn native_dispatch_mutation_error_fences_followups_without_retry() {
    let fixture = QueueFixture::new();
    let mut peer = NativePeer::start(fixture.state.clone());
    let rx = peer.call(NativeOperation::Queue, queue_add());
    peer.request()
        .await
        .responder
        .respond_with_internal_error("mutation may already have happened")
        .unwrap();
    assert!(reply(rx).await.is_err());
    assert!(fixture.state.read().await.native_recovery_required);
    assert!(!fixture.state.read().await.native_mutation_in_flight);
    assert!(reply(peer.call(NativeOperation::Queue, queue_add()))
        .await
        .is_err());
    peer.assert_no_request();
}

#[tokio::test(start_paused = true)]
async fn native_dispatch_timeout_fences_only_mutations_and_does_not_replay() {
    for (params, mutation) in [(queue_add(), true), (json!({"action":"list"}), false)] {
        let fixture = QueueFixture::new();
        let mut peer = NativePeer::start(fixture.state.clone());
        let mut rx = peer.call(NativeOperation::Queue, params);
        let withheld = peer.request().await;
        tokio::time::advance(std::time::Duration::from_secs(44)).await;
        assert!(matches!(
            rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        let error = reply(rx)
            .await
            .expect_err("no response cannot become a success");
        assert!(error.to_string().contains("timed out"), "{error}");
        let state = fixture.state.read().await;
        assert_eq!(state.native_recovery_required, mutation);
        assert!(!state.native_mutation_in_flight);
        drop(state);
        peer.assert_no_request();
        drop(withheld);
    }
}

#[tokio::test]
async fn native_dispatch_read_error_does_not_poison_session() {
    let fixture = QueueFixture::new();
    let mut peer = NativePeer::start(fixture.state.clone());
    let rx = peer.call(NativeOperation::Queue, json!({"action":"list"}));
    peer.request()
        .await
        .responder
        .respond_with_internal_error("read unavailable")
        .unwrap();
    assert!(reply(rx).await.is_err());
    assert!(!fixture.state.read().await.native_recovery_required);
    let retry = peer.call(NativeOperation::Queue, queue_add());
    peer.request()
        .await
        .responder
        .respond(json!({"status":"ok"}))
        .unwrap();
    assert!(reply(retry).await.is_ok());
}

#[tokio::test]
async fn native_dispatch_cancelled_caller_does_not_release_mutation_early() {
    let fixture = QueueFixture::new();
    let mut peer = NativePeer::start(fixture.state.clone());
    let rx = peer.call(NativeOperation::Queue, queue_add());
    let request = peer.request().await;
    drop(rx);
    assert!(fixture.state.read().await.native_mutation_in_flight);
    assert!(reply(peer.call(NativeOperation::Queue, queue_add()))
        .await
        .is_err());
    peer.assert_no_request();
    request
        .responder
        .respond_with_internal_error("lost mutation result")
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fixture.state.read().await.native_mutation_in_flight {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(fixture.state.read().await.native_recovery_required);
}

#[tokio::test]
async fn native_dispatch_requires_idle_for_archive_in_all_active_states() {
    for gate in ["host", "native", "prompting"] {
        let fixture = QueueFixture::new();
        {
            let mut state = fixture.state.write().await;
            match gate {
                "host" => state.turn_in_flight = true,
                "native" => state.native_queue_turn_id = Some(TURN.into()),
                _ => state.status = ConnectionStatus::Prompting,
            }
        }
        let mut peer = NativePeer::start(fixture.state.clone());
        assert!(matches!(
            reply(peer.call(NativeOperation::Archive, json!({}))).await,
            Err(AcpError::TurnInProgress)
        ));
        peer.assert_no_request();
    }
}

#[tokio::test]
async fn native_dispatch_busy_queue_still_accepts_supported_queue_controls() {
    let fixture = QueueFixture::new();
    fixture.turn(TURN, "inProgress").await;
    let mut peer = NativePeer::start(fixture.state.clone());
    let rx = peer.call(NativeOperation::Queue, queue_add());
    peer.request()
        .await
        .responder
        .respond(json!({"status":"ok"}))
        .unwrap();
    assert!(reply(rx).await.is_ok());
    assert_eq!(
        fixture.state.read().await.native_queue_turn_id.as_deref(),
        Some(TURN)
    );
}

#[tokio::test]
async fn native_dispatch_stale_queue_list_cannot_clear_new_invalidation() {
    let fixture = QueueFixture::new();
    let mut peer = NativePeer::start(fixture.state.clone());
    let rx = peer.call(NativeOperation::Queue, json!({"action":"list"}));
    let request = peer.request().await;
    fixture
        .notify(QUEUE_CHANGED, json!({"sessionId":SID}))
        .await;
    request
        .responder
        .respond(json!({"status":"ok","result":{"data":[],"nextCursor":null}}))
        .unwrap();
    reply(rx).await.unwrap();
    assert!(fixture.state.read().await.native_queue_pending);
}

#[tokio::test]
async fn native_dispatch_only_fresh_empty_first_page_clears_queue_fence() {
    for (params, data, cursor, pending) in [
        (json!({"action":"list"}), json!([]), Value::Null, false),
        (
            json!({"action":"list","cursor":"page-2"}),
            json!([]),
            Value::Null,
            true,
        ),
        (json!({"action":"list"}), json!([]), json!("page-2"), true),
        (
            json!({"action":"list"}),
            json!([{"id":"pending-1"}]),
            Value::Null,
            true,
        ),
    ] {
        let fixture = QueueFixture::new();
        fixture.state.write().await.native_queue_pending = true;
        let mut peer = NativePeer::start(fixture.state.clone());
        let rx = peer.call(NativeOperation::Queue, params);
        peer.request()
            .await
            .responder
            .respond(json!({"status":"ok","result":{"data":data,"nextCursor":cursor}}))
            .unwrap();
        reply(rx).await.unwrap();
        assert_eq!(fixture.state.read().await.native_queue_pending, pending);
    }
}

#[tokio::test]
async fn native_dispatch_read_completion_must_not_unlock_another_mutation() {
    // Defense in depth at dispatch: manager serialization must not be assumed
    // by a connection helper which owns the mutation flag itself.
    let fixture = QueueFixture::new();
    let mut peer = NativePeer::start(fixture.state.clone());
    let read_rx = peer.call(NativeOperation::Queue, json!({"action":"list"}));
    let read = peer.request().await;
    let mutation_rx = peer.call(NativeOperation::Queue, queue_add());
    let mutation = peer.request().await;
    read.responder
        .respond(json!({"status":"ok","result":{"data":[]}}))
        .unwrap();
    reply(read_rx).await.unwrap();
    assert!(
        fixture.state.read().await.native_mutation_in_flight,
        "a read does not own another operation's mutation flag"
    );
    assert!(reply(peer.call(NativeOperation::Queue, queue_add()))
        .await
        .is_err());
    peer.assert_no_request();
    mutation.responder.respond(json!({"status":"ok"})).unwrap();
    reply(mutation_rx).await.unwrap();
}

async fn manager_fixture() -> (
    ConnectionManager,
    Arc<RwLock<SessionState>>,
    mpsc::Receiver<ConnectionCommand>,
) {
    let manager = ConnectionManager::new();
    let receiver = manager
        .insert_test_connection_live(
            "native-test",
            AgentType::Codex,
            None,
            EventEmitter::test_web_only(Arc::new(WebEventBroadcaster::new())),
        )
        .await;
    let state = manager.get_state("native-test").await.unwrap();
    {
        let mut state = state.write().await;
        state.external_id = Some(SID.into());
        state.native_capabilities = advertised_caps();
    }
    (manager, state, receiver)
}

#[tokio::test]
async fn native_manager_prompt_is_rejected_by_each_native_lifecycle_gate() {
    for gate in ["mutation", "uncertain", "pending", "active"] {
        let (manager, state, mut commands) = manager_fixture().await;
        {
            let mut state = state.write().await;
            match gate {
                "mutation" => state.native_mutation_in_flight = true,
                "uncertain" => state.native_recovery_required = true,
                "pending" => state.native_queue_pending = true,
                _ => state.native_queue_turn_id = Some(TURN.into()),
            }
        }
        assert!(
            manager
                .send_prompt(
                    "native-test",
                    vec![PromptInputBlock::Text {
                        text: "ordinary prompt".into()
                    }]
                )
                .await
                .is_err(),
            "gate: {gate}"
        );
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(!state.read().await.turn_in_flight);
    }
}

#[tokio::test]
async fn native_manager_mode_change_is_rejected_during_mutation_or_uncertainty() {
    for uncertain in [false, true] {
        let (manager, state, mut commands) = manager_fixture().await;
        {
            let mut state = state.write().await;
            state.native_recovery_required = uncertain;
            state.native_mutation_in_flight = !uncertain;
        }
        assert!(
            manager
                .set_mode("native-test", "plan".into())
                .await
                .is_err(),
            "configuration cannot overlap a native mutation or unknown outcome"
        );
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}

#[tokio::test]
async fn native_manager_config_change_is_rejected_during_mutation_or_uncertainty() {
    for uncertain in [false, true] {
        let (manager, state, mut commands) = manager_fixture().await;
        {
            let mut state = state.write().await;
            state.native_recovery_required = uncertain;
            state.native_mutation_in_flight = !uncertain;
        }
        assert!(
            manager
                .set_config_option("native-test", "model".into(), "another-model".into())
                .await
                .is_err(),
            "provider replacement must not race a native operation"
        );
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}
