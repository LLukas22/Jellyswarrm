# Federated media pipeline

```mermaid
flowchart TD
    A[Validate caller and resolve authorized catalog targets]
    A --> B{Reusable query snapshot?}
    B -->|Hit| H[Apply client StartIndex and Limit]
    B -->|Miss or ineligible| C[Parse query policy; translate request once per backend]
    C --> D[Fetch pages or bounded feeds; validate scan completeness]
    D --> E[Translate response IDs and URLs; preserve original titles]
    E --> F[Group libraries, reconcile media identities, or interleave items]
    F --> G[Restore parent links; sort original metadata; render display labels]
    G --> S[Store eligible snapshot]
    S --> H
    H --> I[Wrap response; cache visible metadata; return]
```

- Pageable queries reuse a 60-second, 64-MiB-weighted cache, scoped to viewer,
  token, query, backend sessions/targets, and display configuration. Concurrent
  identical builds coalesce; `Limit=0` uses the same snapshot. Library roots,
  Resume, Next Up, and latest/suggestions bypass it; latest/suggestions remain
  bounded feeds. Successful proxied mutations advance the viewer's cache
  revision, including when an older snapshot build is still in flight.
- Each backend scan advances by received counts and validates totals, offsets,
  response shape, and unique IDs. The configured timeout (clamped to 1–300
  seconds) applies to each page. Counted catalogs scan to their validated total;
  uncounted sources retain defensive limits of 100,000 items and 10,000 pages.
  Incomplete/failing sources are excluded while healthy sources remain available.
  If every source fails, the request returns 503. Counts describe the available
  result set. Degraded responses are not cached, so recovery is retried promptly.
  Only fully scanned sources can replace their own sightings. Recursive,
  unfiltered type-specific scans (such as `IncludeItemTypes=Movie`) replace only
  those types; per-type generations prevent older scans from reviving removals.
  Missing sources are pruned only by an all-type inventory when every authorized
  target was available and succeeded.
  Authorization scope includes offline backends independently of active sessions.
  Bounded feeds are additive observations, never complete inventories.
- `Items?Ids=...` resolves every requested identity against the caller's mapped
  servers before fetching available members. Offline entries do not block online
  entries; an entirely unavailable lookup returns 503. Aggregates keep their
  requested IDs and advertised source ownership is recorded for queue playback,
  even when the client has not opened an item detail page.
  Configured and automatic library IDs use the same folder presentation as the
  root listing, including their virtual identity, name, and concrete artwork IDs.
- Complete, unfiltered user-view inventories persist each viewer's visible
  library members. Configured-library routing requires compatible visible members
  once an inventory exists; failed scans preserve prior observations, and older
  scans cannot overwrite newer ones. Library detail retries other eligible
  members when a backend rejects or cannot serve the selected member.
- Streaming session aliases normalize media UUIDs, including dashed and uppercase
  forms. Explicit playback sessions retain their item/backend binding; requests
  with an explicit media source but no session do not adopt another player's
  session. Subtitle and attachment source IDs in the path have the same routing
  authority as `MediaSourceId` in the query, and conflicting selections fail
  before forwarding.
- Transcoder cancellation (`DELETE /Videos/ActiveEncodings`) requires a known
  playback session owned by the caller and targets that session's backend.
  Unknown, missing, or conflicting session IDs are rejected; an unavailable
  owner returns 503. Cancellation retains the playback binding for stream changes.
- Field sorts use a final ID tiebreaker. Random queries scan in name order and
  shuffle once per snapshot; Next Up and backend-only rankings retain upstream
  order. Snapshot consistency lasts until expiry/eviction; upstream APIs provide
  no revision token to detect every same-count catalog edit during a scan.
- Library roots translate raw members while grouping. Media reconciliation
  consumes translated IDs and records display decisions without changing titles.
  Numbered-child identity and parent links prefer the current catalog's parent
  groups, so partial search/latest memberships cannot veto a fuller inventory.
  Numbered episodes include their end index: identical ranges can be versions,
  including multiple copies on one server; combined and standalone episodes
  stay distinct. Playback sessions retain the exact selected backend item for
  streaming and start/progress/stopped reports. Other same-server ambiguities
  remain visible.
