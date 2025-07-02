use crate::Slogger;
use rocket::http::Status;
use rocket::request::{FromRequest, Outcome};
use rocket::{Request, State};

/// Inject a request-enriched [`Slogger`] into a handler as a request guard.
///
/// This can fail, unlike the [`ResponseLog`](crate::ResponseLog) guard which is
/// `Infallible`. It resolves the managed `Slogger` from Rocket state, and a
/// missing state value (the fairing was never attached) yields a `500`. The
/// `ResponseLog` guard has no such dependency: it materializes from the
/// request's local cache and so cannot miss.
#[rocket::async_trait]
impl<'r> FromRequest<'r> for Slogger {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Slogger, ()> {
        match request.guard::<&State<Slogger>>().await {
            Outcome::Success(slogger) => {
                let logger = slogger.get_for_request(request);

                rocket::outcome::Outcome::Success(Slogger::from_logger(logger))
            }

            _ => Outcome::Error((Status::InternalServerError, ())),
        }
    }
}
