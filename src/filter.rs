use rocket::http::Method;
use rocket::Route;

/// A mount-independent identity for a route, derived from a `Route` produced by
/// `rocket::routes![...]`. Method, name, and the unmounted path are all
/// preserved when a route is mounted at a base, so the same key is computed
/// before and after mounting. This is what lets us correlate a listed route to
/// its live, mounted entry without the developer repeating the mount base.
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
    /// `<name..>`: matches zero or more remaining request segments.
    /// Ends a pattern.
    Trailing,
}

impl Segment {
    /// Parse route paths like `/users/<id>` or `/files/<rest..>` into segments.
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

/// How a route pattern relates to a single filter pattern.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Coverage {
    /// Every concrete request to the route matches the filter pattern.
    Full,
    /// Only some concrete requests to the route match — the route's dynamic or
    /// trailing segments make it request-dependent.
    Partial,
    /// No concrete request to the route can match the filter pattern.
    None,
}

/// Does `filter` cover requests dispatched to `route`, fully, partially, or not
/// at all? Both are route patterns. This is the pattern-level analogue of
/// `path_matches` used to describe (not decide) a route's logging at launch.
pub(crate) fn coverage(filter: &[Segment], route: &[Segment]) -> Coverage {
    let mut partial = false;
    let mut index = 0;

    for filter_segment in filter {
        match filter_segment {
            // A trailing filter segment swallows everything left.
            Segment::Trailing => {
                return if partial {
                    Coverage::Partial
                } else {
                    Coverage::Full
                };
            }
            Segment::Dynamic => {
                if index >= route.len() {
                    return Coverage::None;
                }
                if route[index] == Segment::Trailing {
                    // Trailing route can expand to zero or many; one dynamic slot
                    // matches some expansions but not others.
                    return Coverage::Partial;
                }
                index += 1;
            }
            Segment::Static(expected) => {
                if index >= route.len() {
                    return Coverage::None;
                }
                match &route[index] {
                    Segment::Static(actual) => {
                        if actual != expected {
                            return Coverage::None;
                        }
                    }
                    // A dynamic route segment equals this literal only sometimes.
                    Segment::Dynamic => partial = true,
                    // A trailing route segment can produce this literal or not.
                    Segment::Trailing => return Coverage::Partial,
                }
                index += 1;
            }
        }
    }

    if index == route.len() {
        return if partial {
            Coverage::Partial
        } else {
            Coverage::Full
        };
    }

    // Filter consumed but route has leftovers. A lone trailing leftover can
    // expand to match the filter length or overshoot it -> request-dependent.
    if index == route.len() - 1 && route[index] == Segment::Trailing {
        return Coverage::Partial;
    }

    Coverage::None
}

/// A route's auto-logging status at launch, as reported on `Route Registered`.
/// Runtime decisions stay boolean; this tri-state exists because a route is a
/// pattern and pattern overlap can make logging request-dependent.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AutoLog {
    /// Every request to this route is logged.
    Always,
    /// No request to this route is logged.
    Never,
    /// Request-dependent: this route partially overlaps the named filter
    /// patterns, so some requests log and others don't.
    Conditional { overlaps: Vec<String> },
}

impl AutoLog {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            AutoLog::Always => "always",
            AutoLog::Never => "never",
            AutoLog::Conditional { .. } => "conditional",
        }
    }

    /// The overlapping patterns to log alongside, only for `Conditional`.
    pub(crate) fn overlaps_field(&self) -> Option<String> {
        match self {
            AutoLog::Conditional { overlaps } => Some(overlaps.join(", ")),
            _ => None,
        }
    }
}

/// One resolved filter pattern: its method, a display string for the
/// `auto_log_overlaps` field (e.g. `/users/<id>`), and the parsed segments used
/// for matching.
pub(crate) struct FilterPattern {
    pub(crate) method: Method,
    pub(crate) display: String,
    pub(crate) segments: Vec<Segment>,
}

/// The show/skip lists resolved to concrete mounted path patterns. Built once
/// from the live route table.
///
/// Matching is pattern-based and leaky: a request is filtered by which pattern
/// it matches, not by which route handles it. Overlapping patterns therefore
/// make a route's logging request-dependent — see [`ResolvedFilter::classify`]
/// and the [`AutoLog::Conditional`] status reported at launch.
pub(crate) struct ResolvedFilter {
    pub(crate) shown: Vec<FilterPattern>,
    pub(crate) skipped: Vec<FilterPattern>,
}

impl ResolvedFilter {
    pub(crate) fn resolve(
        routes: &[&Route],
        show_keys: &[RouteKey],
        skip_keys: &[RouteKey],
    ) -> Self {
        fn collect(routes: &[&Route], keys: &[RouteKey]) -> Vec<FilterPattern> {
            routes
                .iter()
                .filter(|route| {
                    let key = RouteKey::from_route(route);
                    keys.contains(&key)
                })
                .map(|route| FilterPattern {
                    method: route.method,
                    display: route.uri.path().to_string(),
                    segments: Segment::parse_path(route.uri.path()),
                })
                .collect()
        }

        Self {
            shown: collect(routes, show_keys),
            skipped: collect(routes, skip_keys),
        }
    }

    /// The show list gates, the skip list subtracts. An empty show list means
    /// "everything is eligible".
    pub(crate) fn should_log(&self, method: Method, path: &str) -> bool {
        let matches_in = |set: &[FilterPattern]| {
            set.iter()
                .any(|pattern| pattern.method == method && path_matches(&pattern.segments, path))
        };

        let eligible = self.shown.is_empty() || matches_in(&self.shown);
        eligible && !matches_in(&self.skipped)
    }

    /// Describe (not decide) a registered route's auto-logging, matching the
    /// same show-gates/skip-subtracts logic as `should_log` but at the pattern
    /// level. Overlap makes the answer request-dependent -> `Conditional`.
    ///
    /// `Conditional` is the deliberately uncertain bucket: this is a descriptive
    /// launch hint, and runtime decisions ([`ResolvedFilter::should_log`]) stay
    /// exact regardless. The classification is safe-directional — it never
    /// reports `Always` for a route any request could skip, nor `Never` for a
    /// route any request could log. It has one known imprecision in the other,
    /// harmless direction: when a show pattern and a skip pattern both *partially*
    /// overlap the same dynamic route, a route that is in truth never logged is
    /// reported `Conditional` rather than `Never`. Resolving that would require
    /// modelling the show-admitted request set before subtracting skips; for a
    /// launch hint it is intentionally left as the conservative `Conditional`.
    pub(crate) fn classify(&self, method: Method, route: &[Segment]) -> AutoLog {
        // Reduce a set into (any full cover, any partial cover, partial patterns).
        let reduce = |set: &[FilterPattern]| {
            let mut full = false;
            let mut partial = false;
            let mut overlaps = Vec::new();
            for pattern in set {
                if pattern.method != method {
                    continue;
                }
                match coverage(&pattern.segments, route) {
                    Coverage::Full => full = true,
                    Coverage::Partial => {
                        partial = true;
                        overlaps.push(format!("{} {}", pattern.method, pattern.display));
                    }
                    Coverage::None => {}
                }
            }
            (full, partial, overlaps)
        };

        // Eligibility via the show gate: an empty show list means definitely eligible.
        let (eligible_definite, eligible_possible, show_overlaps) = if self.shown.is_empty() {
            (true, true, Vec::new())
        } else {
            let (full, partial, overlaps) = reduce(&self.shown);
            (full, full || partial, overlaps)
        };

        let (skip_full, skip_partial, skip_overlaps) = reduce(&self.skipped);

        if !eligible_possible || skip_full {
            return AutoLog::Never;
        }
        if eligible_definite && !skip_partial {
            return AutoLog::Always;
        }

        let mut overlaps = skip_overlaps;
        if !eligible_definite {
            overlaps.extend(show_overlaps);
        }
        // A show and a skip pattern can name the same overlap; keep each once.
        let mut seen = std::collections::HashSet::new();
        overlaps.retain(|pattern| seen.insert(pattern.clone()));
        AutoLog::Conditional { overlaps }
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

    fn fp(method: Method, path: &str) -> super::FilterPattern {
        super::FilterPattern {
            method,
            display: path.to_string(),
            segments: pat(path),
        }
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
            shown: vec![],
            skipped: vec![fp(Method::Get, "/health")],
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
            shown: vec![fp(Method::Get, "/api")],
            skipped: vec![],
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
            shown: vec![fp(Method::Get, "/api"), fp(Method::Get, "/admin")],
            skipped: vec![fp(Method::Get, "/admin")],
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
            shown: vec![],
            skipped: vec![],
        };
        assert!(
            neither.should_log(Method::Get, "/anything"),
            "I expect everything to log when no lists are set"
        );
    }

    #[test]
    fn test_coverage_classifies_overlap() {
        use super::{coverage, Coverage};

        // exact static match -> fully covered
        assert_eq!(coverage(&pat("/health"), &pat("/health")), Coverage::Full);
        // disjoint statics -> no overlap
        assert_eq!(coverage(&pat("/health"), &pat("/metrics")), Coverage::None);
        // filter dynamic matches a route static -> full
        assert_eq!(
            coverage(&pat("/users/<id>"), &pat("/users/<id>")),
            Coverage::Full
        );
        // route dynamic vs filter static -> only one value caught -> partial
        assert_eq!(
            coverage(&pat("/users/admin"), &pat("/users/<id>")),
            Coverage::Partial
        );
        // filter trailing covers the rest -> full
        assert_eq!(
            coverage(&pat("/files/<rest..>"), &pat("/files/a/b")),
            Coverage::Full
        );
        // route trailing vs shorter static filter -> request-dependent -> partial
        assert_eq!(
            coverage(&pat("/files"), &pat("/files/<rest..>")),
            Coverage::Partial
        );
        // route longer than a non-trailing filter -> no match
        assert_eq!(
            coverage(&pat("/files"), &pat("/files/logs")),
            Coverage::None
        );
    }

    #[test]
    fn test_classify_auto_log_states() {
        use super::AutoLog;

        // No filter at all -> always logged.
        let none = ResolvedFilter {
            shown: vec![],
            skipped: vec![],
        };
        assert_eq!(none.classify(Method::Get, &pat("/keep")), AutoLog::Always);

        // Fully skipped -> never.
        let skip = ResolvedFilter {
            shown: vec![],
            skipped: vec![fp(Method::Get, "/users/admin")],
        };
        assert_eq!(
            skip.classify(Method::Get, &pat("/users/admin")),
            AutoLog::Never
        );

        // Outside a non-empty show list -> never.
        let show = ResolvedFilter {
            shown: vec![fp(Method::Get, "/api")],
            skipped: vec![],
        };
        assert_eq!(show.classify(Method::Get, &pat("/other")), AutoLog::Never);

        // Dynamic route partially overlapping a skipped static -> conditional,
        // naming the pattern it may be interpreted as.
        let overlap = ResolvedFilter {
            shown: vec![],
            skipped: vec![fp(Method::Get, "/users/admin")],
        };
        assert_eq!(
            overlap.classify(Method::Get, &pat("/users/<id>")),
            AutoLog::Conditional {
                overlaps: vec!["GET /users/admin".to_string()]
            },
        );

        // Trailing route over a skipped sub-path -> conditional.
        let trailing = ResolvedFilter {
            shown: vec![],
            skipped: vec![fp(Method::Get, "/files/secret")],
        };
        assert_eq!(
            trailing.classify(Method::Get, &pat("/files/<rest..>")),
            AutoLog::Conditional {
                overlaps: vec!["GET /files/secret".to_string()]
            },
        );
    }

    #[test]
    fn test_classify_show_skip_overlap_reports_conditional() {
        use super::AutoLog;

        // Known, intentional imprecision (pinned): when a show pattern and a
        // skip pattern both partially overlap the same dynamic route, the route
        // is in truth never logged — the only eligible request (`/a`) is also
        // the one skipped — yet `classify` conservatively reports `Conditional`.
        // This is safe-directional (it never claims a skipped route logs) and is
        // documented on `classify`. Overlaps are deduplicated, so the shared
        // `GET /a` pattern appears once rather than twice.
        let overlap = ResolvedFilter {
            shown: vec![fp(Method::Get, "/a")],
            skipped: vec![fp(Method::Get, "/a")],
        };
        assert_eq!(
            overlap.classify(Method::Get, &pat("/<x>")),
            AutoLog::Conditional {
                overlaps: vec!["GET /a".to_string()]
            },
        );
    }
}
