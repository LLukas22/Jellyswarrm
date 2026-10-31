# SyncPlay

SyncPlay coordinates group playback inside Jellyswarrm, allowing a group to
play media from different backends. `/SyncPlay/*` handlers update group state
and send commands to members through the shared client-session connections.

Client identity, WebSockets and delivery are shared with remote control in the
[sessions module](../sessions/README.md). The HTTP handlers are in
[`http/syncplay.rs`](../http/syncplay.rs).
