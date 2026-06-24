use chrono::DateTime;
use rocket::Request;
use uuid::Uuid;

#[cfg(feature = "local_time")]
type TimeZone = chrono::Local;

#[cfg(not(feature = "local_time"))]
type TimeZone = chrono::Utc;

#[derive(Copy, Clone, Debug)]
pub struct RequestTransaction {
    pub id: Uuid,
    pub received: DateTime<TimeZone>,
}

impl Default for RequestTransaction {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestTransaction {
    pub fn new() -> Self {
        Self {
            id: Uuid::new_v4(),
            received: TimeZone::now(),
        }
    }

    pub fn attach_on<'r>(self, request: &'r Request<'_>) -> &'r Self {
        request.local_cache(|| self)
    }

    /// Return the transaction cached on this request, creating one only if none
    /// exists yet. Unlike `attach_on`, the id and timestamp are constructed lazily
    /// inside `local_cache`, so a request that already has a transaction does not
    /// pay for a throwaway `Uuid::new_v4()` and clock read on every later lookup.
    pub fn get_or_init<'r>(request: &'r Request<'_>) -> &'r Self {
        request.local_cache(Self::new)
    }

    pub fn id_as_string(&self) -> String {
        self.id
            .hyphenated()
            .encode_lower(&mut Uuid::encode_buffer())
            .to_string()
    }

    pub fn received_as_string(&self) -> String {
        self.received.to_rfc3339()
    }

    pub fn elapsed_as_string(&self) -> String {
        (TimeZone::now() - self.received).to_string()
    }

    pub fn elapsed_ns(&self) -> Option<i64> {
        (TimeZone::now() - self.received).num_nanoseconds()
    }
}

#[cfg(test)]
mod tests {
    use super::RequestTransaction;
    use uuid::Uuid;

    #[test]
    fn test_id_as_string_round_trips_to_the_same_uuid() {
        let transaction = RequestTransaction::new();
        let rendered = transaction.id_as_string();
        let parsed =
            Uuid::parse_str(&rendered).expect("I expect the id string to be a valid UUID");
        assert_eq!(
            parsed, transaction.id,
            "I expect the rendered id to parse back to the original UUID"
        );
    }

    #[test]
    fn test_received_as_string_is_valid_rfc3339() {
        let transaction = RequestTransaction::new();
        let rendered = transaction.received_as_string();
        chrono::DateTime::parse_from_rfc3339(&rendered)
            .expect("I expect the received timestamp to be valid RFC 3339");
    }

    #[test]
    fn test_elapsed_ns_is_non_negative() {
        let transaction = RequestTransaction::new();
        let elapsed = transaction
            .elapsed_ns()
            .expect("I expect an elapsed nanosecond count");
        assert!(
            elapsed >= 0,
            "I expect elapsed time since creation to be non-negative, got {elapsed}"
        );
    }

    /// Without `local_time` the received timestamp is rendered in UTC, which
    /// chrono writes with a `+00:00` offset.
    #[cfg(not(feature = "local_time"))]
    #[test]
    fn test_received_is_utc_without_local_time_feature() {
        let transaction = RequestTransaction::new();
        let rendered = transaction.received_as_string();
        assert!(
            rendered.ends_with("+00:00"),
            "I expect a UTC offset on the received timestamp, got {rendered}"
        );
    }
}
