# SyncPlay Module (Simplified)

This folder contains Jellyswarrm's local SyncPlay implementation.

## What this module does

- Handles SyncPlay HTTP endpoints directly (`/SyncPlay/*`)
- Handles websocket SyncPlay traffic directly (`/websocket` and `/socket`)
- Keeps group state in memory (no upstream forwarding for SyncPlay)
- Sends SyncPlay websocket messages to connected group members

In short: Jellyswarrm acts as the SyncPlay coordinator.

## File layout

- `mod.rs`
  - Module entry point and exports.
- `models.rs`
  - SyncPlay wire DTOs, enums, websocket envelope payload models, and shared group/member structs.
- `routes.rs`
  - Axum handlers for HTTP + websocket endpoints.
  - Request auth/session extraction.
  - Library access checks before join/queue updates.
- `service.rs`
  - `SyncPlayService` in-memory coordinator.
  - Group lifecycle, queue/state updates, websocket fanout, waiting/ready resolution.

## Runtime flow

1. A client calls a SyncPlay endpoint (for example `/SyncPlay/New` or `/SyncPlay/Queue`).
2. `routes.rs` resolves the Jellyswarrm session from headers/query token.
3. `routes.rs` calls into `SyncPlayService`.
4. `SyncPlayService` updates in-memory group state.
5. `SyncPlayService` emits websocket updates/commands to affected group sessions.

## State model (high-level)

- Groups can be `Idle`, `Waiting`, `Paused`, or `Playing`.
- Queue entries have:
  - `ItemId` (media id used by client/proxy)
  - `PlaylistItemId` (generated UUID per queue entry)
- Waiting/Ready logic:
  - Members report buffering/ready.
  - When all relevant members are ready (or ignored), state resolves.
  - Unpause can be delayed based on highest group ping.

## Websocket messages used

Outbound message envelope:

- `MessageType`
- `MessageId`
- `Data`

Main SyncPlay message types:

- `SyncPlayCommand`
- `SyncPlayGroupUpdate`

Keepalive support:

- `ForceKeepAlive` on connect
- `KeepAlive` response when client sends keepalive

## Access and safety checks

- Join and queue changes validate library visibility based on media mapping + user sessions.
- On denial, requester gets `LibraryAccessDenied` group update.
- If websocket disconnects, the session is removed from its group.
