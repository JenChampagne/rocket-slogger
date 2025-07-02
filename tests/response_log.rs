use rocket::local::blocking::Client;
use rocket::request::{FromRequest, Outcome};
use rocket::{get, routes, Request};
use rocket_slogger::{o, Logger, ResponseLog, Slogger, SloggerExt};
use std::sync::{Arc, Mutex};

/// Collects every key/value a record carries into plain strings. slog's typed
/// `emit_*` methods forward to `emit_arguments` by default, so capturing
/// arguments is enough to see all fields regardless of their kind.
#[derive(Default)]
struct FieldCollector {
    fields: Vec<(String, String)>,
}

impl slog::Serializer for FieldCollector {
    fn emit_arguments(&mut self, key: slog::Key, value: &std::fmt::Arguments) -> slog::Result {
        self.fields.push((key.to_string(), value.to_string()));
        Ok(())
    }
}

type Captured = Vec<(String, Vec<(String, String)>)>;

#[derive(Clone)]
struct CaptureDrain {
    lines: Arc<Mutex<Captured>>,
}

impl slog::Drain for CaptureDrain {
    type Ok = ();
    type Err = slog::Never;

    fn log(
        &self,
        record: &slog::Record,
        values: &slog::OwnedKVList,
    ) -> Result<Self::Ok, Self::Err> {
        use slog::KV;

        let mut collector = FieldCollector::default();
        let _ = values.serialize(record, &mut collector);
        if let Ok(mut lines) = self.lines.lock() {
            lines.push((record.msg().to_string(), collector.fields));
        }
        Ok(())
    }
}

/// A guard that only exists to exercise the `&Request` path: it writes a field
/// through `SloggerExt::response_log` and touches `SloggerExt::logger`, the way
/// an auth guard would.
struct ViaExt;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ViaExt {
    type Error = std::convert::Infallible;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        // Proves the infallible accessor returns a usable logger with no
        // "missing-app-name" fallback.
        let _logger = request.logger();
        request.response_log().set("from_guard", "ext");
        Outcome::Success(ViaExt)
    }
}

#[get("/handler")]
fn handler(log: ResponseLog) -> &'static str {
    log.set("user_id", "u-1");
    log.set("record_count", 5_i64);
    log.set_some("tenant", Some(99_u64));
    log.set_some("absent", None::<String>);
    "ok"
}

#[get("/guard")]
fn guard(_via: ViaExt) -> &'static str {
    "ok"
}

/// Reports whether the non-panicking `SloggerExt::try_logger` found the
/// fairing, so a test can drive both the present and absent paths without
/// risking a panic.
struct LoggerProbe(bool);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for LoggerProbe {
    type Error = std::convert::Infallible;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        Outcome::Success(LoggerProbe(request.try_logger().is_some()))
    }
}

#[get("/probe")]
fn probe(probe: LoggerProbe) -> &'static str {
    if probe.0 {
        "some"
    } else {
        "none"
    }
}

fn capture() -> (Slogger, Arc<Mutex<Captured>>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let drain = CaptureDrain {
        lines: lines.clone(),
    };
    let logger = Logger::root(drain, o!());
    (Slogger::from_logger(logger), lines)
}

fn client_with(slogger: Slogger) -> Client {
    let rocket = rocket::build()
        .attach(slogger)
        .mount("/", routes![handler, guard, probe]);
    Client::tracked(rocket).expect("I expect a valid Rocket instance")
}

/// A client whose Rocket never attaches the fairing, so `try_logger` has no
/// `Slogger` state to find.
fn client_without_fairing() -> Client {
    let rocket = rocket::build().mount("/", routes![probe]);
    Client::tracked(rocket).expect("I expect a valid Rocket instance")
}

fn value_of(lines: &Arc<Mutex<Captured>>, message: &str, key: &str) -> Option<String> {
    lines
        .lock()
        .expect("I expect to lock the captured lines")
        .iter()
        .find(|(logged, _)| logged == message)
        .and_then(|(_, fields)| {
            fields
                .iter()
                .find(|(field_key, _)| field_key == key)
                .map(|(_, value)| value.clone())
        })
}

#[test]
fn test_guard_set_fields_land_on_response_line() {
    let (slogger, lines) = capture();
    client_with(slogger).get("/handler").dispatch();

    assert_eq!(
        value_of(&lines, "Response", "user_id"),
        Some("u-1".to_string()),
        "I expect a field set through the ResponseLog guard to land on the Response line"
    );
    assert_eq!(
        value_of(&lines, "Response", "record_count"),
        Some("5".to_string()),
        "I expect a numeric field set through the guard to land on the Response line"
    );
}

#[test]
fn test_set_some_some_lands_and_none_is_absent() {
    let (slogger, lines) = capture();
    client_with(slogger).get("/handler").dispatch();

    assert_eq!(
        value_of(&lines, "Response", "tenant"),
        Some("99".to_string()),
        "I expect set_some(Some) to land on the Response line"
    );
    assert_eq!(
        value_of(&lines, "Response", "absent"),
        None,
        "I expect set_some(None) to leave the field off the Response line entirely"
    );
}

#[test]
fn test_fields_do_not_leak_onto_the_request_line() {
    let (slogger, lines) = capture();
    client_with(slogger).get("/handler").dispatch();

    assert_eq!(
        value_of(&lines, "Request", "user_id"),
        None,
        "I expect handler-set fields not to appear on the Request line, which is emitted before routing"
    );
}

#[test]
fn test_try_logger_is_some_when_fairing_attached() {
    let (slogger, _lines) = capture();
    let client = client_with(slogger);
    let response = client.get("/probe").dispatch();
    assert_eq!(
        response.into_string().as_deref(),
        Some("some"),
        "I expect try_logger to return Some when the fairing is attached"
    );
}

#[test]
fn test_try_logger_is_none_without_fairing() {
    let client = client_without_fairing();
    let response = client.get("/probe").dispatch();
    assert_eq!(
        response.into_string().as_deref(),
        Some("none"),
        "I expect try_logger to return None when the fairing was never attached"
    );
}

#[test]
fn test_ext_response_log_writes_through_to_the_response_line() {
    let (slogger, lines) = capture();
    client_with(slogger).get("/guard").dispatch();

    assert_eq!(
        value_of(&lines, "Response", "from_guard"),
        Some("ext".to_string()),
        "I expect a field set via SloggerExt::response_log on a &Request to land on the Response line"
    );
}
