#![cfg(feature = "transaction_header")]

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
    let client = client_with(
        silent_slogger()
            .with_request_id_header()
            .skip_reqres_logs(routes![skip]),
    );

    let response = client.get("/skip").dispatch();
    assert!(
        response.headers().get_one("X-Request-Id").is_none(),
        "I expect no X-Request-Id header on a denied (unlogged) route"
    );
}
