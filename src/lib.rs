pub mod fairing;
mod filter;
pub mod from_request;
pub mod response_log;

pub use response_log::{FieldValue, ResponseLog, SloggerExt};

#[cfg(feature = "transactions")]
pub mod transaction;

// various slog re-exports for convenience
pub use slog::{o, o as log_fields, Drain, Logger};
// logging macros that are compiled away in release mode
pub use slog::{debug, trace};
// logging macros that are kept in all builds
pub use slog::{error, info, warn};

use crate::filter::{ResolvedFilter, RouteKey};
use rocket::{Request, Response, Route};
use std::sync::{Arc, OnceLock};

#[allow(unused_imports)]
use std::future::Future;
#[allow(unused_imports)]
use std::pin::Pin;

/// The boxed future a callback handler returns: an optional replacement logger.
#[cfg(feature = "callbacks")]
type HandlerFuture<'r> = Pin<Box<dyn Future<Output = Option<Arc<Logger>>> + Send + 'r>>;

/// A stored `on_request` callback. See [`Slogger::on_request`].
#[cfg(feature = "callbacks")]
type RequestHandler = Arc<
    dyn for<'r> Fn(Arc<Logger>, &'r mut Request<'_>) -> HandlerFuture<'r> + Send + Sync + 'static,
>;

/// A stored `on_response` callback. See [`Slogger::on_response`].
#[cfg(feature = "callbacks")]
type ResponseHandler = Arc<
    dyn for<'r> Fn(Arc<Logger>, &'r Request<'_>, &'r mut Response<'_>) -> HandlerFuture<'r>
        + Send
        + Sync
        + 'static,
>;

#[derive(Clone)]
pub struct Slogger {
    logger: Arc<Logger>,

    filter_show: Vec<RouteKey>,
    filter_skip: Vec<RouteKey>,
    resolved: Arc<OnceLock<ResolvedFilter>>,

    #[cfg(feature = "transaction_header")]
    emit_request_id_header: bool,

    #[cfg(feature = "callbacks")]
    request_handlers: Vec<RequestHandler>,

    #[cfg(feature = "callbacks")]
    response_handlers: Vec<ResponseHandler>,
}

impl Slogger {
    #[cfg(all(feature = "terminal", not(feature = "envlogger")))]
    pub fn new_terminal_logger() -> Self {
        use slog_term::{FullFormat, PlainSyncDecorator};

        let plain_logger = PlainSyncDecorator::new(std::io::stdout());
        let logger = Logger::root(FullFormat::new(plain_logger).build().fuse(), log_fields!());

        Self::from_logger(logger)
    }

    #[cfg(all(feature = "terminal", feature = "envlogger"))]
    pub fn new_terminal_logger() -> Self {
        use slog_envlogger::EnvLogger;
        use slog_term::{FullFormat, PlainSyncDecorator};

        let plain_logger = PlainSyncDecorator::new(std::io::stdout());
        let term_drain = FullFormat::new(plain_logger).build();
        let env_logger = EnvLogger::new(term_drain);
        let logger = Logger::root(env_logger.fuse(), log_fields!());

        Self::from_logger(logger)
    }

    #[cfg(all(feature = "bunyan", not(feature = "envlogger")))]
    pub fn new_bunyan_logger(name: &'static str) -> Self {
        use std::sync::Mutex;

        let bunyan_logger = slog_bunyan::with_name(name, std::io::stderr()).build();
        let logger = Logger::root(Mutex::new(bunyan_logger).fuse(), log_fields!());

        Self::from_logger(logger)
    }

    #[cfg(all(feature = "bunyan", feature = "envlogger"))]
    pub fn new_bunyan_logger(name: &'static str) -> Self {
        use slog_envlogger::EnvLogger;
        use std::sync::Mutex;

        let bunyan_logger = slog_bunyan::with_name(name, std::io::stderr()).build();
        let env_logger = EnvLogger::new(bunyan_logger);
        let logger = Logger::root(Mutex::new(env_logger).fuse(), log_fields!());

        Self::from_logger(logger)
    }

    pub fn from_logger(logger: Logger) -> Self {
        Self {
            logger: Arc::new(logger),

            filter_show: vec![],
            filter_skip: vec![],
            resolved: Arc::new(OnceLock::new()),

            #[cfg(feature = "transaction_header")]
            emit_request_id_header: false,

            #[cfg(feature = "callbacks")]
            request_handlers: vec![],

            #[cfg(feature = "callbacks")]
            response_handlers: vec![],
        }
    }

    pub fn get(&self) -> &Logger {
        &self.logger
    }

    pub fn get_for_request(&self, request: &Request<'_>) -> Logger {
        let content_type = request.content_type().map(|format| format.to_string());
        let user_agent = request
            .headers()
            .get("user-agent")
            .collect::<Vec<_>>()
            .join("; ");

        #[cfg(not(feature = "transactions"))]
        let logger = self.logger.new(log_fields!(
            "user-agent" => user_agent,
            "content-type" => content_type,
        ));

        #[cfg(feature = "transactions")]
        let logger = {
            let transaction = transaction::RequestTransaction::get_or_init(request);

            self.logger.new(log_fields!(
                "received" => transaction.received_as_string(),
                "transaction" => transaction.id_as_string(),

                "user-agent" => user_agent,
                "content-type" => content_type,
            ))
        };

        Self::new_logger_with_request_details(&logger, request)
    }

    pub fn get_for_response(&self, request: &Request<'_>, response: &Response<'_>) -> Logger {
        let content_type = response.content_type().map(|format| format.to_string());
        let status = response.status();

        #[cfg(not(feature = "transactions"))]
        let logger = self.logger.new(log_fields!(
            "content-type" => content_type,
            "reason" => status.reason().map(|reason| reason.to_string()),
            "code" => status.code,
        ));

        #[cfg(feature = "transactions")]
        let logger = {
            let transaction = transaction::RequestTransaction::get_or_init(request);

            self.logger.new(log_fields!(
                "elapsed_ns" => transaction.elapsed_ns(),
                "received" => transaction.received_as_string(),
                "transaction" => transaction.id_as_string(),
                "content-type" => content_type,
                "reason" => status.reason().map(|reason| reason.to_string()),
                "code" => status.code,
            ))
        };

        Self::new_logger_with_request_details(&logger, request)
    }

    fn new_logger_with_request_details(logger: &Logger, request: &Request<'_>) -> Logger {
        if let Some(route) = request.route() {
            logger.new(log_fields!(
                "rank" => route.rank,
                "route" => route.name.as_ref().map(|route| route.to_string()),
                "path" => format!("{}", route.uri),
                "method" => format!("{}", route.method),
                "uri" => format!("{}", request.uri()),
            ))
        } else {
            logger.new(log_fields!(
                "method" => format!("{}", request.method()),
                "uri" => format!("{}", request.uri()),
            ))
        }
    }

    /// Skip the automatic request/response logs for the given routes. Pass the
    /// value produced by `rocket::routes![...]`. Combine with
    /// `show_reqres_logs`: a skipped route wins over a shown one on overlap.
    pub fn skip_reqres_logs(mut self, routes: Vec<Route>) -> Self {
        self.filter_skip
            .extend(routes.iter().map(RouteKey::from_route));
        self
    }

    /// Show the automatic request/response logs only for the given routes.
    /// Pass the value produced by `rocket::routes![...]`. When this is set,
    /// routes not listed are not logged. Leaving it unset (the default)
    /// logs every route.
    pub fn show_reqres_logs(mut self, routes: Vec<Route>) -> Self {
        self.filter_show
            .extend(routes.iter().map(RouteKey::from_route));
        self
    }

    /// Set an `X-Request-Id` response header to the request's transaction id.
    /// Off by default: a logging fairing should not alter responses unless
    /// asked. Requires the `transaction_header` feature.
    #[cfg(feature = "transaction_header")]
    pub fn with_request_id_header(mut self) -> Self {
        self.emit_request_id_header = true;
        self
    }

    /// Decide whether this request should be logged. Resolves the listed routes
    /// to mounted path patterns on first use, then matches by method and path.
    pub(crate) fn filter_decision(&self, request: &Request<'_>) -> bool {
        let resolved = self.resolved.get_or_init(|| {
            let routes: Vec<&Route> = request.rocket().routes().collect();
            ResolvedFilter::resolve(&routes, &self.filter_show, &self.filter_skip)
        });

        resolved.should_log(request.method(), request.uri().path().as_str())
    }

    #[cfg(feature = "callbacks")]
    pub fn on_request(
        mut self,
        handler: impl for<'r> Fn(
                Arc<Logger>,
                &'r mut Request<'_>,
            )
                -> Pin<Box<dyn Future<Output = Option<Arc<Logger>>> + Send + 'r>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.request_handlers.push(Arc::new(handler));
        self
    }

    #[cfg(feature = "callbacks")]
    pub fn on_response(
        mut self,
        handler: impl for<'r> Fn(
                Arc<Logger>,
                &'r Request<'_>,
                &'r mut Response<'_>,
            )
                -> Pin<Box<dyn Future<Output = Option<Arc<Logger>>> + Send + 'r>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.response_handlers.push(Arc::new(handler));
        self
    }
}

impl From<Logger> for Slogger {
    fn from(logger: Logger) -> Self {
        Slogger::from_logger(logger)
    }
}

impl From<&Logger> for Slogger {
    fn from(logger: &Logger) -> Self {
        Slogger::from_logger(logger.clone())
    }
}

impl std::ops::Deref for Slogger {
    type Target = Logger;

    fn deref(&self) -> &Logger {
        &self.logger
    }
}

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
    fn test_skip_reqres_logs_stores_one_key() {
        let slogger = silent_slogger().skip_reqres_logs(routes![skip]);
        assert_eq!(
            slogger.filter_skip.len(),
            1,
            "I expect one skipped route key"
        );
        assert_eq!(slogger.filter_show.len(), 0, "I expect no shown-route keys");
    }

    #[test]
    fn test_show_reqres_logs_accumulates_keys() {
        let slogger = silent_slogger()
            .show_reqres_logs(routes![skip])
            .show_reqres_logs(routes![keep]);
        assert_eq!(
            slogger.filter_show.len(),
            2,
            "I expect two accumulated shown-route keys"
        );
    }

    /// Regression guard: `new_terminal_logger` must compose its drain correctly
    /// under every feature combination, including `terminal` + `envlogger`,
    /// where the `EnvLogger` has to wrap the built terminal drain.
    #[cfg(feature = "terminal")]
    #[test]
    fn test_new_terminal_logger_constructs() {
        let _slogger = Slogger::new_terminal_logger();
    }
}
