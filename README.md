# Rocket Slogger

[Structured logging](https://github.com/slog-rs/slog) middleware for the
[Rocket](https://rocket.rs) web framework.

When this fairing (middleware) is attached to an instance of Rocket, detailed
log messages will automatically be generated for every request received and
every response sent. The logger can also be injected into individual routes
to generate additional custom logs at time of request.

On start-up, all configurations are shown as an initial log message.
This both lists out the current configuration, and can serve as a signal
that the web server has been started/restarted. Next is a log message
detailing the routes available, then one of error status catchers,
then one of the host and port the server is listening on.

## Setup

Rust toolchain required. See [https://rustup.rs/](https://rustup.rs/) for
installation instructions.

Add this crate to your Rust project:

```toml
rocket-slogger = "1.1"
```

### Quick Start

Instantiate the fairing (middleware) with a
[`slog`](https://github.com/slog-rs/slog)-compatible `Logger`,
then add it to your Rocket server:

```rs
// Wrap your `slog`-compatible Logger with the fairing
let fairing = Slogger::from_logger(logger);

// Load config from the usual places, such as Rocket.toml and the environment
let mut config = Config::from(Config::figment());

// The fairing does not turn off Rocket's pretty print logs by default
config.log_level = LogLevel::Off;

rocket::custom(config)
    .attach(fairing)
    ...
```

### When the `envlogger` feature is enabled

Adds support for `RUST_LOG` environment variable handling to control log levels
output. See the `slog-envlogger` crate documentation for more details.

```sh
RUST_LOG=trace cargo run ...
RUST_LOG=debug cargo run ...
RUST_LOG=info cargo run ...
RUST_LOG=warn cargo run ...
RUST_LOG=error cargo run ...
```

By default when enabled, only warning and error levels are displayed.

### When the `terminal` feature is enabled

The helper function `Slogger::new_terminal_logger()` will setup the logger
to output plain text for each log message that looks like the following:

```
Mar 15 04:32:00.815 INFO Request, method: GET, path: /, content-type: None, user-agent: vscode-restclient

Mar 15 04:32:00.815 INFO Response, size: 11, method: GET, path: /, route: always_greet, rank: -9, code: 200, reason: OK, content-type: text/plain; charset=utf-8
```

### When the `bunyan` feature is enabled

The helper function `Slogger::new_bunyan_logger()` will setup the logger
to output [bunyan-style](https://github.com/slog-rs/bunyan) JSON objects
for each log message that looks like the following:

```
{"msg":"Request","v":0,"name":"My App","level":30,"time":"2023-03-15T04:29:35.865466064Z","hostname":"my-computer","pid":810142,"method":"GET","path":"/","content-type":null,"user-agent":"vscode-restclient"}

{"msg":"Response","v":0,"name":"My App","level":30,"time":"2023-03-15T04:29:35.867971878Z","hostname":"my-computer","pid":810142,"method":"GET","path":"/","route":"always_greet","rank":-9,"code":200,"reason":"OK","content-type":"text/plain; charset=utf-8","size":11}
```

Otherwise the `Slogger` fairing can be built with any
[`slog`](https://github.com/slog-rs/slog)-compatible `Logger`
with `Slogger::from_logger(logger)`.

### Examples

There are minimal implementations of a Rocket web server with this logging
middleware attached in various configurations inside the `./examples` folder.

Keep in mind that some of the examples require features to be enabled.

For example, the command to run the `bunyan-callbacks-features` is
`cargo run --example bunyan-callbacks-features --features bunyan,callbacks`.

### Filtering which routes are logged

By default every request and response is logged. To exclude noisy routes
(health checks, metrics) or to log only a chosen set, pass route handles to
`skip_reqres_logs` or `show_reqres_logs`. Both take the value produced by
`rocket::routes![...]`, so renaming or removing a handler is a compile error
rather than a silent typo:

```rs
let fairing = Slogger::from_logger(logger)
    // skip the request/response logs for these routes
    .skip_reqres_logs(rocket::routes![health, metrics])
    // and/or show those logs only for these routes
    .show_reqres_logs(rocket::routes![api_v1, api_v2]);
```

When `show_reqres_logs` is set, only those routes are eligible;
`skip_reqres_logs` then removes from the eligible set, so a skipped route wins
on overlap. With neither set, everything is logged. Matching is by HTTP method
and the route's path pattern (including dynamic `<segment>` and trailing
`<segment..>` parts); query strings are ignored. Filtering applies only to
the automatic request/response logs, not to loggers obtained through
the request guard.

When filtering is configured, a `Request/Response Log Filtering Active` line is
logged at launch with the show/skip counts, so you can confirm it is wired up.

Note: the lists are resolved against the live route table. A route handle passed
to `show_reqres_logs` that is never actually mounted resolves to nothing, which
leaves the show list empty and therefore logs every route. Make sure any handle
you pass to these methods is also mounted.

See `cargo run --example filtering` for a working example.

### Enriching the response log with request-scoped fields

Any code path in a request can attach fields to the automatic `Response` log
line through a request-scoped `ResponseLog`. Unrelated layers (an auth guard, a
handler) write into the same per-request bag without coordinating, and the
fairing merges everything onto the one `Response` line at the end. This needs no
feature flag.

Handlers take it as a request guard. Code that only has a `&Request`, like a
`FromRequest` impl or an auth guard, reaches the same bag through the
`SloggerExt` trait:

```rs
use rocket_slogger::{ResponseLog, SloggerExt};

#[get("/users/<id>")]
fn show_user(id: u64, log: ResponseLog) -> &'static str {
    log.set("user_id", id);               // typed: remains as a number in JSON
    log.set_some("tenant", None::<&str>); // skipped entirely when None
    "ok"
}

// elsewhere, with only a &Request in hand:
fn authenticate(request: &rocket::Request<'_>) {
    request.response_log().set("role", "admin");
}
```

`set` records a value, replacing any prior value for the same key. `set_some`
records only when the value is `Some`, so an absent value leaves the field off
the line rather than emitting a null. Values keep their type (numbers and
booleans stay numbers and booleans in the output) instead of being stringified.

Fields land on the `Response` line only. The `Request` line is emitted before
routing, so it is already written by the time a handler or guard runs.

`SloggerExt` also provides `request.logger()`, which always returns the
request-enriched logger. Unlike the `Slogger` request guard it cannot miss: the
fairing is managed from startup, so the only way it is absent is forgetting to
attach the fairing.

## Details

For each request received, a log message is generated containing the following:
- HTTP Method (e.g. get, post, put, etc)
- URL Path (e.g. /path/to/route?query=string)
- Content-Type Header of Request
- User Agent

For each response sent, a log message is generated containing the following:
- HTTP Method
- URL Path
- Content-Type Header of Response
- Status Code and Reason
- Response Body Size

### When the `transactions` feature is enabled

For each request received, in addition to the above, the following information
will also be generated:
- Exact UTC date and time with time zone of when the request was received.
- A unique UUID that will be the same for all logs generated by a single request
  for correlating logs.

For each response sent, in addition to the above, the following information
will also be generated:
- The same exact time of when the middleware initially received the request.
- The same unique UUID that correlates the response log to the request log.
- The total elapsed time from when the middleware received the request to
  when it received the response in nanoseconds.

### When the `transaction_header` feature is enabled

Calling `.with_request_id_header()` on the fairing sets an `X-Request-Id`
response header to the same transaction UUID that appears in the logs. It is off
by default, since a logging fairing should not alter responses unless asked.

This lives behind its own feature so that enabling `transactions` for logging
never compiles in any response-mutating code. The header reuses the transaction
id, so `transaction_header` requires `transactions`.

### When the `local_time` feature is enabled

The exact date and time with time zone of when the middleware received the
request is shown in the systems local time zone.

Note that the `time` field of when the log was made remains in the UTC time zone.

### When the `callbacks` feature is enabled

Functions can be attached to the fairing either on request or on response.

These callback functions get access to the `slog::Logger` containing all of
the above fields, as well as a reference to the response and/or request.
This enables the callback functions to return the same or a new `slog::Logger`
instance with any new properties added before the log message is generated.

```rs
    Slogger::new_bunyan_logger(env!("CARGO_PKG_NAME"))
        .on_request(|logger, _request| {
            // currently requires a pinned box to have an async context
            Box::pin(async move {
                // here any async function calls or server state can be fetched
                // so that it can be added to the logger that will form the response log
                let new_logger = logger.new(rocket_slogger::log_fields!(
                    "field:from-closure" => "some dynamic data derived at request time",
                    "in:request" => "more dynamic metrics",
                ));

                // the new logger must be returned in an Option<Arc<Logger>>
                Some(Arc::new(new_logger))
            })
        })
        .on_response(|logger, _request, _response| {
            // currently requires a pinned box to have an async context
            Box::pin(async move {
                // here any async function calls or server state can be fetched
                // so that it can be added to the logger that will form the response log
                let new_logger = logger.new(rocket_slogger::log_fields!(
                    "field:from-closure" => "some dynamic data derived at response time",
                    "in:response" => "more dynamic metrics",
                ));

                // the new logger must be returned in an Option<Arc<Logger>>
                Some(Arc::new(new_logger))
            })
        })
```

The `Box::pin( async move { ... } )` structure allows for calling `async`
functions, such as executing a database query.

If you know of a cleaner or simpler way of providing `async` callback functions,
the suggestions are very much welcome!
