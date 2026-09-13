use super::{
    models::InboundMessage, snapshots, transport::OUTBOUND_CAPACITY, valid_session, SessionContext,
};
use crate::AppState;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    sync::mpsc,
    time::{Duration, Instant, MissedTickBehavior},
};

pub async fn websocket(
    State(state): State<AppState>,
    session: SessionContext,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    Ok(ws
        .max_message_size(64 * 1024)
        .on_upgrade(move |socket| handle(state, session, socket)))
}

/// Jellyfin's periodic subscription data is "initialDelayMs,intervalMs".
fn subscription_interval(data: &serde_json::Value) -> Option<(Duration, Duration)> {
    let value = data.as_str()?;
    let mut parts = value.split(',');
    let initial: u64 = parts.next()?.trim().parse().ok()?;
    let interval: u64 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((
        Duration::from_millis(initial.min(30_000)),
        Duration::from_millis(interval.clamp(250, 30_000)),
    ))
}

async fn handle(state: AppState, session: SessionContext, socket: WebSocket) {
    let (mut sender, mut receiver) = socket.split();
    let (tx, mut rx) = mpsc::channel(OUTBOUND_CAPACITY);
    let generation = state
        .syncplay
        .register_websocket(session.session_id.clone(), tx)
        .await;
    let transport = &state.client_sessions.transport;
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut subscription: Option<(Instant, Duration)> = None;
    let mut auth_check = Instant::now();
    let mut last_inbound = Instant::now();
    loop {
        tokio::select! {
            outbound = rx.recv() => {
                let Some(text) = outbound else { break; };
                if !transport.is_current(&session.session_id, generation) { break; }
                if !tokio::time::timeout(Duration::from_secs(10), sender.send(Message::Text(text.into()))).await.is_ok_and(|r| r.is_ok()) { break; }
            }
            inbound = receiver.next() => {
                let Some(Ok(inbound)) = inbound else { break; };
                if !transport.is_current(&session.session_id, generation) { break; }
                last_inbound = Instant::now();
                state.client_sessions.touch(&session.session_id).await;
                match inbound {
                    Message::Text(text) => if let Ok(message) = serde_json::from_str::<InboundMessage>(&text) {
                        match message.message_type.to_ascii_lowercase().as_str() {
                            "keepalive" => { let _ = transport.send(&session.session_id, "KeepAlive", &serde_json::Value::Null); }
                            "sessionsstart" => if let Some((delay, period)) = subscription_interval(&message.data) { subscription = Some((Instant::now() + delay, period)); },
                            "sessionsstop" => subscription = None,
                            _ => {}
                        }
                    },
                    Message::Ping(payload) => {
                        if !tokio::time::timeout(Duration::from_secs(10), sender.send(Message::Pong(payload))).await.is_ok_and(|r| r.is_ok()) { break; }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            _ = interval.tick() => {
                if !transport.is_current(&session.session_id, generation) || last_inbound.elapsed() > Duration::from_secs(60) { break; }
                if auth_check.elapsed() >= Duration::from_secs(15) {
                    let Some(current) = state.client_sessions.get(&session.session_id).await else { break; };
                    if !matches!(valid_session(&state, &current).await, Ok(true)) { break; }
                    auth_check = Instant::now();
                }
                if let Some((next, period)) = subscription {
                    if Instant::now() >= next {
                        match snapshots(&state, &session.user.id).await {
                            Ok(sessions) => if transport.send(&session.session_id, "Sessions", &sessions).is_err() { break; },
                            Err(_) => break,
                        }
                        subscription = Some((Instant::now() + period, period));
                    }
                }
            }
        }
    }
    state
        .syncplay
        .unregister_websocket_with_grace(session.session_id, generation)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subscriptions_are_bounded_and_invalid_requests_are_ignored() {
        assert_eq!(
            subscription_interval(&serde_json::json!("100,800")),
            Some((Duration::from_millis(100), Duration::from_millis(800)))
        );
        assert_eq!(
            subscription_interval(&serde_json::json!("0,0")).unwrap().1,
            Duration::from_millis(250)
        );
        assert!(subscription_interval(&serde_json::json!("100,-1")).is_none());
    }
}
