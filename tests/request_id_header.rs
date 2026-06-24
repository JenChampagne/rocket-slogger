#![cfg(feature = "transaction_header")]

use rocket::local::blocking::Client;
use rocket::{get, routes};
use rocket_slogger::{o, Logger, Slogger};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// A slog drain that records the value of the `transaction` field from every log
/// line, so a test can confirm the `X-Request-Id` header reuses that same id
/// rather than minting a fresh one.
#[derive(Clone)]
struct TransactionCaptureDrain {
    ids: Arc<Mutex<Vec<String>>>,
}

struct TransactionGrabber {
    id: Option<String>,
}

impl slog::Serializer for TransactionGrabber {
    fn emit_arguments(&mut self, key: slog::Key, value: &std::fmt::Arguments) -> slog::Result {
        if key == "transaction" {
            self.id = Some(value.to_string());
        }
        Ok(())
    }
}

impl slog::Drain for TransactionCaptureDrain {
    type Ok = ();
    type Err = slog::Never;

    fn log(
        &self,
        record: &slog::Record,
        values: &slog::OwnedKVList,
    ) -> Result<Self::Ok, Self::Err> {
        use slog::KV;

        let mut grabber = TransactionGrabber { id: None };
        let _ = values.serialize(record, &mut grabber);
        if let Some(id) = grabber.id {
            self.ids
                .lock()
                .expect("I expect to lock the captured ids")
                .push(id);
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

#[test]
fn test_header_matches_logged_transaction_id() {
    let ids = Arc::new(Mutex::new(Vec::new()));
    let logger = Logger::root(TransactionCaptureDrain { ids: ids.clone() }, o!());
    let client = client_with(Slogger::from_logger(logger).with_request_id_header());

    let response = client.get("/keep").dispatch();
    let header = response
        .headers()
        .get_one("X-Request-Id")
        .expect("I expect an X-Request-Id header on a logged response")
        .to_string();

    let logged = ids.lock().expect("I expect to lock the captured ids");
    assert!(
        !logged.is_empty(),
        "I expect the logs to carry a transaction id"
    );
    assert!(
        logged.iter().all(|id| *id == header),
        "I expect every logged transaction id to equal the header id"
    );
}
