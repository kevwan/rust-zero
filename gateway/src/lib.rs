//! Gateway route selection with longest-prefix matching and round-robin upstream pools.

use std::sync::atomic::{AtomicUsize, Ordering};

/// A configured HTTP path prefix and its upstream endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayRoute {
    pub prefix: String,
    pub upstreams: Vec<String>,
}

impl GatewayRoute {
    pub fn new(prefix: impl Into<String>, upstreams: Vec<String>) -> Result<Self, GatewayError> {
        let prefix = normalize_prefix(prefix.into())?;
        if upstreams.is_empty() {
            return Err(GatewayError::EmptyUpstreams(prefix));
        }
        if upstreams.iter().any(|upstream| upstream.is_empty()) {
            return Err(GatewayError::EmptyUpstream);
        }

        Ok(Self { prefix, upstreams })
    }
}

struct RoutePool {
    route: GatewayRoute,
    next_upstream: AtomicUsize,
}

/// Selects the most specific route and distributes requests across its upstreams.
pub struct GatewayRouter {
    routes: Vec<RoutePool>,
}

impl GatewayRouter {
    pub fn new(routes: impl IntoIterator<Item = GatewayRoute>) -> Result<Self, GatewayError> {
        let mut routes: Vec<_> = routes
            .into_iter()
            .map(|route| RoutePool {
                route,
                next_upstream: AtomicUsize::new(0),
            })
            .collect();

        routes
            .sort_unstable_by(|left, right| right.route.prefix.len().cmp(&left.route.prefix.len()));
        for routes_with_same_prefix in routes.windows(2) {
            if routes_with_same_prefix[0].route.prefix == routes_with_same_prefix[1].route.prefix {
                return Err(GatewayError::DuplicatePrefix(
                    routes_with_same_prefix[0].route.prefix.clone(),
                ));
            }
        }

        Ok(Self { routes })
    }

    /// Selects an upstream for a request path, using round robin within its matched route.
    pub fn select(&self, path: &str) -> Option<&str> {
        self.routes
            .iter()
            .find(|route| matches_prefix(path, &route.route.prefix))
            .map(|route| {
                let index = route.next_upstream.fetch_add(1, Ordering::Relaxed)
                    % route.route.upstreams.len();
                route.route.upstreams[index].as_str()
            })
    }
}

/// Errors produced by invalid gateway routing configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayError {
    InvalidPrefix(String),
    EmptyUpstreams(String),
    EmptyUpstream,
    DuplicatePrefix(String),
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPrefix(prefix) => {
                write!(formatter, "gateway prefix must start with '/': {prefix}")
            }
            Self::EmptyUpstreams(prefix) => {
                write!(formatter, "gateway route {prefix} has no upstreams")
            }
            Self::EmptyUpstream => formatter.write_str("gateway upstream cannot be empty"),
            Self::DuplicatePrefix(prefix) => {
                write!(formatter, "duplicate gateway prefix: {prefix}")
            }
        }
    }
}

impl std::error::Error for GatewayError {}

fn normalize_prefix(mut prefix: String) -> Result<String, GatewayError> {
    if !prefix.starts_with('/') {
        return Err(GatewayError::InvalidPrefix(prefix));
    }
    while prefix.len() > 1 && prefix.ends_with('/') {
        prefix.pop();
    }
    Ok(prefix)
}

fn matches_prefix(path: &str, prefix: &str) -> bool {
    prefix == "/"
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(prefix: &str, upstreams: &[&str]) -> GatewayRoute {
        GatewayRoute::new(
            prefix,
            upstreams
                .iter()
                .map(|upstream| (*upstream).to_owned())
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn selects_the_most_specific_matching_prefix() {
        let router = GatewayRouter::new([
            route("/", &["http://home"]),
            route("/api", &["http://api"]),
            route("/api/admin", &["http://admin"]),
        ])
        .unwrap();

        assert_eq!(router.select("/api/admin/users"), Some("http://admin"));
        assert_eq!(router.select("/api/users"), Some("http://api"));
        assert_eq!(router.select("/apix"), Some("http://home"));
    }

    #[test]
    fn rotates_through_matched_route_upstreams() {
        let router = GatewayRouter::new([route("/api", &["http://one", "http://two"])]).unwrap();

        assert_eq!(router.select("/api/items"), Some("http://one"));
        assert_eq!(router.select("/api/items"), Some("http://two"));
        assert_eq!(router.select("/api/items"), Some("http://one"));
    }

    #[test]
    fn rejects_invalid_routes() {
        assert_eq!(
            GatewayRoute::new("api", vec!["http://api".to_owned()]).unwrap_err(),
            GatewayError::InvalidPrefix("api".to_owned())
        );
        assert_eq!(
            GatewayRoute::new("/api", Vec::new()).unwrap_err(),
            GatewayError::EmptyUpstreams("/api".to_owned())
        );
    }
}
