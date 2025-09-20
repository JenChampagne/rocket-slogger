use rocket::local::blocking::Client;
use rocket::{get, routes};
use rocket_slogger::{o, Logger, Slogger};
use slog::KV;
use std::sync::{Arc, Mutex};

/// One captured log line: its message and its inline key/value fields.
#[derive(Clone)]
struct Captured {
    msg: String,
    fields: Vec<(String, String)>,
}

/// Collects a record's inline key/value pairs as strings.
struct FieldCollector {
    fields: Vec<(String, String)>,
}

impl slog::Serializer for FieldCollector {
    fn emit_arguments(
        &mut self,
        key: slog::Key,
        val: &std::fmt::Arguments,
    ) -> slog::Result {
        self.fields.push((key.to_string(), val.to_string()));
        Ok(())
    }
}

/// A slog drain that records each log line's message and inline fields, so a
/// test can assert which automatic log lines were emitted and with what values.
#[derive(Clone)]
struct CaptureDrain {
    records: Arc<Mutex<Vec<Captured>>>,
}

impl slog::Drain for CaptureDrain {
    type Ok = ();
    type Err = slog::Never;

    fn log(
        &self,
        record: &slog::Record,
        _values: &slog::OwnedKVList,
    ) -> Result<Self::Ok, Self::Err> {
        let mut collector = FieldCollector { fields: Vec::new() };
        let _ = record.kv().serialize(record, &mut collector);
        if let Ok(mut records) = self.records.lock() {
            records.push(Captured {
                msg: record.msg().to_string(),
                fields: collector.fields,
            });
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

fn count(records: &Arc<Mutex<Vec<Captured>>>, needle: &str) -> usize {
    records
        .lock()
        .expect("I expect to lock the captured records")
        .iter()
        .filter(|record| record.msg.as_str() == needle)
        .count()
}

fn capture() -> (Slogger, Arc<Mutex<Vec<Captured>>>) {
    let records = Arc::new(Mutex::new(Vec::new()));
    let drain = CaptureDrain {
        records: records.clone(),
    };
    let logger = Logger::root(drain, o!());
    (Slogger::from_logger(logger), records)
}

/// The value of `field` on the `Route Registered` line whose `path` matches.
fn route_field(
    records: &Arc<Mutex<Vec<Captured>>>,
    path: &str,
    field: &str,
) -> Option<String> {
    let records = records.lock().expect("I expect to lock the captured records");
    records
        .iter()
        .filter(|record| record.msg == "Route Registered")
        .find(|record| {
            record
                .fields
                .iter()
                .any(|(key, value)| key == "path" && value == path)
        })
        .and_then(|record| {
            record
                .fields
                .iter()
                .find(|(key, _)| key == field)
                .map(|(_, value)| value.clone())
        })
}

#[get("/users/<id>")]
fn user(id: &str) -> String {
    format!("user {id}")
}

#[get("/users/admin")]
fn user_admin() -> &'static str {
    "admin"
}

fn client_with(
    slogger_builder: impl FnOnce(Slogger) -> Slogger,
) -> (Client, Arc<Mutex<Vec<Captured>>>) {
    let (slogger, records) = capture();
    let rocket = rocket::build()
        .attach(slogger_builder(slogger))
        .mount("/", routes![keep, skip, user, user_admin]);
    let client = Client::tracked(rocket).expect("I expect a valid Rocket instance");
    (client, records)
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

#[test]
fn test_route_registered_reports_auto_log() {
    let (_client, records) = client_with(|slogger| slogger.skip_reqres_logs(routes![skip, user_admin]));

    assert_eq!(
        route_field(&records, "/keep", "auto_log").as_deref(),
        Some("always"),
        "I expect an unfiltered route to be always logged"
    );
    assert_eq!(
        route_field(&records, "/skip", "auto_log").as_deref(),
        Some("never"),
        "I expect a skipped route to never log"
    );
    assert_eq!(
        route_field(&records, "/users/admin", "auto_log").as_deref(),
        Some("never"),
        "I expect the fully-skipped static route to never log"
    );
    assert_eq!(
        route_field(&records, "/users/<id>", "auto_log").as_deref(),
        Some("conditional"),
        "I expect the overlapping dynamic route to be conditional"
    );
    assert_eq!(
        route_field(&records, "/users/<id>", "auto_log_overlaps").as_deref(),
        Some("GET /users/admin"),
        "I expect the conditional route to name the skipped pattern it overlaps"
    );
}
