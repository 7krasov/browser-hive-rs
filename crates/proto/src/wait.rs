//! Resolution of a request's wait parameters from `WaitOptions` and the deprecated flat fields.
//!
//! Every `WaitOptions` field is proto3 `optional`, so "set to `0`/`""`" and "not set" are
//! distinguishable there, while the flat fields have always read `0`/`""` as "not set". The
//! merge is therefore per field: a field set in `WaitOptions` wins — even when it is empty —
//! and an unset one falls back to its flat counterpart. A request without `wait` resolves to
//! exactly its flat fields, which is what keeps existing clients unaffected.

use crate::{coordinator, worker};

/// Wait parameters after the merge, in the flat fields' own representation
/// (`0` / empty string = not set, the defaults are applied downstream as before).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedWait {
    pub strategy: String,
    pub timeout_ms: u32,
    pub wait_selector: String,
    pub skip_selector: String,
}

fn pick<T: Clone>(set: Option<&T>, flat: &T) -> T {
    set.unwrap_or(flat).clone()
}

// The two request types are generated from two proto packages, hence one impl each.
macro_rules! impl_wait_resolution {
    ($pkg:ident) => {
        impl $pkg::ScrapePageRequest {
            /// The wait parameters this request asks for; see the module docs for the rule.
            #[allow(deprecated)]
            pub fn resolved_wait(&self) -> ResolvedWait {
                let w = self.wait.as_ref();
                ResolvedWait {
                    strategy: pick(w.and_then(|w| w.strategy.as_ref()), &self.wait_strategy),
                    timeout_ms: pick(w.and_then(|w| w.timeout_ms.as_ref()), &self.wait_timeout_ms),
                    wait_selector: pick(
                        w.and_then(|w| w.wait_selector.as_ref()),
                        &self.wait_selector,
                    ),
                    skip_selector: pick(
                        w.and_then(|w| w.skip_selector.as_ref()),
                        &self.skip_selector,
                    ),
                }
            }
        }
    };
}

impl_wait_resolution!(coordinator);
impl_wait_resolution!(worker);

impl From<&ResolvedWait> for worker::WaitOptions {
    /// Every field set, so a worker reading `wait` never falls back to the flat fields.
    fn from(r: &ResolvedWait) -> Self {
        Self {
            strategy: Some(r.strategy.clone()),
            timeout_ms: Some(r.timeout_ms),
            wait_selector: Some(r.wait_selector.clone()),
            skip_selector: Some(r.skip_selector.clone()),
        }
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use coordinator::{ScrapePageRequest, WaitOptions};

    fn flat() -> ScrapePageRequest {
        ScrapePageRequest {
            wait_strategy: "timeout".into(),
            wait_timeout_ms: 5000,
            wait_selector: ".content".into(),
            skip_selector: ".captcha".into(),
            ..Default::default()
        }
    }

    fn flat_resolved() -> ResolvedWait {
        ResolvedWait {
            strategy: "timeout".into(),
            timeout_ms: 5000,
            wait_selector: ".content".into(),
            skip_selector: ".captcha".into(),
        }
    }

    #[test]
    fn without_wait_the_flat_fields_are_used_unchanged() {
        assert_eq!(flat().resolved_wait(), flat_resolved());
        assert_eq!(
            ScrapePageRequest::default().resolved_wait(),
            ResolvedWait::default()
        );
    }

    #[test]
    fn an_empty_wait_falls_back_to_every_flat_field() {
        let req = ScrapePageRequest {
            wait: Some(WaitOptions::default()),
            ..flat()
        };
        assert_eq!(req.resolved_wait(), flat_resolved());
    }

    #[test]
    fn each_set_field_overrides_only_its_own_flat_counterpart() {
        let cases: [(WaitOptions, fn(&mut ResolvedWait)); 4] = [
            (
                WaitOptions {
                    strategy: Some("network_idle".into()),
                    ..Default::default()
                },
                |r| r.strategy = "network_idle".into(),
            ),
            (
                WaitOptions {
                    timeout_ms: Some(9000),
                    ..Default::default()
                },
                |r| r.timeout_ms = 9000,
            ),
            (
                WaitOptions {
                    wait_selector: Some("#x".into()),
                    ..Default::default()
                },
                |r| r.wait_selector = "#x".into(),
            ),
            (
                WaitOptions {
                    skip_selector: Some("#y".into()),
                    ..Default::default()
                },
                |r| r.skip_selector = "#y".into(),
            ),
        ];
        for (options, apply) in cases {
            let mut expected = flat_resolved();
            apply(&mut expected);
            let req = ScrapePageRequest {
                wait: Some(options.clone()),
                ..flat()
            };
            assert_eq!(req.resolved_wait(), expected, "{options:?}");
        }
    }

    #[test]
    fn an_explicit_zero_or_empty_value_overrides_the_flat_field() {
        let req = ScrapePageRequest {
            wait: Some(WaitOptions {
                strategy: Some(String::new()),
                timeout_ms: Some(0),
                wait_selector: Some(String::new()),
                skip_selector: Some(String::new()),
            }),
            ..flat()
        };
        assert_eq!(req.resolved_wait(), ResolvedWait::default());
    }

    #[test]
    fn the_worker_request_resolves_by_the_same_rule() {
        let req = worker::ScrapePageRequest {
            wait_timeout_ms: 5000,
            wait_selector: ".content".into(),
            wait: Some(worker::WaitOptions {
                wait_selector: Some(String::new()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let r = req.resolved_wait();
        assert_eq!((r.timeout_ms, r.wait_selector.as_str()), (5000, ""));
    }

    #[test]
    fn forwarded_wait_options_resolve_to_the_same_values() {
        // A worker request carrying the forwarded `wait` resolves to it whatever its flat fields say.
        let resolved = flat_resolved();
        let req = worker::ScrapePageRequest {
            wait: Some((&resolved).into()),
            ..Default::default()
        };
        assert_eq!(req.resolved_wait(), resolved);
    }
}
