use rocket::http::Method;
use rocket::Route;

/// A mount-independent identity for a route, derived from a `Route` produced by
/// `rocket::routes![...]`. Method, name, and the unmounted path are all preserved
/// when a route is mounted at a base, so the same key is computed before and
/// after mounting. This is what lets us correlate a listed route to its live,
/// mounted entry without the developer repeating the mount base.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RouteKey {
    pub(crate) method: Method,
    pub(crate) name: Option<String>,
    pub(crate) unmounted: String,
}

impl RouteKey {
    pub(crate) fn from_route(route: &Route) -> Self {
        Self {
            method: route.method,
            name: route.name.as_ref().map(|name| name.to_string()),
            unmounted: route.uri.unmounted_origin.path().as_str().to_string(),
        }
    }
}

/// One segment of a route path pattern.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Segment {
    /// A literal segment that must equal the request segment.
    Static(String),
    /// `<name>`: matches exactly one request segment.
    Dynamic,
    /// `<name..>`: matches zero or more remaining request segments. Ends a pattern.
    Trailing,
}

impl Segment {
    /// Parse a route path like `/users/<id>` or `/files/<rest..>` into segments.
    /// Empty segments (leading/trailing/double slashes) are dropped.
    pub(crate) fn parse_path(path: &str) -> Vec<Segment> {
        path.split('/')
            .filter(|segment| !segment.is_empty())
            .map(|segment| {
                if segment.starts_with('<') && segment.ends_with("..>") {
                    Segment::Trailing
                } else if segment.starts_with('<') && segment.ends_with('>') {
                    Segment::Dynamic
                } else {
                    Segment::Static(segment.to_string())
                }
            })
            .collect()
    }
}

/// Does the concrete request `path` match this single `pattern`? Query strings
/// are ignored. This is a single-pattern matcher only: no ranking, no collision
/// detection, no format negotiation. It is not a reimplementation of Rocket's
/// router.
pub(crate) fn path_matches(pattern: &[Segment], path: &str) -> bool {
    let request: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();

    let mut index = 0;
    for segment in pattern {
        match segment {
            Segment::Trailing => return true,
            Segment::Dynamic => {
                if index >= request.len() {
                    return false;
                }
                index += 1;
            }
            Segment::Static(expected) => {
                if index >= request.len() || request[index] != expected.as_str() {
                    return false;
                }
                index += 1;
            }
        }
    }

    index == request.len()
}

/// The allow/deny lists resolved to concrete mounted path patterns. Built once
/// from the live route table.
pub(crate) struct ResolvedFilter {
    pub(crate) allow: Vec<(Method, Vec<Segment>)>,
    pub(crate) deny: Vec<(Method, Vec<Segment>)>,
}

impl ResolvedFilter {
    pub(crate) fn resolve(routes: &[&Route], allow_keys: &[RouteKey], deny_keys: &[RouteKey]) -> Self {
        fn collect(routes: &[&Route], keys: &[RouteKey]) -> Vec<(Method, Vec<Segment>)> {
            routes
                .iter()
                .filter(|route| {
                    let key = RouteKey::from_route(route);
                    keys.contains(&key)
                })
                .map(|route| (route.method, Segment::parse_path(route.uri.path())))
                .collect()
        }

        Self {
            allow: collect(routes, allow_keys),
            deny: collect(routes, deny_keys),
        }
    }

    /// Allow gates, deny subtracts. Empty allow means "everything is eligible".
    pub(crate) fn should_log(&self, method: Method, path: &str) -> bool {
        let matches_in = |set: &[(Method, Vec<Segment>)]| {
            set.iter().any(|(route_method, pattern)| {
                *route_method == method && path_matches(pattern, path)
            })
        };

        let eligible = self.allow.is_empty() || matches_in(&self.allow);
        eligible && !matches_in(&self.deny)
    }
}

/// Cached per-request log decision, stored in `request.local_cache()` so that
/// `on_request` and `on_response` always agree.
#[derive(Clone, Copy)]
pub(crate) struct LogDecision(pub(crate) bool);

#[cfg(test)]
mod tests {
    use super::{path_matches, ResolvedFilter, Segment};
    use rocket::http::Method;

    fn pat(path: &str) -> Vec<Segment> {
        Segment::parse_path(path)
    }

    #[test]
    fn test_parse_path_classifies_segments() {
        assert_eq!(
            pat("/users/<id>/files/<rest..>"),
            vec![
                Segment::Static("users".into()),
                Segment::Dynamic,
                Segment::Static("files".into()),
                Segment::Trailing,
            ],
            "I expect static, dynamic, static, trailing segments"
        );
    }

    #[test]
    fn test_static_path_exact_match() {
        assert!(
            path_matches(&pat("/health"), "/health"),
            "I expect /health to match itself"
        );
        assert!(
            !path_matches(&pat("/health"), "/healthz"),
            "I expect /healthz not to match /health"
        );
        assert!(
            !path_matches(&pat("/health"), "/health/x"),
            "I expect a longer path not to match"
        );
    }

    #[test]
    fn test_dynamic_segment_matches_one() {
        assert!(
            path_matches(&pat("/users/<id>"), "/users/42"),
            "I expect a dynamic segment to match one value"
        );
        assert!(
            !path_matches(&pat("/users/<id>"), "/users"),
            "I expect a missing dynamic segment not to match"
        );
        assert!(
            !path_matches(&pat("/users/<id>"), "/users/42/extra"),
            "I expect an extra segment not to match"
        );
    }

    #[test]
    fn test_trailing_matches_rest_including_none() {
        assert!(
            path_matches(&pat("/files/<rest..>"), "/files"),
            "I expect trailing to match zero segments"
        );
        assert!(
            path_matches(&pat("/files/<rest..>"), "/files/a"),
            "I expect trailing to match one segment"
        );
        assert!(
            path_matches(&pat("/files/<rest..>"), "/files/a/b/c"),
            "I expect trailing to match many segments"
        );
    }

    #[test]
    fn test_root_path_matches() {
        assert!(path_matches(&pat("/"), "/"), "I expect root to match root");
        assert!(
            !path_matches(&pat("/"), "/x"),
            "I expect root not to match a child"
        );
    }

    #[test]
    fn test_decision_truth_table() {
        let only_deny = ResolvedFilter {
            allow: vec![],
            deny: vec![(Method::Get, pat("/health"))],
        };
        assert!(
            only_deny.should_log(Method::Get, "/keep"),
            "I expect a non-denied route to log"
        );
        assert!(
            !only_deny.should_log(Method::Get, "/health"),
            "I expect a denied route not to log"
        );
        assert!(
            only_deny.should_log(Method::Post, "/health"),
            "I expect a different method to log"
        );

        let only_allow = ResolvedFilter {
            allow: vec![(Method::Get, pat("/api"))],
            deny: vec![],
        };
        assert!(
            only_allow.should_log(Method::Get, "/api"),
            "I expect an allowed route to log"
        );
        assert!(
            !only_allow.should_log(Method::Get, "/other"),
            "I expect a non-allowed route not to log"
        );

        let both = ResolvedFilter {
            allow: vec![(Method::Get, pat("/api")), (Method::Get, pat("/admin"))],
            deny: vec![(Method::Get, pat("/admin"))],
        };
        assert!(
            both.should_log(Method::Get, "/api"),
            "I expect allowed-and-not-denied to log"
        );
        assert!(
            !both.should_log(Method::Get, "/admin"),
            "I expect deny to win over allow"
        );

        let neither = ResolvedFilter {
            allow: vec![],
            deny: vec![],
        };
        assert!(
            neither.should_log(Method::Get, "/anything"),
            "I expect everything to log when no lists are set"
        );
    }
}
