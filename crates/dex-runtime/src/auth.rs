//! Capability authority.
//!
//! Authority is **deny by default**. A capability call runs only when a grant
//! exists for its name *and* the resource it wants matches the grant's scope.
//! Both checks happen here, in Rust, below the script runtime, so a generated
//! program has no way to widen its own permissions: it can only ask, and the
//! answer is already fixed by configuration.
//!
//! Authorization is deliberately separate from the path guard. The path guard
//! answers "is this path inside the session working directory"; the authorizer
//! answers "is this session permitted to touch it at all". Both must pass.

use std::collections::BTreeMap;
use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};

/// One granted permission: a capability name and the resources it covers.
#[derive(Clone, Debug)]
pub struct Grant {
    pub capability: String,
    pub scope: ScopeMatcher,
}

/// The set of permissions a runtime holds.
#[derive(Clone, Debug, Default)]
pub struct Authority {
    grants: BTreeMap<String, ScopeMatcher>,
}

impl Authority {
    /// Parse the `DEX_AUTHORITY` value.
    ///
    /// The format is `capability=scope`, entries separated by `;`. A scope of
    /// `*` matches any resource. Anything else is a glob matched against the
    /// canonical resource string. An empty or absent value yields an authority
    /// that grants nothing, which is the intended default.
    pub fn parse(spec: &str) -> Result<Self, AuthorityParseError> {
        let mut grants: BTreeMap<String, ScopeMatcher> = BTreeMap::new();
        for raw in spec.split(';') {
            let entry = raw.trim();
            if entry.is_empty() {
                continue;
            }
            let (capability, scope) = entry.split_once('=').ok_or(AuthorityParseError::NoScope {
                entry: entry.to_string(),
            })?;
            let capability = capability.trim();
            if capability.is_empty() {
                return Err(AuthorityParseError::EmptyCapability {
                    entry: entry.to_string(),
                });
            }
            let matcher = ScopeMatcher::new(scope.trim())?;
            // A repeated capability widens rather than replaces, so two grants
            // for `filesystem.read` union instead of the later one silently
            // narrowing the earlier one.
            match grants.get_mut(capability) {
                Some(existing) => existing.union(matcher),
                None => {
                    grants.insert(capability.to_string(), matcher);
                }
            }
        }
        Ok(Self { grants })
    }

    /// An authority that grants nothing.
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// True when `capability` has at least one grant. Does not consider the
    /// resource; use [`Authority::check`] for the real decision.
    pub fn grants(&self, capability: &str) -> bool {
        self.grants.contains_key(capability)
    }

    /// The decision. `resource` is the canonical string the capability actually
    /// intends to touch: a canonical absolute path for filesystem work, or the
    /// working directory for something like the test runner.
    pub fn check(&self, capability: &str, resource: &str) -> Result<(), Denied> {
        let Some(matcher) = self.grants.get(capability) else {
            return Err(Denied {
                capability: capability.to_string(),
                resource: resource.to_string(),
                reason: DenyReason::NoGrant,
            });
        };
        if matcher.matches(resource) {
            Ok(())
        } else {
            Err(Denied {
                capability: capability.to_string(),
                resource: resource.to_string(),
                reason: DenyReason::ScopeMismatch,
            })
        }
    }

    /// Convenience for path-shaped resources.
    pub fn check_path(&self, capability: &str, path: &Path) -> Result<(), Denied> {
        self.check(capability, &path.to_string_lossy())
    }

    /// Every grant, for `ListCapabilities` and the CLI banner.
    pub fn describe(&self) -> Vec<dex_protocol::GrantedAuthority> {
        self.grants
            .iter()
            .map(|(capability, scope)| dex_protocol::GrantedAuthority {
                capability: capability.clone(),
                scope: scope.describe(),
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DenyReason {
    /// No grant names this capability at all.
    NoGrant,
    /// A grant exists but does not cover this resource.
    ScopeMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denied {
    pub capability: String,
    pub resource: String,
    pub reason: DenyReason,
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason {
            DenyReason::NoGrant => write!(
                f,
                "no authority granted for `{}` (requested on {})",
                self.capability, self.resource
            ),
            DenyReason::ScopeMismatch => write!(
                f,
                "authority for `{}` does not cover {}",
                self.capability, self.resource
            ),
        }
    }
}

impl std::error::Error for Denied {}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AuthorityParseError {
    #[error("entry {entry:?} has no `=scope`; expected `capability=scope`")]
    NoScope { entry: String },
    #[error("entry {entry:?} has an empty capability name")]
    EmptyCapability { entry: String },
    #[error("scope {scope:?} is not a valid glob: {source}")]
    BadGlob {
        scope: String,
        #[source]
        source: globset::Error,
    },
}

/// The resource patterns one grant covers.
#[derive(Clone, Debug)]
pub struct ScopeMatcher {
    /// `*` — any resource.
    any: bool,
    set: Option<GlobSet>,
    /// The original patterns, kept so `describe` can show the operator what was
    /// actually configured rather than a compiled blob.
    patterns: Vec<String>,
}

impl ScopeMatcher {
    pub fn new(scope: &str) -> Result<Self, AuthorityParseError> {
        if scope == "*" || scope.is_empty() {
            return Ok(Self {
                any: true,
                set: None,
                patterns: vec!["*".to_string()],
            });
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in scope.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let glob = Glob::new(pattern).map_err(|source| AuthorityParseError::BadGlob {
                scope: pattern.to_string(),
                source,
            })?;
            builder.add(glob);
            // `/work/**` is what people write when they mean "everything under
            // /work", and they expect the directory itself to match too. Plain
            // globset requires a trailing component, so accept both.
            if let Some(prefix) = pattern.strip_suffix("/**") {
                if let Ok(glob) = Glob::new(prefix) {
                    builder.add(glob);
                }
            }
        }
        let set = builder
            .build()
            .map_err(|source| AuthorityParseError::BadGlob {
                scope: scope.to_string(),
                source,
            })?;
        Ok(Self {
            any: false,
            set: Some(set),
            patterns: scope.split(',').map(|p| p.trim().to_string()).collect(),
        })
    }

    pub fn matches(&self, resource: &str) -> bool {
        if self.any {
            return true;
        }
        self.set
            .as_ref()
            .is_some_and(|set| set.is_match(resource))
    }

    fn union(&mut self, other: ScopeMatcher) {
        if other.any {
            self.any = true;
            self.set = None;
            return;
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in &self.patterns {
            if let Ok(glob) = Glob::new(pattern) {
                builder.add(glob);
            }
            if let Some(prefix) = pattern.strip_suffix("/**") {
                if let Ok(glob) = Glob::new(prefix) {
                    builder.add(glob);
                }
            }
        }
        for pattern in &other.patterns {
            self.patterns.push(pattern.clone());
            if let Ok(glob) = Glob::new(pattern) {
                builder.add(glob);
            }
            if let Some(prefix) = pattern.strip_suffix("/**") {
                if let Ok(glob) = Glob::new(prefix) {
                    builder.add(glob);
                }
            }
        }
        if let Ok(set) = builder.build() {
            self.set = Some(set);
        }
    }

    pub fn describe(&self) -> String {
        self.patterns.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_spec_grants_nothing() {
        let authority = Authority::parse("").expect("parse");
        let denied = authority.check("filesystem.read", "/work/a.rs").expect_err("denied");
        assert_eq!(denied.reason, DenyReason::NoGrant);
    }

    #[test]
    fn a_missing_authority_is_denied_by_default() {
        let authority = Authority::parse("filesystem.read=/work/**").expect("parse");
        assert!(authority.check("filesystem.write", "/work/a.rs").is_err());
        assert!(authority.check("filesystem.read", "/work/a.rs").is_ok());
    }

    #[test]
    fn scope_is_matched_against_the_resource() {
        let authority = Authority::parse("filesystem.read=/work/**").expect("parse");
        assert!(authority.check("filesystem.read", "/work/src/a.rs").is_ok());
        assert_eq!(
            authority.check("filesystem.read", "/etc/passwd").unwrap_err().reason,
            DenyReason::ScopeMismatch
        );
    }

    #[test]
    fn a_recursive_scope_also_matches_the_directory_itself() {
        let authority = Authority::parse("filesystem.read=/work/**").expect("parse");
        assert!(authority.check("filesystem.read", "/work").is_ok());
    }

    #[test]
    fn a_star_scope_matches_anything() {
        let authority = Authority::parse("memory.read=*").expect("parse");
        assert!(authority.check("memory.read", "anything/at/all").is_ok());
    }

    #[test]
    fn repeated_capabilities_widen_rather_than_replace() {
        let authority = Authority::parse(
            "filesystem.read=/work/**;filesystem.read=/srv/shared/**",
        )
        .expect("parse");
        assert!(authority.check("filesystem.read", "/work/a.rs").is_ok());
        assert!(authority.check("filesystem.read", "/srv/shared/a.rs").is_ok());
        assert!(authority.check("filesystem.read", "/etc/passwd").is_err());
    }

    #[test]
    fn a_missing_scope_is_a_parse_error() {
        assert!(matches!(
            Authority::parse("filesystem.read").expect_err("must fail"),
            AuthorityParseError::NoScope { .. }
        ));
    }

    #[test]
    fn describe_reports_every_grant() {
        let authority =
            Authority::parse("filesystem.read=/work/**;memory.write=*").expect("parse");
        let described = authority.describe();
        assert_eq!(described.len(), 2);
        assert!(described
            .iter()
            .any(|g| g.capability == "filesystem.read" && g.scope == "/work/**"));
        assert!(described.iter().any(|g| g.capability == "memory.write" && g.scope == "*"));
    }
}
