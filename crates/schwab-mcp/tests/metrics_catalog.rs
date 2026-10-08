//! Every metric the workspace emits is declared once, in
//! `schwab_rs::metrics` or `schwab_mcp::metrics`, so operators have a single
//! place to read names, kinds and help text. This test keeps the sources
//! honest against those catalogs:
//!
//! * a `metrics::counter!` / `gauge!` / `histogram!` call site must name a
//!   catalog constant, never a string literal, and the instrument must match
//!   the catalog kind;
//! * every `pub const NAME: &str = "schwab_…"` in a catalog module has a
//!   [`Metric`] entry, and the module docs list it.

use schwab_rs::metrics::{Metric, MetricKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The crates whose sources are audited, with their catalog modules.
fn crates() -> Vec<(PathBuf, &'static [Metric])> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    vec![
        (root.join("schwab-rs"), schwab_rs::metrics::CATALOG),
        (root.join("schwab-mcp"), schwab_mcp::metrics::CATALOG),
    ]
}

fn sources(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push((path.clone(), std::fs::read_to_string(&path).unwrap()));
        }
    }
}

/// `pub const IDENT: &str = "name";` declarations, ident → name.
fn constants(catalog_source: &str) -> HashMap<&str, &str> {
    catalog_source
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("pub const ")?;
            let (ident, rest) = rest.split_once(": &str = \"")?;
            Some((ident, rest.strip_suffix("\";")?))
        })
        .collect()
}

/// Each `metrics::<instrument>!(<first argument>` in `source`, skipping
/// comment lines.
fn call_sites(source: &str) -> Vec<(MetricKind, &str)> {
    const INSTRUMENTS: [(&str, MetricKind); 3] = [
        ("metrics::counter!(", MetricKind::Counter),
        ("metrics::gauge!(", MetricKind::Gauge),
        ("metrics::histogram!(", MetricKind::Histogram),
    ];
    let mut sites = Vec::new();
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        if !line.trim_start().starts_with("//") {
            for (needle, kind) in INSTRUMENTS {
                for (at, _) in line.match_indices(needle) {
                    // The argument may sit on the next line.
                    let rest = source[offset + at + needle.len()..].trim_start();
                    let end = rest
                        .find(|c: char| matches!(c, ',' | ')') || c.is_whitespace())
                        .unwrap_or(rest.len());
                    sites.push((kind, &rest[..end]));
                }
            }
        }
        offset += line.len();
    }
    sites
}

#[test]
fn call_sites_use_catalog_constants_of_the_right_kind() {
    let crates = crates();
    let catalog_sources: Vec<String> = crates
        .iter()
        .map(|(dir, _)| std::fs::read_to_string(dir.join("src/metrics.rs")).unwrap())
        .collect();
    let idents: HashMap<&str, &str> = catalog_sources
        .iter()
        .flat_map(|source| constants(source))
        .collect();
    let mut catalog: HashMap<&str, &Metric> = HashMap::new();
    let mut files = Vec::new();
    for (dir, entries) in &crates {
        for metric in *entries {
            assert!(
                catalog.insert(metric.name, metric).is_none(),
                "{} is catalogued twice",
                metric.name
            );
        }
        sources(&dir.join("src"), &mut files);
    }

    let mut audited = 0;
    for (path, source) in &files {
        for (kind, argument) in call_sites(source) {
            let ident = argument.rsplit("::").next().unwrap();
            let name = idents.get(ident).unwrap_or_else(|| {
                panic!("{}: `{argument}` is not a catalog constant", path.display())
            });
            let metric = catalog
                .get(name)
                .unwrap_or_else(|| panic!("{}: {name} has no CATALOG entry", path.display()));
            assert_eq!(
                metric.kind,
                kind,
                "{}: {name} is recorded as a {kind:?} but catalogued as a {:?}",
                path.display(),
                metric.kind
            );
            audited += 1;
        }
    }
    assert!(
        audited > 20,
        "only {audited} call sites found; scanner broken?"
    );
}

#[test]
fn catalog_constants_are_catalogued_and_documented() {
    for (dir, entries) in crates() {
        let path = dir.join("src/metrics.rs");
        let source = std::fs::read_to_string(&path).unwrap();
        let names: Vec<&str> = entries.iter().map(|m| m.name).collect();
        for (ident, name) in constants(&source) {
            assert!(
                names.contains(&name),
                "{}: {ident} has no CATALOG entry",
                path.display()
            );
        }
        for metric in entries {
            assert!(
                metric.name.starts_with("schwab_"),
                "{} is outside the schwab_ namespace",
                metric.name
            );
            let row = format!("//! | `{}` |", metric.name);
            assert!(
                source.contains(&row),
                "{}: {} is missing from the module doc table",
                path.display(),
                metric.name
            );
            let documented_kind = source
                .lines()
                .find(|line| line.starts_with(&row))
                .and_then(|line| line.split('|').nth(2))
                .map(str::trim);
            let expected = match metric.kind {
                MetricKind::Counter => "counter",
                MetricKind::Gauge => "gauge",
                MetricKind::Histogram => "histogram",
            };
            assert_eq!(
                documented_kind,
                Some(expected),
                "{} doc table kind",
                metric.name
            );
        }
    }
}

#[test]
fn describe_is_a_no_op_without_a_recorder() {
    schwab_mcp::metrics::describe();
}

/// Every metric the shipped alert rules and dashboard query exists in a
/// catalog, so a rename cannot silently blind them.
#[test]
fn alerts_and_dashboard_query_only_declared_metrics() {
    let declared: std::collections::HashSet<&str> = crates()
        .iter()
        .flat_map(|(_, catalog)| catalog.iter().map(|metric| metric.name))
        .collect();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ops");
    for file in ["alerts.yml", "dashboard.json"] {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        let mut rest = text.as_str();
        while let Some(at) = rest.find("schwab_") {
            let name: String = rest[at..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            rest = &rest[at + name.len()..];
            // Prometheus derives these series from a histogram.
            let base = ["_bucket", "_sum", "_count"]
                .iter()
                .find_map(|suffix| name.strip_suffix(suffix))
                .filter(|base| declared.contains(base))
                .unwrap_or(&name);
            assert!(
                declared.contains(base),
                "{file} queries {name}, which no catalog declares"
            );
        }
    }
}
