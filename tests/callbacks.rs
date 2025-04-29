#![cfg(feature = "callbacks")]

use rocket::local::blocking::Client;
use rocket::{get, routes};
use rocket_slogger::{log_fields, o, Logger, Slogger};
use std::sync::{Arc, Mutex};

/// Collects every key/value a record carries into plain strings. slog provides
/// default typed `emit_*` methods that forward to `emit_arguments`, so capturing
/// arguments is enough to see all fields.
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

/// Records each log line as its message plus the fields carried on the logger,
/// so a test can assert which callback-added fields landed on which line.
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

#[get("/keep")]
fn keep() -> &'static str {
    "keep"
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
    let rocket = rocket::build().attach(slogger).mount("/", routes![keep]);
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
fn test_on_request_callback_adds_field_to_request_line() {
    let (slogger, lines) = capture();
    let slogger = slogger.on_request(|logger, _request| {
        Box::pin(async move {
            let enriched = logger.new(log_fields!("req_field" => "req_val"));
            Some(Arc::new(enriched))
        })
    });

    client_with(slogger).get("/keep").dispatch();

    assert_eq!(
        value_of(&lines, "Request", "req_field"),
        Some("req_val".to_string()),
        "I expect the on_request callback field to land on the Request line"
    );
}

#[test]
fn test_on_response_callback_adds_field_to_response_line() {
    let (slogger, lines) = capture();
    let slogger = slogger.on_response(|logger, _request, _response| {
        Box::pin(async move {
            let enriched = logger.new(log_fields!("res_field" => "res_val"));
            Some(Arc::new(enriched))
        })
    });

    client_with(slogger).get("/keep").dispatch();

    assert_eq!(
        value_of(&lines, "Response", "res_field"),
        Some("res_val".to_string()),
        "I expect the on_response callback field to land on the Response line"
    );
}

#[test]
fn test_callback_returning_none_keeps_the_prior_logger() {
    let (slogger, lines) = capture();
    let slogger = slogger
        .on_request(|logger, _request| {
            Box::pin(async move {
                let enriched = logger.new(log_fields!("kept" => "yes"));
                Some(Arc::new(enriched))
            })
        })
        // A handler that declines to enrich must not discard the prior logger.
        .on_request(|_logger, _request| Box::pin(async move { None }));

    client_with(slogger).get("/keep").dispatch();

    assert_eq!(
        value_of(&lines, "Request", "kept"),
        Some("yes".to_string()),
        "I expect a None-returning handler to leave the earlier handler's field intact"
    );
}

#[test]
fn test_handlers_run_in_registration_order() {
    let (slogger, _lines) = capture();
    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    let first = order.clone();
    let second = order.clone();
    let slogger = slogger
        .on_request(move |logger, _request| {
            let first = first.clone();
            Box::pin(async move {
                first.lock().expect("I expect to lock order").push("first");
                Some(logger)
            })
        })
        .on_request(move |logger, _request| {
            let second = second.clone();
            Box::pin(async move {
                second
                    .lock()
                    .expect("I expect to lock order")
                    .push("second");
                Some(logger)
            })
        });

    client_with(slogger).get("/keep").dispatch();

    assert_eq!(
        *order.lock().expect("I expect to lock order"),
        vec!["first", "second"],
        "I expect request handlers to run in the order they were registered"
    );
}
