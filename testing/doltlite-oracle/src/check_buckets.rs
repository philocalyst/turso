//! Structural checks for the checked-in oracle corpus.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::{default_bucket_root, OracleError, BUCKET_NAMES};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BucketReport {
    pub name: String,
    pub scenarios: usize,
}

/// Validate manifests and return a compact per-bucket report.
pub fn validate(root: &Path) -> Result<Vec<BucketReport>, OracleError> {
    let mut reports = Vec::new();
    let mut listed = HashSet::new();
    for bucket in BUCKET_NAMES {
        let directory = root.join(bucket);
        if !directory.is_dir() {
            return Err(OracleError::InvalidScenario(format!(
                "bucket directory is missing: {}",
                directory.display()
            )));
        }
        let manifest = directory.join("manifest.txt");
        let content = fs::read_to_string(&manifest)?;
        let mut scenarios = 0;
        for (line_number, raw_entry) in content.lines().enumerate() {
            let entry = raw_entry.trim();
            if entry.is_empty() || entry.starts_with('#') {
                continue;
            }
            let relative = Path::new(entry);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|component| component == std::path::Component::ParentDir)
            {
                return Err(OracleError::InvalidScenario(format!(
                    "{}:{} escapes its bucket",
                    manifest.display(),
                    line_number + 1
                )));
            }
            if relative
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("sql")
            {
                return Err(OracleError::InvalidScenario(format!(
                    "{}:{} must list a .sql file",
                    manifest.display(),
                    line_number + 1
                )));
            }
            let path = directory.join(relative);
            if !path.is_file() {
                return Err(OracleError::InvalidScenario(format!(
                    "{}:{} lists missing {}",
                    manifest.display(),
                    line_number + 1,
                    path.display()
                )));
            }
            if !listed.insert(path.clone()) {
                return Err(OracleError::InvalidScenario(format!(
                    "scenario is listed more than once: {}",
                    path.display()
                )));
            }
            scenarios += 1;
        }
        if *bucket != "remotes-recovery" && scenarios == 0 {
            return Err(OracleError::InvalidScenario(format!(
                "O6 bucket has no scenarios: {bucket}"
            )));
        }
        reports.push(BucketReport {
            name: (*bucket).to_string(),
            scenarios,
        });
    }
    Ok(reports)
}

/// The repository-local guard used by the CLI and by tests.
pub fn check_default_buckets() -> Result<Vec<BucketReport>, OracleError> {
    validate(&default_bucket_root())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn manifests_are_complete_and_owned_buckets_are_populated() {
        let reports = check_default_buckets().expect("oracle manifests");
        assert_eq!(reports.len(), BUCKET_NAMES.len());
        for report in reports {
            if report.name != "remotes-recovery" {
                assert!(report.scenarios >= 1, "empty O6 bucket: {}", report.name);
            }
        }
    }

    #[test]
    #[ignore = "requires both built engines; run with --ignored and DOLTLITE_BIN"]
    fn configured_differential_corpus_is_green() {
        let Some(_) = std::env::var_os("DOLTLITE_BIN") else {
            return;
        };
        let scenarios =
            crate::discover_scenarios(&default_bucket_root()).expect("discover oracle scenarios");
        let config = crate::RunnerConfig::default();
        let mut failures = Vec::new();
        for scenario in scenarios {
            let result = crate::run_scenario(&scenario, &config).expect("run oracle scenario");
            if !result.is_match() {
                failures.push(result.diff().unwrap_or_else(|| scenario.name.clone()));
            }
        }
        assert!(
            failures.is_empty(),
            "oracle divergences:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn remote_recovery_manifest_has_no_o6_entries() {
        let path: PathBuf = default_bucket_root()
            .join("remotes-recovery")
            .join("manifest.txt");
        let content = fs::read_to_string(path).expect("remote manifest");
        assert!(content.lines().all(|line| {
            let line = line.trim();
            line.is_empty() || line.starts_with('#')
        }));
    }
}
