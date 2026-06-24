# TODO

Deferred work, with rationale so the decisions are not lost.

## Offer a non-blocking (async) drain

The built-in constructors all use synchronous, lock-serialized drains:

- `new_terminal_logger` writes through `PlainSyncDecorator` over stdout.
- `new_bunyan_logger` wraps the drain in a `std::sync::Mutex` over stderr.

Both `on_request` and `on_response` run inside async fairing methods, so each
`info!` performs a blocking write while holding a global lock, on a Tokio worker
thread. Under concurrency that serializes all request logging on one lock and
parks an executor thread on a write syscall per line.

`slog-async` exists for exactly this: it hands records to a background thread
over a non-blocking channel. Consider adding an async-drain constructor (or an
`async` feature) that wraps the chosen output in `slog_async::Async`, and
document that the sync constructors are fine for development but should be
wrapped for production throughput.

Consumers can already supply their own async drain via `from_logger`; this is
about making the batteries-included path non-blocking too.
