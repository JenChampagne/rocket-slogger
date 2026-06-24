# Route Log Filtering and X-Request-Id Header Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let consumers allow/deny which routes produce automatic request/response logs, and optionally emit an `X-Request-Id` response header carrying the transaction id.

**Architecture:** A new `filter` module holds a mount-independent route identity (`RouteKey`), a tiny path-pattern matcher (`Segment`), and the allow/deny decision (`ResolvedFilter`). The `Slogger` fairing resolves listed routes to mounted path patterns once (lazily, via `OnceLock`), computes the log decision once per request at request time, caches it in `request.local_cache()`, and reuses it in `on_response`. The request line stays live; request and response lines are filtered as a consistent pair.

**Tech Stack:** Rust 2021, Rocket 0.5, slog 2.7, std only for the new logic (no new dependencies).

## Global Constraints

- No new dependencies. The filter logic uses `std` only.
- No new feature flag for filtering: `.deny`/`.allow` are always available, zero-cost when lists are empty.
- `with_request_id_header()` is gated to the existing `transactions` feature.
- No panics in library code: no `unwrap()`, `expect()`, `todo!()`, slice indexing that can go out of bounds. Use `Result`/`Option` and explicit bounds checks. In tests, prefer `.expect("...")` (message completes "I expect...") over `.unwrap()`.
- `match` statements over the `Segment` enum must enumerate all variants (no wildcard arm).
- No emdashes, no AI-isms, ASCII only, in code and docs.
- Run `cargo fmt` and `cargo clippy` before considering any task done; treat clippy warnings as errors.
- Conventional commit messages, ending with a period. End commit bodies with the Co-Authored-By trailer:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

---

### Task 1: Filter module (identity, matcher, decision)

**Files:**
- Create: `src/filter.rs`

**Interfaces:**
- Consumes: `rocket::Route`, `rocket::http::Method`.
- Produces:
  - `pub struct RouteKey { pub method: Method, pub name: Option<String>, pub unmounted: String }` with `pub fn from_route(route: &Route) -> RouteKey`.
  - `pub enum Segment { Static(String), Dynamic, Trailing }` with `pub fn parse_path(path: &str) -> Vec<Segment>`.
  - `pub fn path_matches(pattern: &[Segment], path: &str) -> bool`.
  - `pub struct ResolvedFilter { pub allow: Vec<(Method, Vec<Segment>)>, pub deny: Vec<(Method, Vec<Segment>)> }` with `pub fn resolve(routes: &[&Route], allow_keys: &[RouteKey], deny_keys: &[RouteKey]) -> ResolvedFilter` and `pub fn should_log(&self, method: Method, path: &str) -> bool`.
  - `#[derive(Clone, Copy)] pub struct LogDecision(pub bool);`

- [ ] **Step 1: Write the module with failing tests**

Create `src/filter.rs`:

```rust
use rocket::http::Method;
use rocket::Route;

/// A mount-independent identity for a route, derived from a `Route` produced by
/// `rocket::routes![...]`. Method, name, and the unmounted path are all preserved
/// when a route is mounted at a base, so the same key is computed before and
/// after mounting. This is what lets us correlate a listed route to its live,
/// mounted entry without the developer repeating the mount base.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteKey {
    pub method: Method,
    pub name: Option<String>,
    pub unmounted: String,
}

impl RouteKey {
    pub fn from_route(route: &Route) -> Self {
        Self {
            method: route.method,
            name: route.name.as_ref().map(|name| name.to_string()),
            unmounted: route.uri.unmounted_origin.path().as_str().to_string(),
        }
    }
}

/// One segment of a route path pattern.
#[derive(Clone, Debug, PartialEq)]
pub enum Segment {
    /// A literal segment that must equal the request segment.
    Static(String),
    /// `<name>`: matches exactly one request segment.
    Dynamic,
    /// `<name..>`: matches zero or more remaining request segments. Ends a pattern.
    Trailing,
}

impl Segment {
    /// Parse a route path like `/users/<id>` or `/files/<rest..>` into segments.
    /// Empty segments (leading/trailing/double slashes) are dropped.
    pub fn parse_path(path: &str) -> Vec<Segment> {
        path.split('/')
            .filter(|segment| !segment.is_empty())
            .map(|segment| {
                if segment.starts_with('<') && segment.ends_with("..>") {
                    Segment::Trailing
                } else if segment.starts_with('<') && segment.ends_with('>') {
                    Segment::Dynamic
                } else {
                    Segment::Static(segment.to_string())
                }
            })
            .collect()
    }
}

/// Does the concrete request `path` match this single `pattern`? Query strings
/// are ignored. This is a single-pattern matcher only: no ranking, no collision
/// detection, no format negotiation. It is not a reimplementation of Rocket's
/// router.
pub fn path_matches(pattern: &[Segment], path: &str) -> bool {
    let request: Vec<&str> = path.split('/').filter(|segment| !segment.is_empty()).collect();

    let mut index = 0;
    for segment in pattern {
        match segment {
            Segment::Trailing => return true,
            Segment::Dynamic => {
                if index >= request.len() {
                    return false;
                }
                index += 1;
            }
            Segment::Static(expected) => {
                if index >= request.len() || request[index] != expected.as_str() {
                    return false;
                }
                index += 1;
            }
        }
    }

    index == request.len()
}

/// The allow/deny lists resolved to concrete mounted path patterns. Built once
/// from the live route table.
pub struct ResolvedFilter {
    pub allow: Vec<(Method, Vec<Segment>)>,
    pub deny: Vec<(Method, Vec<Segment>)>,
}

impl ResolvedFilter {
    pub fn resolve(routes: &[&Route], allow_keys: &[RouteKey], deny_keys: &[RouteKey]) -> Self {
        fn collect(routes: &[&Route], keys: &[RouteKey]) -> Vec<(Method, Vec<Segment>)> {
            routes
                .iter()
                .filter(|route| {
                    let key = RouteKey::from_route(route);
                    keys.iter().any(|listed| *listed == key)
                })
                .map(|route| (route.method, Segment::parse_path(route.uri.path())))
                .collect()
        }

        Self {
            allow: collect(routes, allow_keys),
            deny: collect(routes, deny_keys),
        }
    }

    /// Allow gates, deny subtracts. Empty allow means "everything is eligible".
    pub fn should_log(&self, method: Method, path: &str) -> bool {
        let matches_in = |set: &[(Method, Vec<Segment>)]| {
            set.iter()
                .any(|(route_method, pattern)| *route_method == method && path_matches(pattern, path))
        };

        let eligible = self.allow.is_empty() || matches_in(&self.allow);
        eligible && !matches_in(&self.deny)
    }
}

/// Cached per-request log decision, stored in `request.local_cache()` so that
/// `on_request` and `on_response` always agree.
#[derive(Clone, Copy)]
pub struct LogDecision(pub bool);

#[cfg(test)]
mod tests {
    use super::{path_matches, ResolvedFilter, Segment};
    use rocket::http::Method;

    fn pat(path: &str) -> Vec<Segment> {
        Segment::parse_path(path)
    }

    #[test]
    fn test_parse_path_classifies_segments() {
        assert_eq!(
            pat("/users/<id>/files/<rest..>"),
            vec![
                Segment::Static("users".into()),
                Segment::Dynamic,
                Segment::Static("files".into()),
                Segment::Trailing,
            ],
            "I expect static, dynamic, static, trailing segments"
        );
    }

    #[test]
    fn test_static_path_exact_match() {
        assert!(path_matches(&pat("/health"), "/health"), "I expect /health to match itself");
        assert!(!path_matches(&pat("/health"), "/healthz"), "I expect /healthz not to match /health");
        assert!(!path_matches(&pat("/health"), "/health/x"), "I expect a longer path not to match");
    }

    #[test]
    fn test_dynamic_segment_matches_one() {
        assert!(path_matches(&pat("/users/<id>"), "/users/42"), "I expect a dynamic segment to match one value");
        assert!(!path_matches(&pat("/users/<id>"), "/users"), "I expect a missing dynamic segment not to match");
        assert!(!path_matches(&pat("/users/<id>"), "/users/42/extra"), "I expect an extra segment not to match");
    }

    #[test]
    fn test_trailing_matches_rest_including_none() {
        assert!(path_matches(&pat("/files/<rest..>"), "/files"), "I expect trailing to match zero segments");
        assert!(path_matches(&pat("/files/<rest..>"), "/files/a"), "I expect trailing to match one segment");
        assert!(path_matches(&pat("/files/<rest..>"), "/files/a/b/c"), "I expect trailing to match many segments");
    }

    #[test]
    fn test_root_path_matches() {
        assert!(path_matches(&pat("/"), "/"), "I expect root to match root");
        assert!(!path_matches(&pat("/"), "/x"), "I expect root not to match a child");
    }

    #[test]
    fn test_decision_truth_table() {
        let only_deny = ResolvedFilter { allow: vec![], deny: vec![(Method::Get, pat("/health"))] };
        assert!(only_deny.should_log(Method::Get, "/keep"), "I expect a non-denied route to log");
        assert!(!only_deny.should_log(Method::Get, "/health"), "I expect a denied route not to log");
        assert!(only_deny.should_log(Method::Post, "/health"), "I expect a different method to log");

        let only_allow = ResolvedFilter { allow: vec![(Method::Get, pat("/api"))], deny: vec![] };
        assert!(only_allow.should_log(Method::Get, "/api"), "I expect an allowed route to log");
        assert!(!only_allow.should_log(Method::Get, "/other"), "I expect a non-allowed route not to log");

        let both = ResolvedFilter {
            allow: vec![(Method::Get, pat("/api")), (Method::Get, pat("/admin"))],
            deny: vec![(Method::Get, pat("/admin"))],
        };
        assert!(both.should_log(Method::Get, "/api"), "I expect allowed-and-not-denied to log");
        assert!(!both.should_log(Method::Get, "/admin"), "I expect deny to win over allow");

        let neither = ResolvedFilter { allow: vec![], deny: vec![] };
        assert!(neither.should_log(Method::Get, "/anything"), "I expect everything to log when no lists are set");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail to compile (module not yet wired)**

Run: `cargo test --lib filter 2>&1 | head -20`
Expected: compile error, `filter` module is not declared in `lib.rs` yet. This is expected; the module becomes reachable in Task 2. To verify the file itself is sound in isolation, instead run Step 3.

- [ ] **Step 3: Temporarily declare the module to run its tests**

Add to the top of `src/lib.rs` (this line is also required permanently by Task 2, so leave it):

```rust
pub mod filter;
```

Run: `cargo test --lib filter`
Expected: PASS, 6 tests in `filter::tests`.

- [ ] **Step 4: Format, lint, commit**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: no warnings.

```bash
git add src/filter.rs src/lib.rs
git commit -m "feat: Add route filter module with path matcher and decision logic.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Wire filtering and header opt-in into Slogger

**Files:**
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `filter::{RouteKey, ResolvedFilter, LogDecision}` from Task 1.
- Produces (on `Slogger`):
  - `pub fn deny(self, routes: Vec<Route>) -> Self`
  - `pub fn allow(self, routes: Vec<Route>) -> Self`
  - `#[cfg(feature = "transactions")] pub fn with_request_id_header(self) -> Self`
  - `pub(crate) fn filter_decision(&self, request: &Request<'_>) -> bool`
  - new fields: `filter_allow: Vec<RouteKey>`, `filter_deny: Vec<RouteKey>`, `resolved: Arc<OnceLock<ResolvedFilter>>`, and `#[cfg(feature = "transactions")] emit_request_id_header: bool`.

- [ ] **Step 1: Add imports and new struct fields**

In `src/lib.rs`, `pub mod filter;` is already present from Task 1. Add the `Route` import and `OnceLock`. The existing imports are:

```rust
use rocket::{Request, Response};
use std::sync::Arc;
```

Change to:

```rust
use rocket::{Request, Response, Route};
use std::sync::{Arc, OnceLock};

use crate::filter::{LogDecision, ResolvedFilter, RouteKey};
```

Add fields to the `Slogger` struct, after `logger: Arc<Logger>,`:

```rust
    filter_allow: Vec<RouteKey>,
    filter_deny: Vec<RouteKey>,
    resolved: Arc<OnceLock<ResolvedFilter>>,

    #[cfg(feature = "transactions")]
    emit_request_id_header: bool,
```

- [ ] **Step 2: Initialize the new fields in `from_logger`**

In `from_logger`, the existing body is:

```rust
    pub fn from_logger(logger: Logger) -> Self {
        Self {
            logger: Arc::new(logger),

            #[cfg(feature = "callbacks")]
            request_handlers: vec![],

            #[cfg(feature = "callbacks")]
            response_handlers: vec![],
        }
    }
```

Add the new field initializers:

```rust
    pub fn from_logger(logger: Logger) -> Self {
        Self {
            logger: Arc::new(logger),

            filter_allow: vec![],
            filter_deny: vec![],
            resolved: Arc::new(OnceLock::new()),

            #[cfg(feature = "transactions")]
            emit_request_id_header: false,

            #[cfg(feature = "callbacks")]
            request_handlers: vec![],

            #[cfg(feature = "callbacks")]
            response_handlers: vec![],
        }
    }
```

- [ ] **Step 3: Add builder methods and the decision helper**

Add these methods inside `impl Slogger` (for example after `get_for_response`):

```rust
    /// Exclude the given routes from automatic request/response logs. Pass the
    /// value produced by `rocket::routes![...]`. Combine with `allow`: deny wins
    /// on overlap.
    pub fn deny(mut self, routes: Vec<Route>) -> Self {
        self.filter_deny
            .extend(routes.iter().map(RouteKey::from_route));
        self
    }

    /// Log only the given routes automatically. Pass the value produced by
    /// `rocket::routes![...]`. When an allowlist is set, routes not in it are not
    /// logged. An empty allowlist (the default) means every route is eligible.
    pub fn allow(mut self, routes: Vec<Route>) -> Self {
        self.filter_allow
            .extend(routes.iter().map(RouteKey::from_route));
        self
    }

    /// Set an `X-Request-Id` response header to the request's transaction id.
    /// Off by default: a logging fairing should not alter responses unless asked.
    #[cfg(feature = "transactions")]
    pub fn with_request_id_header(mut self) -> Self {
        self.emit_request_id_header = true;
        self
    }

    /// Decide whether this request should be logged. Resolves the listed routes
    /// to mounted path patterns on first use, then matches by method and path.
    pub(crate) fn filter_decision(&self, request: &Request<'_>) -> bool {
        let resolved = self.resolved.get_or_init(|| {
            let routes: Vec<&Route> = request.rocket().routes().collect();
            ResolvedFilter::resolve(&routes, &self.filter_allow, &self.filter_deny)
        });

        resolved.should_log(request.method(), request.uri().path().as_str())
    }
```

- [ ] **Step 4: Add a unit test for the builders at the bottom of `src/lib.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::Slogger;
    use rocket::{get, routes};

    #[get("/skip")]
    fn skip() -> &'static str {
        "skip"
    }

    #[get("/keep")]
    fn keep() -> &'static str {
        "keep"
    }

    fn silent_slogger() -> Slogger {
        let logger = super::Logger::root(slog::Discard, super::o!());
        Slogger::from_logger(logger)
    }

    #[test]
    fn test_deny_stores_one_key() {
        let slogger = silent_slogger().deny(routes![skip]);
        assert_eq!(slogger.filter_deny.len(), 1, "I expect one denied route key");
        assert_eq!(slogger.filter_allow.len(), 0, "I expect no allow keys");
    }

    #[test]
    fn test_allow_accumulates_keys() {
        let slogger = silent_slogger().allow(routes![skip]).allow(routes![keep]);
        assert_eq!(slogger.filter_allow.len(), 2, "I expect two accumulated allow keys");
    }
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib`
Expected: PASS, including `tests::test_deny_stores_one_key`, `tests::test_allow_accumulates_keys`, and the Task 1 filter tests.

- [ ] **Step 6: Verify the header builder is gated correctly**

Run: `cargo test --lib --features transactions`
Expected: PASS. (The `with_request_id_header` method compiles under the feature.)

- [ ] **Step 7: Format, lint, commit**

Run: `cargo fmt && cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings.

```bash
git add src/lib.rs
git commit -m "feat: Add deny/allow route filtering and request-id header opt-in to Slogger.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Apply filtering in the fairing

**Files:**
- Modify: `src/fairing.rs`
- Test: `tests/filtering.rs` (create)

**Interfaces:**
- Consumes: `Slogger::filter_decision` and `filter::LogDecision` from Task 2.
- Produces: filtered `on_request`/`on_response` behavior. No new public API.

- [ ] **Step 1: Write the failing integration test**

Create `tests/filtering.rs`:

```rust
use rocket::local::blocking::Client;
use rocket::{get, routes};
use rocket_slogger::{o, Logger, Slogger};
use std::sync::{Arc, Mutex};

/// A slog drain that records each log message's text, so a test can assert which
/// automatic log lines were emitted.
#[derive(Clone)]
struct CaptureDrain {
    messages: Arc<Mutex<Vec<String>>>,
}

impl slog::Drain for CaptureDrain {
    type Ok = ();
    type Err = slog::Never;

    fn log(
        &self,
        record: &slog::Record,
        _values: &slog::OwnedKVList,
    ) -> Result<Self::Ok, Self::Err> {
        if let Ok(mut messages) = self.messages.lock() {
            messages.push(record.msg().to_string());
        }
        Ok(())
    }
}

#[get("/keep")]
fn keep() -> &'static str {
    "keep"
}

#[get("/skip")]
fn skip() -> &'static str {
    "skip"
}

fn count(messages: &Arc<Mutex<Vec<String>>>, needle: &str) -> usize {
    messages
        .lock()
        .expect("I expect to lock the captured messages")
        .iter()
        .filter(|message| message.as_str() == needle)
        .count()
}

fn client_with(slogger_builder: impl FnOnce(Slogger) -> Slogger) -> (Client, Arc<Mutex<Vec<String>>>) {
    let messages = Arc::new(Mutex::new(Vec::new()));
    let drain = CaptureDrain { messages: messages.clone() };
    let logger = Logger::root(drain, o!());
    let slogger = slogger_builder(Slogger::from_logger(logger));

    let rocket = rocket::build()
        .attach(slogger)
        .mount("/", routes![keep, skip]);
    let client = Client::tracked(rocket).expect("I expect a valid Rocket instance");
    (client, messages)
}

#[test]
fn test_denied_route_produces_no_lines() {
    let (client, messages) = client_with(|slogger| slogger.deny(routes![skip]));

    client.get("/skip").dispatch();
    assert_eq!(count(&messages, "Request"), 0, "I expect no Request line for a denied route");
    assert_eq!(count(&messages, "Response"), 0, "I expect no Response line for a denied route");

    client.get("/keep").dispatch();
    assert_eq!(count(&messages, "Request"), 1, "I expect a Request line for a non-denied route");
    assert_eq!(count(&messages, "Response"), 1, "I expect a Response line for a non-denied route");
}

#[test]
fn test_allowlist_logs_only_allowed() {
    let (client, messages) = client_with(|slogger| slogger.allow(routes![keep]));

    client.get("/skip").dispatch();
    assert_eq!(count(&messages, "Request"), 0, "I expect no Request line for a route outside the allowlist");

    client.get("/keep").dispatch();
    assert_eq!(count(&messages, "Request"), 1, "I expect a Request line for an allowed route");
    assert_eq!(count(&messages, "Response"), 1, "I expect a Response line for an allowed route");
}

#[test]
fn test_no_lists_logs_everything() {
    let (client, messages) = client_with(|slogger| slogger);

    client.get("/skip").dispatch();
    client.get("/keep").dispatch();
    assert_eq!(count(&messages, "Request"), 2, "I expect both routes to log a Request line");
    assert_eq!(count(&messages, "Response"), 2, "I expect both routes to log a Response line");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test filtering`
Expected: FAIL. `test_denied_route_produces_no_lines` fails because denied routes still log (filtering not applied in the fairing yet).

- [ ] **Step 3: Apply the decision in `on_request`**

In `src/fairing.rs`, add the import near the top (after the existing `use crate::{info, Slogger};`):

```rust
use crate::filter::LogDecision;
```

Replace the existing `on_request`:

```rust
    async fn on_request(&self, request: &mut Request<'_>, _: &mut Data<'_>) {
        #[allow(unused_mut)]
        let mut logger = Arc::new(self.get_for_request(request));

        #[cfg(feature = "callbacks")]
        for handler in &self.request_handlers {
            if let Some(new_logger) = handler(logger.clone(), request).await {
                logger = new_logger;
            }
        }

        info!(logger, "Request");
    }
```

with:

```rust
    async fn on_request(&self, request: &mut Request<'_>, _: &mut Data<'_>) {
        let should_log = self.filter_decision(request);
        request.local_cache(|| LogDecision(should_log));
        if !should_log {
            return;
        }

        #[allow(unused_mut)]
        let mut logger = Arc::new(self.get_for_request(request));

        #[cfg(feature = "callbacks")]
        for handler in &self.request_handlers {
            if let Some(new_logger) = handler(logger.clone(), request).await {
                logger = new_logger;
            }
        }

        info!(logger, "Request");
    }
```

- [ ] **Step 4: Apply the cached decision in `on_response`**

Replace the start of the existing `on_response`:

```rust
    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        #[allow(unused_mut)]
        let mut logger = Arc::new(self.get_for_response(request, response));
```

with an early return that reads the cached decision:

```rust
    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        let should_log = request.local_cache(|| LogDecision(true)).0;
        if !should_log {
            return;
        }

        #[allow(unused_mut)]
        let mut logger = Arc::new(self.get_for_response(request, response));
```

Leave the rest of `on_response` unchanged for this task. (The `X-Request-Id` header is added in Task 4.)

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --test filtering`
Expected: PASS, 3 tests.

- [ ] **Step 6: Run the full default test suite**

Run: `cargo test`
Expected: PASS (lib tests plus the filtering integration tests).

- [ ] **Step 7: Format, lint, commit**

Run: `cargo fmt && cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings.

```bash
git add src/fairing.rs tests/filtering.rs
git commit -m "feat: Filter automatic request/response logs by allow/deny lists.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Emit the X-Request-Id response header

**Files:**
- Modify: `src/fairing.rs`
- Test: `tests/request_id_header.rs` (create)

**Interfaces:**
- Consumes: `Slogger::emit_request_id_header` and `with_request_id_header` from Task 2, `transaction::RequestTransaction` (existing).
- Produces: an `X-Request-Id` response header on logged responses when opted in, under the `transactions` feature.

- [ ] **Step 1: Write the failing test**

Create `tests/request_id_header.rs`:

```rust
#![cfg(feature = "transactions")]

use rocket::local::blocking::Client;
use rocket::{get, routes};
use rocket_slogger::{o, Logger, Slogger};
use uuid::Uuid;

#[get("/keep")]
fn keep() -> &'static str {
    "keep"
}

#[get("/skip")]
fn skip() -> &'static str {
    "skip"
}

fn client_with(slogger: Slogger) -> Client {
    let rocket = rocket::build()
        .attach(slogger)
        .mount("/", routes![keep, skip]);
    Client::tracked(rocket).expect("I expect a valid Rocket instance")
}

fn silent_slogger() -> Slogger {
    let logger = Logger::root(slog::Discard, o!());
    Slogger::from_logger(logger)
}

#[test]
fn test_header_present_and_valid_uuid_when_opted_in() {
    let client = client_with(silent_slogger().with_request_id_header());

    let response = client.get("/keep").dispatch();
    let header = response
        .headers()
        .get_one("X-Request-Id")
        .expect("I expect an X-Request-Id header on a logged response");
    Uuid::parse_str(header).expect("I expect the X-Request-Id header to be a valid UUID");
}

#[test]
fn test_header_absent_when_not_opted_in() {
    let client = client_with(silent_slogger());

    let response = client.get("/keep").dispatch();
    assert!(
        response.headers().get_one("X-Request-Id").is_none(),
        "I expect no X-Request-Id header when the option is off"
    );
}

#[test]
fn test_header_absent_on_denied_route() {
    let client = client_with(silent_slogger().with_request_id_header().deny(routes![skip]));

    let response = client.get("/skip").dispatch();
    assert!(
        response.headers().get_one("X-Request-Id").is_none(),
        "I expect no X-Request-Id header on a denied (unlogged) route"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test request_id_header --features transactions`
Expected: FAIL. `test_header_present_and_valid_uuid_when_opted_in` fails because the header is not set yet.

- [ ] **Step 3: Set the header in `on_response`**

In `src/fairing.rs`, inside `on_response`, after the early-return decision check and before the final `info!(logger, "Response"; ...)`, add the header emission. Insert this block (it sits after the callbacks loop and the `body_size` computation, immediately before the final `info!`):

```rust
        #[cfg(feature = "transactions")]
        if self.emit_request_id_header {
            let transaction = crate::transaction::RequestTransaction::new().attach_on(request);
            response.set_header(rocket::http::Header::new(
                "X-Request-Id",
                transaction.id_as_string(),
            ));
        }
```

Note: `attach_on` returns the transaction already cached for this request (created during `on_request`'s `get_for_request`), so the header value matches the `transaction` field in the logs.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --test request_id_header --features transactions`
Expected: PASS, 3 tests.

- [ ] **Step 5: Confirm the default build is unaffected**

Run: `cargo test`
Expected: PASS. The `request_id_header` test file compiles to nothing without the feature (`#![cfg(feature = "transactions")]`).

- [ ] **Step 6: Format, lint, commit**

Run: `cargo fmt && cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings.

```bash
git add src/fairing.rs tests/request_id_header.rs
git commit -m "feat: Add opt-in X-Request-Id response header from the transaction id.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Update example, README, and Cargo.toml

**Files:**
- Create: `examples/filtering.rs`
- Modify: `Cargo.toml`
- Modify: `README.md`

**Interfaces:**
- Consumes: the public API from Tasks 2 and 4. No new code interfaces.

- [ ] **Step 1: Add the example crate entry to `Cargo.toml`**

After the existing `[[example]]` blocks, add:

```toml
[[example]]
name = "filtering"
path = "examples/filtering.rs"
```

- [ ] **Step 2: Create `examples/filtering.rs`**

This reuses the shared `examples/routes/mod.rs` handlers. It denies `always_fail` and `dynamic_path` from automatic logging, and (only when built with `--features transactions`) opts into the `X-Request-Id` header.

```rust
mod routes;

use rocket::config::Config;
use rocket::log::LogLevel;
use rocket::{catchers, routes, Build, Rocket};
use rocket_slogger::{o, Drain, Logger, Slogger};
use routes::{always_fail, always_greet, always_thank, dynamic_path, not_found};

use slog_term::{FullFormat, PlainSyncDecorator};

#[rocket::launch]
async fn rocket() -> Rocket<Build> {
    let plain = PlainSyncDecorator::new(std::io::stdout());
    let logger = Logger::root(FullFormat::new(plain).build().fuse(), o!());

    // Exclude the failing route and the dynamic catch-all from automatic logs.
    // Swap `.deny(...)` for `.allow(...)` to log only the listed routes instead.
    #[allow(unused_mut)]
    let mut fairing = Slogger::from_logger(logger).deny(routes![always_fail, dynamic_path]);

    // Only available with the `transactions` feature, which provides the id.
    #[cfg(feature = "transactions")]
    {
        fairing = fairing.with_request_id_header();
    }

    // Turn off Rocket logging, not rocket-slogger logging.
    let mut config = Config::from(Config::figment());
    config.log_level = LogLevel::Off;

    rocket::custom(config)
        .attach(fairing)
        .mount(
            "/",
            routes![always_greet, always_thank, always_fail, dynamic_path],
        )
        .register("/", catchers![not_found])
}
```

- [ ] **Step 3: Verify the example builds (both feature configurations)**

Run: `cargo build --example filtering`
Expected: success.

Run: `cargo build --example filtering --features transactions`
Expected: success.

- [ ] **Step 4: Update `README.md`**

Add a new subsection under `### Examples` (after the `bunyan-callbacks-features` run note), describing the filtering controls. Insert:

```markdown
### Filtering which routes are logged

By default every request and response is logged. To exclude noisy routes (health
checks, metrics) or to log only a chosen set, pass route handles to `deny` or
`allow`. Both take the value produced by `rocket::routes![...]`, so renaming or
removing a handler is a compile error rather than a silent typo:

\```rs
let fairing = Slogger::from_logger(logger)
    // exclude these routes from automatic logs
    .deny(rocket::routes![health, metrics])
    // and/or log only these routes
    .allow(rocket::routes![api_v1, api_v2]);
\```

When an allowlist is set, only those routes are eligible; the denylist then
removes from the eligible set, so deny wins on overlap. With neither list set,
everything is logged. Matching is by HTTP method and the route's path pattern
(including dynamic `<segment>` and trailing `<segment..>` parts); query strings
are ignored. Filtering applies only to the automatic request/response logs, not
to loggers obtained through the request guard.

See `cargo run --example filtering` for a working example.
```

(Replace the `\`` sequences with real triple backticks when writing the file.)

- [ ] **Step 5: Document the X-Request-Id header in the transactions section of `README.md`**

In `### When the `transactions` feature is enabled`, append a paragraph after the existing list describing the response section:

```markdown
Optionally, calling `.with_request_id_header()` on the fairing sets an
`X-Request-Id` response header to the same transaction UUID that appears in the
logs. It is off by default, since a logging fairing should not alter responses
unless asked, and is only available when the `transactions` feature is enabled.
```

- [ ] **Step 6: Confirm docs and examples are consistent**

Run: `cargo build --examples`
Expected: success for all examples.

Run: `cargo test --doc`
Expected: PASS (no doctests are added, so this is a no-op success; run it to confirm nothing regressed).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml examples/filtering.rs README.md
git commit -m "docs: Document and demonstrate route log filtering and request-id header.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification

After all tasks, run the full matrix to confirm nothing regressed:

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test` (default features)
- [ ] `cargo test --features transactions` (filtering + header tests)
- [ ] `cargo test --all-features`
- [ ] `cargo build --examples` and `cargo build --examples --features transactions,callbacks,bunyan,terminal`
