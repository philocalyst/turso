//! Process and timing helpers for the differential oracle.

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::{
    structured_diff, EngineOutput, OracleError, RunnerConfig, Scenario, ScenarioResult,
    TimingResult,
};

const SHELL_SETUP: &str = ".headers off\n.mode list\n";

/// Select an engine for direct execution or timing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Engine {
    Turso,
    DoltLite,
}

/// Run one scenario against both binaries.
pub fn run_scenario(
    scenario: &Scenario,
    config: &RunnerConfig,
) -> Result<ScenarioResult, OracleError> {
    let turso = run_engine(Engine::Turso, scenario, &config.tursodb)?;
    let doltlite = run_engine(Engine::DoltLite, scenario, &config.doltlite)?;
    Ok(ScenarioResult {
        scenario: scenario.clone(),
        turso,
        doltlite,
    })
}

/// Time both engines with a fresh in-memory database for every repetition.
/// Engine order alternates so process startup and host scheduling are not
/// always charged to one side.
pub fn time_scenario(
    scenario: &Scenario,
    config: &RunnerConfig,
) -> Result<TimingResult, OracleError> {
    let repetitions = config.repetitions.max(1);
    let mut turso_samples = Vec::with_capacity(repetitions);
    let mut doltlite_samples = Vec::with_capacity(repetitions);
    for index in 0..repetitions {
        let (first, second) = if index % 2 == 0 {
            (Engine::Turso, Engine::DoltLite)
        } else {
            (Engine::DoltLite, Engine::Turso)
        };
        let first_started = Instant::now();
        let first_output = run_engine_for_config(first, scenario, config)?;
        let first_elapsed = first_started.elapsed();
        let second_started = Instant::now();
        let second_output = run_engine_for_config(second, scenario, config)?;
        let second_elapsed = second_started.elapsed();
        if first == Engine::Turso {
            turso_samples.push(first_elapsed);
            doltlite_samples.push(second_elapsed);
            ensure_matching_outputs(scenario, &first_output, &second_output)?;
        } else {
            doltlite_samples.push(first_elapsed);
            turso_samples.push(second_elapsed);
            ensure_matching_outputs(scenario, &second_output, &first_output)?;
        }
    }
    Ok(TimingResult {
        scenario: scenario.name.clone(),
        repetitions,
        turso: median(&mut turso_samples),
        doltlite: median(&mut doltlite_samples),
    })
}

/// Run one selected engine. This is public for the timing/reporting layer.
pub fn run_engine(
    engine: Engine,
    scenario: &Scenario,
    binary: &Path,
) -> Result<EngineOutput, OracleError> {
    run_engine_with_args(scenario, binary, &config_sql_args(engine))
}

fn run_engine_for_config(
    engine: Engine,
    scenario: &Scenario,
    config: &RunnerConfig,
) -> Result<EngineOutput, OracleError> {
    let binary = match engine {
        Engine::Turso => &config.tursodb,
        Engine::DoltLite => &config.doltlite,
    };
    run_engine(engine, scenario, binary)
}

fn config_sql_args(engine: Engine) -> Vec<String> {
    match engine {
        Engine::Turso => vec![
            ":memory:".to_string(),
            "-q".to_string(),
            "-m".to_string(),
            "list".to_string(),
        ],
        Engine::DoltLite => vec![":memory:".to_string()],
    }
}

fn run_engine_with_args(
    scenario: &Scenario,
    binary: &Path,
    args: &[String],
) -> Result<EngineOutput, OracleError> {
    if binary.components().count() > 1 && !binary.exists() {
        return Err(OracleError::MissingBinary(binary.to_path_buf()));
    }
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            OracleError::MissingBinary(binary.to_path_buf())
        } else {
            OracleError::Io(error)
        }
    })?;
    {
        let stdin = child.stdin.as_mut().ok_or_else(|| {
            OracleError::InvalidScenario(format!("{} has no stdin", binary.display()))
        })?;
        stdin.write_all(SHELL_SETUP.as_bytes())?;
        stdin.write_all(scenario.sql.as_bytes())?;
        if !scenario.sql.ends_with('\n') {
            stdin.write_all(b"\n")?;
        }
    }
    let output = child.wait_with_output()?;
    Ok(engine_output(output))
}

fn engine_output(output: Output) -> EngineOutput {
    EngineOutput {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn ensure_matching_outputs(
    scenario: &Scenario,
    left: &EngineOutput,
    right: &EngineOutput,
) -> Result<(), OracleError> {
    if left.normalized() == right.normalized() {
        return Ok(());
    }
    Err(OracleError::InvalidScenario(
        structured_diff(&scenario.name, &left.normalized(), &right.normalized())
            .unwrap_or_else(|| format!("{} produced different output", scenario.name)),
    ))
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}
