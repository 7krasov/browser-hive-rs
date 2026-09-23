//! Chromium feature switches assembled from several sources into one.
//!
//! Chromium keeps only the **last** `--disable-features=` switch on its command line (measured
//! 2026-09-23, Chrome 153 and Brave: `--disable-features=BackForwardCache` followed by a second
//! `--disable-features=…` left the back/forward cache on). Three parties add one here: headless_chrome's
//! `DEFAULT_ARGS`, the binary params middlewares and the pool's own options (see
//! `ScopeConfig::disable_back_forward_cache`). Appending would silently cancel whichever came
//! first, so every value is merged into a single switch instead.
//!
//! `--enable-features=` has the same property and is **not** merged here: a middleware's
//! `--enable-features=TabDiscarding` already overrides headless_chrome's
//! `--enable-features=NetworkService,NetworkServiceInProcess`, and merging would move the network
//! service into the browser process — a behaviour change of its own. See TODO.md.

use std::ffi::OsStr;

const DISABLE_FEATURES: &str = "--disable-features=";

/// Feature list of a `--disable-features=` switch, `None` for any other argument.
fn disabled_features(arg: &str) -> Option<impl Iterator<Item = &str>> {
    arg.strip_prefix(DISABLE_FEATURES)
        .map(|list| list.split(',').map(str::trim).filter(|f| !f.is_empty()))
}

/// Launch arguments with every `--disable-features=` switch folded into one.
pub(crate) struct MergedArgs<'a> {
    /// Middleware arguments without their `--disable-features=` switches.
    pub args: Vec<&'a OsStr>,
    /// The single merged switch, `None` when no source beyond headless_chrome asked for one
    /// (then its own default switch stays in force, unchanged).
    pub disable_features: Option<String>,
    /// headless_chrome defaults that the merged switch replaces; goes to `ignore_default_args`.
    pub ignored_defaults: Vec<&'static str>,
}

/// Merge `crate_defaults` (headless_chrome's `DEFAULT_ARGS`), the middleware `args` and the
/// `extra` features into one `--disable-features=` switch, in that order, without duplicates.
pub(crate) fn merge_disable_features<'a>(
    crate_defaults: &[&'static str],
    args: Vec<&'a OsStr>,
    extra: &[&str],
) -> MergedArgs<'a> {
    let mut features: Vec<String> = Vec::new();
    let mut add = |f: &str| {
        if !features.iter().any(|known| known == f) {
            features.push(f.to_string());
        }
    };

    let ignored_defaults: Vec<&'static str> = crate_defaults
        .iter()
        .copied()
        .filter(|arg| disabled_features(arg).is_some())
        .collect();
    for list in ignored_defaults
        .iter()
        .filter_map(|arg| disabled_features(arg))
    {
        list.for_each(&mut add);
    }

    let mut own_sources = 0;
    let mut kept = Vec::with_capacity(args.len());
    for arg in args {
        match arg.to_str().and_then(disabled_features) {
            Some(list) => {
                own_sources += 1;
                list.for_each(&mut add);
            }
            None => kept.push(arg),
        }
    }
    extra.iter().for_each(|f| add(f));

    if own_sources == 0 && extra.is_empty() {
        return MergedArgs {
            args: kept,
            disable_features: None,
            ignored_defaults: Vec::new(),
        };
    }
    MergedArgs {
        args: kept,
        disable_features: Some(format!("{DISABLE_FEATURES}{}", features.join(","))),
        ignored_defaults,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULTS: &[&str] = &[
        "--disable-sync",
        "--disable-features=TranslateUI,BlinkGenPropertyTrees",
    ];

    fn os(args: &[&'static str]) -> Vec<&'static OsStr> {
        args.iter().map(|a| OsStr::new(*a)).collect()
    }

    #[test]
    fn nothing_to_merge_leaves_the_crate_default_alone() {
        let merged = merge_disable_features(DEFAULTS, os(&["--mute-audio"]), &[]);
        assert_eq!(merged.args, os(&["--mute-audio"]));
        assert_eq!(merged.disable_features, None);
        assert!(merged.ignored_defaults.is_empty());
    }

    #[test]
    fn extra_feature_keeps_the_crate_defaults() {
        let merged = merge_disable_features(DEFAULTS, os(&[]), &["BackForwardCache"]);
        assert_eq!(
            merged.disable_features.as_deref(),
            Some("--disable-features=TranslateUI,BlinkGenPropertyTrees,BackForwardCache")
        );
        assert_eq!(
            merged.ignored_defaults,
            vec!["--disable-features=TranslateUI,BlinkGenPropertyTrees"]
        );
    }

    #[test]
    fn middleware_switches_are_folded_in_and_deduplicated() {
        let merged = merge_disable_features(
            DEFAULTS,
            os(&[
                "--mute-audio",
                "--disable-features=IsolateOrigins,site-per-process",
                "--disable-features=TranslateUI, ,BackForwardCache",
            ]),
            &["BackForwardCache"],
        );
        assert_eq!(merged.args, os(&["--mute-audio"]));
        assert_eq!(
            merged.disable_features.as_deref(),
            Some(
                "--disable-features=TranslateUI,BlinkGenPropertyTrees,IsolateOrigins,\
                 site-per-process,BackForwardCache"
            )
        );
    }

    #[test]
    fn crate_without_a_default_switch_ignores_nothing() {
        let merged = merge_disable_features(&["--disable-sync"], os(&[]), &["BackForwardCache"]);
        assert_eq!(
            merged.disable_features.as_deref(),
            Some("--disable-features=BackForwardCache")
        );
        assert!(merged.ignored_defaults.is_empty());
    }
}
