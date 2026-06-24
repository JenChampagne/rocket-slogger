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
    // Swap `.skip_reqres_logs(...)` for `.show_reqres_logs(...)` to log only the
    // listed routes instead.
    #[allow(unused_mut)]
    let mut fairing =
        Slogger::from_logger(logger).skip_reqres_logs(routes![always_fail, dynamic_path]);

    // Only available with the `transaction_header` feature (which requires
    // `transactions`, the source of the id the header carries).
    #[cfg(feature = "transaction_header")]
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
