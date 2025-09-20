use crate::Slogger;
use rocket::request::{FromRequest, Outcome};
use rocket::Request;
use slog::Logger;
use std::sync::{Arc, Mutex, MutexGuard};

/// A single field value, kept as a typed variant so numeric and boolean fields
/// survive to the output drain as JSON numbers and booleans rather than being
/// stringified. This is what a hand-rolled `Option<T>`-per-field bag loses when
/// it funnels everything through `String`.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldValue {
    Str(String),
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
    /// An explicit null. Distinct from a field that was never set, which is
    /// simply absent. Use `set_some` when you want absence instead of null.
    Null,
}

impl FieldValue {
    /// Emit this value under `key` into a slog serializer, choosing the typed
    /// `emit_*` call that preserves the value's kind.
    fn emit(&self, key: &'static str, serializer: &mut dyn slog::Serializer) -> slog::Result {
        match self {
            FieldValue::Str(value) => serializer.emit_str(key, value),
            FieldValue::I64(value) => serializer.emit_i64(key, *value),
            FieldValue::U64(value) => serializer.emit_u64(key, *value),
            FieldValue::F64(value) => serializer.emit_f64(key, *value),
            FieldValue::Bool(value) => serializer.emit_bool(key, *value),
            FieldValue::Null => serializer.emit_none(key),
        }
    }
}

impl From<&str> for FieldValue {
    fn from(value: &str) -> Self {
        FieldValue::Str(value.to_owned())
    }
}

impl From<String> for FieldValue {
    fn from(value: String) -> Self {
        FieldValue::Str(value)
    }
}

impl From<&String> for FieldValue {
    fn from(value: &String) -> Self {
        FieldValue::Str(value.clone())
    }
}

impl From<bool> for FieldValue {
    fn from(value: bool) -> Self {
        FieldValue::Bool(value)
    }
}

macro_rules! impl_from_int {
    ($variant:ident; $($source:ty),+) => {
        $(
            impl From<$source> for FieldValue {
                fn from(value: $source) -> Self {
                    FieldValue::$variant(value as _)
                }
            }
        )+
    };
}

impl_from_int!(I64; i8, i16, i32, i64, isize);
impl_from_int!(U64; u8, u16, u32, u64, usize);

macro_rules! impl_from_float {
    ($($source:ty),+) => {
        $(
            impl From<$source> for FieldValue {
                fn from(value: $source) -> Self {
                    FieldValue::F64(value as f64)
                }
            }
        )+
    };
}

impl_from_float!(f32, f64);

/// A request-scoped bag of fields that the fairing merges onto the auto-emitted
/// "Response" log line. Any code path holding a `&Request` (auth guards,
/// `FromRequest` impls) reaches it via [`SloggerExt::response_log`]; handlers
/// take it directly as a request guard. Both resolve to the same per-request
/// instance, so unrelated layers accumulate fields without coordinating.
///
/// Fields only ever reach the Response line. The Request line is emitted before
/// routing, so it has already been written by the time a handler or guard runs.
///
/// Cloning is cheap: every handle shares one inner store, which is how a value
/// set in an auth guard is visible to the fairing at response time.
///
/// ```
/// use rocket_slogger::ResponseLog;
///
/// let log = ResponseLog::default();
/// log.set("user_id", 42_u64); // typed: stays a number in the output
/// log.set("plan", "pro"); // string field
/// log.set_some("tenant", Some("acme")); // recorded, the value is present
/// log.set_some("trial_days", None::<u64>); // skipped, field stays absent
/// ```
#[derive(Clone, Debug, Default)]
pub struct ResponseLog {
    inner: Arc<Mutex<Vec<(&'static str, FieldValue)>>>,
}

impl ResponseLog {
    /// Record `value` under `key`, replacing any prior value for the same key
    /// so the last write wins. Pass a `Null` (via `FieldValue::Null`) to record
    /// an explicit null; pass nothing through `set_some` to leave the field
    /// absent.
    pub fn set(&self, key: &'static str, value: impl Into<FieldValue>) {
        let value = value.into();
        let mut fields = self.lock();
        match fields.iter_mut().find(|(existing, _)| *existing == key) {
            Some(slot) => slot.1 = value,
            None => fields.push((key, value)),
        }
    }

    /// Record `value` only when it is `Some`. A `None` records nothing, so the
    /// field is absent from the line rather than emitted as null. This is the
    /// "attach this field only if present" path that replaces stacks of
    /// `if let Some(x) = .. { logger = logger.new(..) }`.
    pub fn set_some<V: Into<FieldValue>>(&self, key: &'static str, value: Option<V>) {
        if let Some(value) = value {
            self.set(key, value);
        }
    }

    /// A point-in-time copy of the accumulated fields, used by the fairing to
    /// merge them onto the Response line.
    pub(crate) fn snapshot(&self) -> ResponseLogSnapshot {
        ResponseLogSnapshot(self.lock().clone())
    }

    fn lock(&self) -> MutexGuard<'_, Vec<(&'static str, FieldValue)>> {
        // A poisoned lock means a prior writer panicked while holding it. The
        // accumulated fields are still structurally valid, and a logging side
        // channel must never panic the request it is observing, so recover the
        // guard and carry on rather than propagate the poison.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// An owned snapshot of a [`ResponseLog`] that implements `slog::KV`, letting
/// the fairing graft accumulated fields onto a logger with `logger.new(..)`.
#[derive(Clone)]
pub struct ResponseLogSnapshot(Vec<(&'static str, FieldValue)>);

impl ResponseLogSnapshot {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl slog::KV for ResponseLogSnapshot {
    fn serialize(
        &self,
        _record: &slog::Record,
        serializer: &mut dyn slog::Serializer,
    ) -> slog::Result {
        for (key, value) in &self.0 {
            value.emit(key, serializer)?;
        }
        Ok(())
    }
}

/// Methods on `rocket::Request` for reaching the request's logger and field bag
/// from any code path that holds a `&Request`. Bring it into scope with
/// `use rocket_slogger::SloggerExt;`.
///
/// ```
/// use rocket_slogger::{info, SloggerExt};
/// use rocket::Request;
///
/// // Any layer holding a `&Request`, such as an auth guard, can both log and
/// // attach a field to the eventual Response line.
/// fn note_authenticated_user(request: &Request<'_>, user_id: &str) {
///     info!(request.logger(), "authenticated");
///     request.response_log().set("user_id", user_id);
/// }
/// ```
pub trait SloggerExt {
    /// The request-enriched logger. In a running app this never fails: the
    /// fairing manages a `Slogger` from ignite onward, so the state is present
    /// for the whole lifetime of the app.
    ///
    /// # Panics
    ///
    /// Panics if the [`Slogger`] fairing was never attached, which is a setup
    /// bug rather than a per-request condition. Use [`try_logger`] when a
    /// caller cannot guarantee the fairing is present and wants to handle
    /// its absence.
    ///
    /// [`try_logger`]: SloggerExt::try_logger
    fn logger(&self) -> Logger;

    /// The request-enriched logger, or `None` if the [`Slogger`] fairing was
    /// never attached. The non-panicking counterpart to [`logger`].
    ///
    /// [`logger`]: SloggerExt::logger
    fn try_logger(&self) -> Option<Logger>;

    /// The request-scoped [`ResponseLog`] field bag. The returned handle shares
    /// the same per-request store every other caller sees.
    fn response_log(&self) -> ResponseLog;
}

impl SloggerExt for Request<'_> {
    // The only failure mode is a request reaching this code without the fairing
    // attached, which `on_ignite` rules out for any running app. The panic is
    // documented on the trait method and `try_logger` is the non-panicking
    // path, so the `expect` is a justified, single-site exception to the lint.
    #[allow(clippy::expect_used)]
    fn logger(&self) -> Logger {
        self.try_logger()
            .expect("Slogger fairing to be attached when accessing the request logger")
    }

    fn try_logger(&self) -> Option<Logger> {
        self.rocket()
            .state::<Slogger>()
            .map(|slogger| slogger.get_for_request(self))
    }

    fn response_log(&self) -> ResponseLog {
        self.local_cache(ResponseLog::default).clone()
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ResponseLog {
    // Resolving the request-scoped bag cannot fail, so the error type is
    // uninhabited: this guard always succeeds.
    type Error = std::convert::Infallible;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        Outcome::Success(request.local_cache(ResponseLog::default).clone())
    }
}

#[cfg(test)]
mod tests {
    use super::{FieldValue, ResponseLog};

    #[test]
    fn test_integer_converts_to_typed_i64() {
        assert_eq!(
            FieldValue::from(-7_i32),
            FieldValue::I64(-7),
            "I expect a signed integer to convert to a typed I64, not a string"
        );
    }

    #[test]
    fn test_unsigned_converts_to_typed_u64() {
        assert_eq!(
            FieldValue::from(42_u16),
            FieldValue::U64(42),
            "I expect an unsigned integer to convert to a typed U64"
        );
    }

    #[test]
    fn test_bool_converts_to_typed_bool() {
        assert_eq!(
            FieldValue::from(true),
            FieldValue::Bool(true),
            "I expect a bool to convert to a typed Bool"
        );
    }

    #[test]
    fn test_set_then_snapshot_carries_the_field() {
        let log = ResponseLog::default();
        log.set("user_id", "abc");
        let snapshot = log.snapshot();
        assert_eq!(
            snapshot.0,
            vec![("user_id", FieldValue::Str("abc".to_owned()))],
            "I expect a set field to appear in the snapshot"
        );
    }

    #[test]
    fn test_set_replaces_prior_value_for_same_key() {
        let log = ResponseLog::default();
        log.set("count", 1_i64);
        log.set("count", 2_i64);
        let snapshot = log.snapshot();
        assert_eq!(
            snapshot.0,
            vec![("count", FieldValue::I64(2))],
            "I expect the last write for a key to win and not duplicate the key"
        );
    }

    #[test]
    fn test_set_some_with_none_records_nothing() {
        let log = ResponseLog::default();
        log.set_some("tenant", None::<String>);
        assert!(
            log.snapshot().is_empty(),
            "I expect set_some(None) to leave the field absent"
        );
    }

    #[test]
    fn test_set_some_with_value_records_it() {
        let log = ResponseLog::default();
        log.set_some("tenant", Some(99_u64));
        let snapshot = log.snapshot();
        assert_eq!(
            snapshot.0,
            vec![("tenant", FieldValue::U64(99))],
            "I expect set_some(Some) to record the field"
        );
    }

    #[test]
    fn test_clones_share_one_store() {
        let log = ResponseLog::default();
        let clone = log.clone();
        clone.set("via_clone", true);
        let snapshot = log.snapshot();
        assert_eq!(
            snapshot.0,
            vec![("via_clone", FieldValue::Bool(true))],
            "I expect a write through a clone to be visible on the original handle"
        );
    }
}
