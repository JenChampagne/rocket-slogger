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

#[get("/item/<id>")]
fn item(id: &str) -> String {
    format!("item {id}")
}

fn count(messages: &Arc<Mutex<Vec<String>>>, needle: &str) -> usize {
    messages
        .lock()
        .expect("I expect to lock the captured messages")
        .iter()
        .filter(|message| message.as_str() == needle)
        .count()
}

fn capture() -> (Slogger, Arc<Mutex<Vec<String>>>) {
    let messages = Arc::new(Mutex::new(Vec::new()));
    let drain = CaptureDrain {
        messages: messages.clone(),
    };
    let logger = Logger::root(drain, o!());
    (Slogger::from_logger(logger), messages)
}

fn client_with(
    slogger_builder: impl FnOnce(Slogger) -> Slogger,
) -> (Client, Arc<Mutex<Vec<String>>>) {
    let (slogger, messages) = capture();
    let rocket = rocket::build()
        .attach(slogger_builder(slogger))
        .mount("/", routes![keep, skip]);
    let client = Client::tracked(rocket).expect("I expect a valid Rocket instance");
    (client, messages)
}

#[test]
fn test_denied_route_produces_no_lines() {
    let (client, messages) = client_with(|slogger| slogger.skip_reqres_logs(routes![skip]));

    client.get("/skip").dispatch();
    assert_eq!(
        count(&messages, "Request"),
        0,
        "I expect no Request line for a denied route"
    );
    assert_eq!(
        count(&messages, "Response"),
        0,
        "I expect no Response line for a denied route"
    );

    client.get("/keep").dispatch();
    assert_eq!(
        count(&messages, "Request"),
        1,
        "I expect a Request line for a non-denied route"
    );
    assert_eq!(
        count(&messages, "Response"),
        1,
        "I expect a Response line for a non-denied route"
    );
}

#[test]
fn test_allowlist_logs_only_allowed() {
    let (client, messages) = client_with(|slogger| slogger.show_reqres_logs(routes![keep]));

    client.get("/skip").dispatch();
    assert_eq!(
        count(&messages, "Request"),
        0,
        "I expect no Request line for a route outside the allowlist"
    );

    client.get("/keep").dispatch();
    assert_eq!(
        count(&messages, "Request"),
        1,
        "I expect a Request line for an allowed route"
    );
    assert_eq!(
        count(&messages, "Response"),
        1,
        "I expect a Response line for an allowed route"
    );
}

#[test]
fn test_no_lists_logs_everything() {
    let (client, messages) = client_with(|slogger| slogger);

    client.get("/skip").dispatch();
    client.get("/keep").dispatch();
    assert_eq!(
        count(&messages, "Request"),
        2,
        "I expect both routes to log a Request line"
    );
    assert_eq!(
        count(&messages, "Response"),
        2,
        "I expect both routes to log a Response line"
    );
}

#[test]
fn test_filter_resolves_under_nonroot_mount() {
    // The whole point of resolving by route handle is that it survives mounting
    // at any base. Mount at /api and confirm the skip still lands.
    let (slogger, messages) = capture();
    let rocket = rocket::build()
        .attach(slogger.skip_reqres_logs(routes![skip]))
        .mount("/api", routes![keep, skip]);
    let client = Client::tracked(rocket).expect("I expect a valid Rocket instance");

    client.get("/api/skip").dispatch();
    assert_eq!(
        count(&messages, "Request"),
        0,
        "I expect a skipped route to stay skipped when mounted at a non-root base"
    );

    client.get("/api/keep").dispatch();
    assert_eq!(
        count(&messages, "Request"),
        1,
        "I expect a non-skipped route to log when mounted at a non-root base"
    );
}

#[test]
fn test_show_and_skip_combined_deny_wins() {
    let (slogger, messages) = capture();
    let rocket = rocket::build()
        .attach(
            slogger
                .show_reqres_logs(routes![keep, skip])
                .skip_reqres_logs(routes![skip]),
        )
        .mount("/", routes![keep, skip]);
    let client = Client::tracked(rocket).expect("I expect a valid Rocket instance");

    client.get("/skip").dispatch();
    assert_eq!(
        count(&messages, "Request"),
        0,
        "I expect skip to win over show when a route is on both lists"
    );

    client.get("/keep").dispatch();
    assert_eq!(
        count(&messages, "Request"),
        1,
        "I expect a shown, non-skipped route to log"
    );
}

#[test]
fn test_dynamic_route_filtered_by_handle() {
    let (slogger, messages) = capture();
    let rocket = rocket::build()
        .attach(slogger.skip_reqres_logs(routes![item]))
        .mount("/", routes![item, keep]);
    let client = Client::tracked(rocket).expect("I expect a valid Rocket instance");

    client.get("/item/42").dispatch();
    assert_eq!(
        count(&messages, "Request"),
        0,
        "I expect a dynamic route skipped by handle to produce no lines"
    );

    client.get("/keep").dispatch();
    assert_eq!(
        count(&messages, "Request"),
        1,
        "I expect an unlisted route to still log"
    );
}
