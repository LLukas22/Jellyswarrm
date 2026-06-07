# Request and Response Processing

Jellyswarrm is a Jellyfin-aware reverse proxy. Clients see Jellyswarrm user IDs, media IDs, server IDs, and API keys. Upstream Jellyfin servers receive their original IDs and credentials.

The code keeps three concerns separate:

- Request preprocessing chooses the upstream server/session and rewrites the outgoing URL/auth.
- Request and response JSON processing rewrites IDs inside bodies.
- URL processing rewrites IDs and credentials in request URLs and embedded media delivery URLs.

Handlers should use the `extractors` module, `ProxyProcessors`, and `AppState::process_response_json` instead of calling low-level processors directly.

## Request Flow

```mermaid
flowchart TD
    A[Incoming Axum request] --> B{Route handler type}

    B -->|Routed handler| C[Preprocessed / RequireUser / RequireSession / RequireUserSession]
    B -->|Catch-all proxy_handler| D[Static asset check]
    D -->|Asset found| E[Return asset]
    D -->|No asset| F[preprocess_request]
    C --> F

    F --> G[axum_to_reqwest]
    G --> H[extract_request_infos]
    H --> I[Parse auth headers/query]
    H --> J[Resolve user and device]
    H --> K{JSON request body?}

    K -->|yes| L[RequestAnalyzer]
    L --> M[Find media/user/session hints]
    K -->|no| N[No body hints]
    M --> O[Load user sessions]
    N --> O
    J --> O

    O --> P[resolve_server]
    P --> Q[UrlProcessor.server_from_client_url]
    Q --> R[Pick upstream server/session]
    R --> S[remap_authorization]
    S --> T[apply_to_request]
    T --> U[Remove hop-by-hop headers]
    T --> V[Set Host/Auth headers]
    T --> W[UrlProcessor.client_to_server_url]
    W --> X[Virtual IDs/API key -> upstream IDs/token in path/query]

    X --> Y{Forwarded body is JSON?}
    Y -->|yes| Z[ProxyProcessors.process_request_body]
    Z --> AA[RequestProcessor rewrites JSON virtual IDs -> upstream IDs]
    AA --> AB[set_json_body if modified]
    Y -->|no| AC[Forward request]
    AB --> AC
```

## Response Flow

```mermaid
flowchart TD
    A[Upstream response] --> B{Handler path}

    B -->|Item/media JSON handlers| C[Deserialize as serde_json::Value]
    B -->|Fallback JSON response| D[Deserialize as serde_json::Value]
    B -->|Typed special handlers| E[Deserialize typed model]
    B -->|Non-JSON response| F[Pass through]

    E --> G[Run typed behavior]
    G --> H[Example: playback session tracking]
    G --> I[Serialize typed response to JSON if rewriting is needed]

    C --> J[process_response_json profile: Media]
    D --> K[process_response_json profile: BestEffortMedia]
    I --> J

    J --> L[ResponseProcessor]
    K --> L
    L --> M[Rewrite upstream media IDs -> virtual IDs]
    L --> N[Rewrite ServerId -> proxy server id]
    L --> O[Disable CanDelete/CanDownload]
    L --> P[Optionally suffix names with server name]
    L --> Q[DeliveryUrl/StreamUrl/TranscodingUrl]
    Q --> R[UrlProcessor.server_to_client_delivery_url]
    R --> S[Rewrite embedded path/query IDs and api_key]

    S --> T[Processed JSON]
    M --> T
    N --> T
    O --> T
    P --> T
    T --> U{Typed handler?}
    U -->|yes| V[Deserialize JSON back to typed model]
    U -->|no| W[Serialize JSON body]
    V --> X[Return response]
    W --> Y[Update Content-Length]
    Y --> X
    F --> X
```

## Response Profiles

- `Media`: full media response rewriting for explicitly routed media/item/playback/federated handlers.
- `BestEffortMedia`: full media-like rewriting for catch-all JSON responses. This keeps less common Jellyfin endpoints working even when they are not explicitly routed.
- `Disabled`: no response rewriting.

`Media` and `BestEffortMedia` currently rewrite the same fields. The main difference is how they are selected: explicit handlers use `Media`; the generic fallback uses `BestEffortMedia`. Name suffixing is still controlled separately by `should_change_name` and config.

## Component Boundaries

- `extractors.rs`: Axum extractors that run preprocessing and optionally require a resolved user, session, or both.
- `request_preprocessing.rs`: request identity extraction, session lookup, server selection, auth remapping, and outgoing URL/header rewriting.
- `processors/url_processor.rs`: all path/query URL rewriting for client-to-server request URLs, server-to-client delivery URLs, and request server detection.
- `processors/request_analyzer.rs`: scans incoming JSON bodies for media IDs, user IDs, and session IDs that help pick the upstream server/session.
- `processors/request_processor.rs`: rewrites JSON request body IDs from Jellyswarrm virtual IDs to upstream Jellyfin IDs.
- `processors/response_processor.rs`: rewrites JSON response fields from upstream Jellyfin IDs to Jellyswarrm virtual IDs and delegates embedded URL rewriting to `UrlProcessor`.
- `processors/json_processor.rs`: generic recursive JSON walker used by analyzers and processors.
- `processors/field_matcher.rs`: centralized field-name groups for JSON rewrite rules.
- `ProxyProcessors`: facade that constructs and coordinates request, response, analyzer, and URL processors.

## Design Rules

- Prefer `serde_json::Value` for pass-through media/item responses so unknown Jellyfin schema changes are preserved.
- Keep typed models where the proxy performs behavior beyond simple transformation, such as playback session tracking and federated item interleaving.
- Keep URL rewriting centralized in `UrlProcessor`; request URL rules and embedded delivery URL rules should not drift.
- Keep handler signatures expressive: use `Preprocessed`, `RequireUser`, `RequireSession`, or `RequireUserSession` instead of manually calling `preprocess_request` in routed handlers.
