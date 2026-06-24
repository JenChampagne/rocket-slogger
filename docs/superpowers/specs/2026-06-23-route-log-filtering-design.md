# Route log filtering and X-Request-Id header

## Goal

Give consumers of `rocket-slogger` two new capabilities:

1. Control which routes produce automatic request/response log lines, via a
   denylist (filter out specific routes) and/or an allowlist (log only specific
   routes).
2. Optionally emit an `X-Request-Id` response header carrying the transaction
   id, opted into per logger.

Both must work without deferring the request log: the request line stays a live
account of what is happening, emitted at request receipt, and the request and
response lines for a given request are always filtered as a consistent pair.

## Background and constraints

These come from reading Rocket 0.5.1 source, not assumption.

- Request fairings run in `preprocess_request` (`server.rs:244`) **before**
  routing calls `request.set_route()` (`server.rs:332`). So during `on_request`
  the route is unknown: `request.route()` is always `None`. This is why the
  current "Request" log line carries only method and uri, never route/path/rank.
- The route is known only in `on_response`.
- `Route::matches()` is `pub(crate)`, so Rocket's own matcher cannot be reused
  from this crate. A minimal single-pattern matcher is required instead.
- `rocket::routes![a, b]` expands to `vec![a.into_route(), ...]`, yielding real
  `Route` values. Each `Route` exposes `method`, `name`, and
  `uri.unmounted_origin` (the pre-mount path, preserved after mounting). The
  tuple `(method, name, unmounted_origin)` is a stable, mount-independent,
  compile-checked route identity that needs no proc-macro and no knowledge of
  Rocket's generated symbols.

## Route identity

Developers list routes by handle, reusing Rocket's own macro:

```rust
Slogger::new_terminal_logger()
    .skip_reqres_logs(rocket::routes![health, metrics])  // denylist
    .show_reqres_logs(rocket::routes![api_v1, api_v2]);  // allowlist
```

`.skip_reqres_logs` and `.show_reqres_logs` each accept what `routes![]` produces. At call time we
extract a lightweight key from each `Route` and drop the heavy `Route` (which
owns a `Box<dyn Handler>`), keeping `Slogger: Clone` cheap:

```rust
struct RouteKey {
    method: Method,           // rocket::http::Method, Copy + PartialEq
    name: Option<String>,     // route/function name
    unmounted: String,        // uri.unmounted_origin, e.g. "/health"
}
```

Renaming or removing a handler is a compile error, because the handle no longer
resolves. There are no magic strings to typo.

## Lazy resolution of mounted patterns

The denylist/allowlist hold unmounted identities; matching at request time needs
the full mounted path (including the `.mount("/base", ...)` prefix). We resolve
this once, lazily, on the first request, using a `OnceLock` stored behind `Arc`
on `Slogger` so all clones share the resolved data:

```rust
resolved: Arc<OnceLock<ResolvedFilter>>
```

On the first request:

1. Read the live mounted route table from `request.rocket().routes()`.
2. For each mounted route, compute its `RouteKey` (same `(method, name,
   unmounted_origin)`).
3. If that key is present in the denylist, parse the route's full mounted path
   (`route.uri.path()`) into segments and record it as a deny pattern. Same for
   the allowlist.

```rust
struct ResolvedFilter {
    allow: Vec<(Method, Vec<Segment>)>,
    deny: Vec<(Method, Vec<Segment>)>,
}

enum Segment {
    Static(String), // must equal the request segment
    Dynamic,        // <name>: matches exactly one segment
    Trailing,       // <name..>: matches zero or more remaining segments
}
```

Correlating by key means the developer never repeats the mount base; we learn it
from the route table.

## Matching

A minimal single-pattern path matcher, roughly 30 lines. It does **not**
reimplement Rocket routing: no ranking, no collision detection, no format
negotiation, no query matching. It answers one question: does this concrete
request path match this one pattern?

- Read `request.uri().path()` (already parsed by Rocket) and split on `/`.
- Compare against the pattern segments:
  - `Static(s)` must equal the request segment.
  - `Dynamic` matches any single segment.
  - `Trailing` matches all remaining segments, including none, and ends the
    pattern.
- Without a trailing segment, segment counts must be equal.
- The method must also be equal.

Examples:

| pattern         | request        | result   |
|-----------------|----------------|----------|
| `/health`       | `/health`      | match    |
| `/health`       | `/healthz`     | no match |
| `/users/<id>`   | `/users/42`    | match    |
| `/files/<p..>`  | `/files/a/b`   | match    |
| `/files/<p..>`  | `/files`       | match    |

Query strings are ignored entirely.

## Decision

Computed once, at request time, with allow gating and deny subtracting:

```text
eligible = allow.is_empty() || allow matches (method, path)
log      = eligible && !(deny matches (method, path))
```

- Neither list set: log everything (current behavior, untouched).
- Only deny set: log all except denied.
- Only allow set: log only allowed.
- Both set: log allowed minus denied; deny wins on overlap.

The resulting boolean is stored in `request.local_cache()` via a newtype so
`on_response` reads the exact same decision:

```rust
struct LogDecision(bool);
```

`on_response` defaults to `LogDecision(true)` if absent (fail open to logging),
though `on_request` always runs first and sets it.

## Fairing changes

`on_request`:

1. Compute the decision (initializing the `OnceLock` if needed) and cache it.
2. If `false`, return immediately: no transaction attached, no callbacks run, no
   line emitted.
3. Otherwise proceed exactly as today.

`on_response`:

1. Read the cached `LogDecision`. If `false`, return immediately.
2. Otherwise proceed as today.
3. If the `X-Request-Id` header is enabled (see below), set it before emitting
   the response line.

Because both lines read one cached decision, a request either produces both
lines or neither.

## X-Request-Id header

Off by default. A logging library should not alter responses unless asked. A
builder method, gated to the `transactions` feature (the only configuration
where a transaction id exists), opts in:

```rust
Slogger::new_terminal_logger()
    .with_request_id_header();   // only compiles with the `transactions` feature
```

Implementation:

- Add a `bool` field to `Slogger`, e.g. `emit_request_id_header`, also gated to
  `transactions`.
- `with_request_id_header()` is `#[cfg(feature = "transactions")]`, so calling
  it without the feature is a compile error rather than a silent no-op.
- When enabled, `on_response` sets `X-Request-Id` to the transaction UUID, the
  same value as the `transaction` log field, so header and logs agree.
- Denied routes get neither a line nor a header, consistently.

## Scope and non-goals

- Filtering affects only the automatic Request/Response log lines. Loggers
  obtained through the `FromRequest` guard are unaffected.
- No new dependencies. Matching uses `std` only.
- No new feature flag for filtering: the `.skip_reqres_logs`/`.show_reqres_logs` methods are always
  available and zero-cost when the lists are empty. The header method is gated
  to the existing `transactions` feature.
- The matcher does not handle query or format constraints. Routes are identified
  by method and path pattern only.

## Known caveat: overlapping ranked routes

The decision is made by path pattern, not by the specific route that ultimately
serves the request. If a developer denies a dynamic route like `/<id>` and a
request to `/health` also matches that pattern but is actually served by a
separate, higher-ranked `/health` route, the request is still suppressed. For
the routes people typically filter (health, metrics, ping, all static paths)
there is no ambiguity. If it ever matters, the developer filters by listing the
specific static route. This is an accepted, well-defined corner, not a bug.

## File layout

- `src/filter.rs` (new): `RouteKey`, `Segment`, `ResolvedFilter`, `LogDecision`,
  the segment matcher, and the decision function.
- `src/lib.rs`: add `pub mod filter;` and the `skip_reqres_logs`/`show_reqres_logs`/
  `with_request_id_header` builder methods plus the new `Slogger` fields.
- `src/fairing.rs`: the `on_request`/`on_response` changes.

## Testing

- Unit tests for the segment matcher: static, dynamic, trailing, count
  mismatch, method mismatch, query ignored.
- Unit tests for the decision function across the allow/deny truth table.
- Integration tests with a small Rocket instance: denied route produces no
  lines, allowed-only logs only the allowlist, neither list logs all, and the
  `X-Request-Id` header appears only when opted in and matches the logged
  transaction id.
