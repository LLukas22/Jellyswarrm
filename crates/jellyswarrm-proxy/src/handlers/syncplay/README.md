# SyncPlay

SyncPlay coordinates group playback inside Jellyswarrm, allowing a group to
play media from different backends. `/SyncPlay/*` handlers update group state
and send commands to members through the shared client-session transport.

Group membership and synchronization live here. Client identity, WebSockets,
and delivery are shared with remote control in the
[sessions module](../../sessions/README.md).
