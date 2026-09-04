//! Differential execution of version-control SQL scenarios.
//!
//! The oracle deliberately talks to both engines through their command-line
//! shells.  That keeps the compared input identical and catches differences
//! in SQL registration, output, and error handling at the boundary users see.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub mod check_buckets;
pub mod runner;
pub use runner::{run_engine, run_scenario, time_scenario, Engine};
pub const BUCKET_NAMES: &[&str] = &[
    "refs-workspace",
    "diff-history-data",
    "merge-replay-schema",
    "feature-interaction",
    "remotes-recovery",
];

/// Errors reported by the runner itself, as opposed to errors produced by an
/// engine (which are part of the comparison).
#[derive(Debug)]
pub enum OracleError {
    Io(io::Error),
    InvalidScenario(String),
    MissingBinary(PathBuf),
    NoScenarios,
}

impl fmt::Display for OracleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::InvalidScenario(message) => write!(f, "invalid scenario: {message}"),
            Self::MissingBinary(path) => {
                write!(f, "engine binary does not exist: {}", path.display())
            }
            Self::NoScenarios => write!(f, "no oracle scenarios found"),
        }
    }
}

impl std::error::Error for OracleError {}

impl From<io::Error> for OracleError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A checked-in SQL script and its bucket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scenario {
    pub bucket: String,
    pub name: String,
    pub path: PathBuf,
    pub sql: String,
}

/// Paths and timing settings used by a run.
#[derive(Clone, Debug)]
pub struct RunnerConfig {
    pub tursodb: PathBuf,
    pub doltlite: PathBuf,
    pub repetitions: usize,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            tursodb: std::env::var_os("TURSODB_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("target/debug/tursodb")),
            doltlite: std::env::var_os("DOLTLITE_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("doltlite")),
            repetitions: 30,
        }
    }
}

/// Captured output from one engine invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl EngineOutput {
    pub fn normalized(&self) -> String {
        normalize_engine_output(self.status, &self.stdout, &self.stderr)
    }

    pub fn succeeded(&self) -> bool {
        self.status == Some(0) && normalize_error(&self.stderr).is_empty()
    }
}

/// Result of running one scenario against both shells.
#[derive(Clone, Debug)]
pub struct ScenarioResult {
    pub scenario: Scenario,
    pub turso: EngineOutput,
    pub doltlite: EngineOutput,
}

impl ScenarioResult {
    pub fn is_match(&self) -> bool {
        self.turso.normalized() == self.doltlite.normalized()
    }

    pub fn diff(&self) -> Option<String> {
        structured_diff(
            &self.scenario.name,
            &self.turso.normalized(),
            &self.doltlite.normalized(),
        )
    }
}

/// A process timing result.  Values are nanoseconds so callers can choose
/// their display unit without losing precision.
#[derive(Clone, Debug)]
pub struct TimingResult {
    pub scenario: String,
    pub repetitions: usize,
    pub turso: Duration,
    pub doltlite: Duration,
}

/// Return the repository's oracle bucket directory.
pub fn default_bucket_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("buckets")
}

/// Discover scenarios listed by each bucket manifest.
pub fn discover_scenarios(root: &Path) -> Result<Vec<Scenario>, OracleError> {
    let mut scenarios = Vec::new();
    for bucket in BUCKET_NAMES {
        let manifest = root.join(bucket).join("manifest.txt");
        let content = fs::read_to_string(&manifest)?;
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
            let path = root.join(bucket).join(relative);
            let sql = fs::read_to_string(&path)?;
            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| {
                    OracleError::InvalidScenario(format!("{} has no UTF-8 stem", path.display()))
                })?
                .to_string();
            scenarios.push(Scenario {
                bucket: (*bucket).to_string(),
                name,
                path,
                sql,
            });
        }
    }
    scenarios.sort_by(|left, right| left.path.cmp(&right.path));
    if scenarios.is_empty() {
        return Err(OracleError::NoScenarios);
    }
    Ok(scenarios)
}

/// Normalize output while preserving query and row order.
pub fn normalize_output(output: &str) -> String {
    output
        .replace('\r', "")
        .lines()
        .map(|line| {
            let cells = line
                .trim()
                .split('|')
                .map(normalize_cell)
                .collect::<Vec<_>>();
            cells.join("|")
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalize shell errors without hiding the actual error message.
pub fn normalize_error(error: &str) -> String {
    let mut lines = Vec::new();
    for raw_line in error.replace('\r', "").lines() {
        let mut line = strip_ansi(raw_line).trim().to_string();
        if let Some(index) = line.find("near line ") {
            if let Some(colon) = line[index..].find(':') {
                line = line[index + colon + 1..].trim().to_string();
            }
        }
        for prefix in ["Runtime error:", "Error:", "Parse error:"] {
            if let Some(rest) = line.strip_prefix(prefix) {
                line = rest.trim().to_string();
            }
        }
        while let Some(rest) = line.strip_prefix("× ") {
            line = rest.trim().to_string();
        }
        if matches!(line.chars().next(), Some('╭' | '╰' | '─' | '│' | '·')) || line.is_empty()
        {
            continue;
        }
        lines.push(collapse_spaces(&line));
    }
    lines.join("\n")
}

fn normalize_engine_output(status: Option<i32>, stdout: &str, stderr: &str) -> String {
    let stdout = normalize_output(stdout);
    let stderr = normalize_error(stderr);
    if status == Some(0) && stderr.is_empty() {
        return stdout;
    }
    format!(
        "status={}\nstdout:\n{}\nstderr:\n{}",
        status.map_or_else(|| "signal".to_string(), |code| code.to_string()),
        stdout,
        stderr
    )
}

fn normalize_cell(cell: &str) -> String {
    let mut cell = collapse_spaces(cell.trim());
    if cell == "true" || cell == "TRUE" {
        return "1".to_string();
    }
    if cell == "false" || cell == "FALSE" {
        return "0".to_string();
    }
    cell = normalize_hash_tokens(&cell);
    if cell.contains(',') {
        let mut parts = cell.split(',').map(str::trim).collect::<Vec<_>>();
        if parts
            .iter()
            .all(|part| !part.is_empty() && !part.contains(' '))
        {
            parts.sort_unstable();
            cell = parts.join(",");
        }
    }
    cell
}

fn normalize_hash_tokens(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut token = String::new();
    let flush = |normalized: &mut String, token: &mut String| {
        if is_hash_token(token) {
            normalized.push_str("<HASH>");
        } else {
            normalized.push_str(token);
        }
        token.clear();
    };
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            token.push(character);
        } else {
            flush(&mut normalized, &mut token);
            normalized.push(character);
        }
    }
    flush(&mut normalized, &mut token);
    normalized
}

fn is_hash_token(token: &str) -> bool {
    (token.len() == 40 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
        || (token.len() == 32
            && token
                .bytes()
                .all(|byte| matches!(byte.to_ascii_lowercase(), b'0'..=b'9' | b'a'..=b'v')))
}

fn collapse_spaces(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut pending_space = false;
    for character in value.chars() {
        if character.is_whitespace() {
            pending_space = !result.is_empty();
        } else {
            if pending_space && !result.ends_with(' ') {
                result.push(' ');
            }
            pending_space = false;
            result.push(character);
        }
    }
    result.trim().to_string()
}

fn strip_ansi(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            if characters.next() == Some('[') {
                for control in characters.by_ref() {
                    if ('@'..='~').contains(&control) {
                        break;
                    }
                }
            }
        } else {
            result.push(character);
        }
    }
    result
}

/// Render a deterministic, line-oriented diff for CI logs.
pub fn structured_diff(name: &str, turso: &str, doltlite: &str) -> Option<String> {
    if turso == doltlite {
        return None;
    }
    let turso_lines = turso.lines().collect::<Vec<_>>();
    let doltlite_lines = doltlite.lines().collect::<Vec<_>>();
    let mut diff = format!(
        "scenario: {name}\nturso_lines: {}\ndoltlite_lines: {}\ndiffs:",
        turso_lines.len(),
        doltlite_lines.len()
    );
    for index in 0..turso_lines.len().max(doltlite_lines.len()) {
        let left = turso_lines.get(index).copied().unwrap_or("<missing>");
        let right = doltlite_lines.get(index).copied().unwrap_or("<missing>");
        if left != right {
            diff.push_str(&format!(
                "\n  line {}: turso={} doltlite={}",
                index + 1,
                left,
                right
            ));
        }
    }
    Some(diff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_hashes_booleans_and_cells() {
        assert_eq!(
            normalize_output("40c0ffee40c0ffee40c0ffee40c0ffee40c0ffee|true|z,y,x\r\n"),
            "<HASH>|1|x,y,z"
        );
    }

    #[test]
    fn normalizes_base32_hashes() {
        assert_eq!(
            normalize_output("0123456789abcdefghijklmnopqrstuv"),
            "<HASH>"
        );
    }

    #[test]
    fn preserves_row_order() {
        assert_eq!(normalize_output("b\na\n"), "b\na");
    }

    #[test]
    fn strips_shell_error_decoration() {
        assert_eq!(
            normalize_error("  × near line 14: branch not found: nope\n   ╰─ here"),
            "branch not found: nope"
        );
    }

    #[test]
    fn structured_diff_reports_missing_lines() {
        let diff = structured_diff("case", "one\ntwo", "one").unwrap();
        assert!(diff.contains("line 2: turso=two doltlite=<missing>"));
    }
}
