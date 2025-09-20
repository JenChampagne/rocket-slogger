use crate::filter::{LogDecision, Segment};
use crate::response_log::ResponseLog;
use crate::{info, Slogger};
use rocket::fairing::{Fairing, Info, Kind};
use rocket::{Build, Config, Data, Orbit, Request, Response, Rocket};
use slog::Logger;
use std::sync::Arc;

#[inline]
fn url_from_rocket_config(config: &Config) -> String {
    format!(
        "{scheme}://{address}:{port}",
        scheme = if config.tls_enabled() {
            "https"
        } else {
            "http"
        },
        address = &config.address,
        port = &config.port
    )
}

#[inline]
fn temp_dir_path_from_rocket_config(config: &Config) -> String {
    config
        .temp_dir
        .relative()
        .into_os_string()
        .into_string()
        .unwrap_or_else(|_| String::from(""))
}

#[rocket::async_trait]
impl Fairing for Slogger {
    fn info(&self) -> Info {
        Info {
            name: "Slog Fairing",
            kind: Kind::Ignite | Kind::Liftoff | Kind::Request | Kind::Response,
        }
    }

    async fn on_ignite(&self, rocket: Rocket<Build>) -> Result<Rocket<Build>, Rocket<Build>> {
        Ok(rocket.manage(self.clone()))
    }

    async fn on_liftoff(&self, rocket: &Rocket<Orbit>) {
        let config = rocket.config();

        let url = url_from_rocket_config(config);
        let temp_dir_string = temp_dir_path_from_rocket_config(config);

        info!(
            &self.logger,
            "Rocket Launched";
            "log_level" => %config.log_level,
            "temp_dir" => temp_dir_string,
            "ident" => %config.ident,
            "tls" => config.tls_enabled(),
            "limits" => %config.limits,
            "keep_alive" => config.keep_alive,
            "workers" => config.workers,
            "port" => config.port,
            "host" => %config.address,
            "url" => %url,
            "profile" => %config.profile,
        );

        let resolved = self.resolved_filter(rocket);
        for route in rocket.routes() {
            let status = resolved.classify(route.method, &Segment::parse_path(route.uri.path()));
            info!(
                &self.logger,
                "Route Registered";
                "auto_log_overlaps" => status.overlaps_field(),
                "auto_log" => status.label(),
                "rank" => route.rank,
                "route" => route.name.as_ref().map(|route| route.to_string()),
                "content-type" => route.format.as_ref().map(|format| format.to_string()),
                "path" => %route.uri,
                "url" => format!("{}{}", url, route.uri),
                "method" => %route.method,
            );
        }

        for catcher in rocket.catchers() {
            info!(
                &self.logger,
                "Catcher Registered";
                "route" => catcher.name.as_ref().map(|catcher| catcher.to_string()),
                "code" => catcher.code,
                "path" => %catcher.base,
                "url" => format!("{}{}", url, catcher.base),
            );
        }

        info!(
            &self.logger,
            "Accepting Connections";
            "port" => config.port,
            "host" => %config.address,
            "url" => url,
        );
    }

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

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        let should_log = request.local_cache(|| LogDecision(true)).0;
        if !should_log {
            return;
        }

        #[allow(unused_mut)]
        let mut logger = Arc::new(self.get_for_response(request, response));

        #[cfg(feature = "callbacks")]
        for handler in &self.response_handlers {
            if let Some(new_logger) = handler(logger.clone(), request, response).await {
                logger = new_logger;
            }
        }

        let body_size = response.body_mut().size().await;

        #[cfg(feature = "transaction_header")]
        if self.emit_request_id_header {
            // The transaction was cached during `on_request`, so this reuses
            // the same id the logs carry rather than minting a new one.
            let transaction = crate::transaction::RequestTransaction::get_or_init(request);
            response.set_header(rocket::http::Header::new(
                "X-Request-Id",
                transaction.id_as_string(),
            ));
        }

        // Merge any request-scoped fields that handlers or guards accumulated.
        // Reads the same per-request `ResponseLog` instance they wrote to; an
        // empty bag adds no logger layer.
        let snapshot = request.local_cache(ResponseLog::default).snapshot();
        let logger: Logger = if snapshot.is_empty() {
            (*logger).clone()
        } else {
            logger.new(slog::OwnedKV(snapshot))
        };

        info!(
            logger,
            "Response";
            "size" => body_size,
        );
    }
}
