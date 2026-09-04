use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use doltlite_oracle::{
    check_buckets, default_bucket_root, discover_scenarios, run_scenario, structured_diff,
    time_scenario, RunnerConfig,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Action {
    Run,
    CheckBuckets,
}

#[derive(Debug, Parser)]
#[command(
    name = "doltlite-oracle",
    about = "Run normalized SQL scenarios against Turso and DoltLite"
)]
struct Args {
    #[arg(value_enum, default_value_t = Action::Run)]
    action: Action,
    #[arg(long, value_name = "NAME", help = "Run one scenario by name or path")]
    filter: Option<String>,
    #[arg(long, help = "Run every scenario in all buckets")]
    batch: bool,
    #[arg(long, help = "Report median process time instead of only pass/fail")]
    time: bool,
    #[arg(
        long,
        value_name = "START:END",
        help = "Run the deterministic generated sweep for an inclusive seed range"
    )]
    seeds: Option<String>,
    #[arg(long, default_value_t = 30, value_name = "N")]
    reps: usize,
    #[arg(long, value_name = "PATH")]
    tursodb: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    doltlite: Option<PathBuf>,
    #[arg(long, value_name = "PATH", default_value_os_t = default_bucket_root())]
    root: PathBuf,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("doltlite-oracle: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let reports = check_buckets::validate(&args.root)?;
    if matches!(args.action, Action::CheckBuckets) {
        if args.seeds.is_some() {
            return Err("--seeds is only valid with the run action".into());
        }
        for report in reports {
            println!("{} | {} scenarios", report.name, report.scenarios);
        }
        return Ok(());
    }

    let scenarios = discover_scenarios(&args.root)?;
    let selected = select_scenarios(scenarios, args.filter.as_deref())?;
    let config = RunnerConfig {
        tursodb: args
            .tursodb
            .unwrap_or_else(|| RunnerConfig::default().tursodb),
        doltlite: args
            .doltlite
            .unwrap_or_else(|| RunnerConfig::default().doltlite),
        repetitions: args.reps.max(1),
    };

    if let Some(raw_range) = args.seeds.as_deref() {
        if args.filter.is_some() || args.batch || args.time {
            return Err("--seeds cannot be combined with --filter, --batch, or --time".into());
        }
        let (start, end) = parse_seed_range(raw_range)?;
        run_seed_sweep(start, end, &config)?;
        return Ok(());
    }

    if args.time {
        run_timing(&selected, &config)?;
    } else {
        run_differential(&selected, &config)?;
    }
    Ok(())
}

fn parse_seed_range(raw: &str) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let Some((start, end)) = raw.split_once(':') else {
        return Err(format!("seed range must be START:END, got {raw:?}").into());
    };
    let start = start
        .parse::<u64>()
        .map_err(|_| format!("invalid seed range start: {start:?}"))?;
    let end = end
        .parse::<u64>()
        .map_err(|_| format!("invalid seed range end: {end:?}"))?;
    if end < start {
        return Err(format!("seed range ends before it starts: {raw:?}").into());
    }
    if end - start >= 1_000_000 {
        return Err("seed sweep is limited to 1,000,000 seeds per invocation".into());
    }
    Ok((start, end))
}

fn run_seed_sweep(
    start: u64,
    end: u64,
    config: &RunnerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let total = end - start + 1;
    let mut seed = start;
    let mut completed = 0_u64;
    loop {
        let scenario = generated_seed_scenario(seed);
        let result = run_scenario(&scenario, config)?;
        if !result.is_match() {
            let diff = result.diff().unwrap_or_else(|| scenario.name.clone());
            return Err(format!("seed {seed} diverged:\n{diff}").into());
        }
        completed += 1;
        if completed % 100 == 0 || completed == total {
            println!("sweep: {completed}/{total} seeds passed");
        }
        if seed == end {
            break;
        }
        seed += 1;
    }
    Ok(())
}

fn generated_seed_scenario(seed: u64) -> doltlite_oracle::Scenario {
    let id = (seed % 1_000_000) + 1;
    let value = (seed.wrapping_mul(31).wrapping_add(7)) % 1_000_000;
    let sql = format!(
        ".output /dev/null\n\
         SELECT dolt_config('user.name', 'Oracle Sweep');\n\
         SELECT dolt_config('user.email', 'oracle-sweep@example.com');\n\
         CREATE TABLE sweep (id INTEGER PRIMARY KEY, value INTEGER);\n\
         INSERT INTO sweep VALUES ({id}, {value});\n\
         SELECT dolt_add('-A');\n\
         SELECT dolt_commit('-m', 'seed');\n\
         SELECT dolt_branch('feature');\n\
         SELECT dolt_checkout('feature');\n\
         UPDATE sweep SET value = value + 1 WHERE id = {id};\n\
         SELECT dolt_add('-A');\n\
         SELECT dolt_commit('-m', 'feature');\n\
         SELECT dolt_checkout('main');\n\
         .output stdout\n\
         SELECT '{seed}' AS seed, id, value FROM sweep ORDER BY id;\n"
    );
    doltlite_oracle::Scenario {
        bucket: "generated-sweep".to_string(),
        name: format!("seed-{seed}"),
        path: PathBuf::from(format!("generated/seed-{seed}.sql")),
        sql,
    }
}

fn select_scenarios(
    scenarios: Vec<doltlite_oracle::Scenario>,
    filter: Option<&str>,
) -> Result<Vec<doltlite_oracle::Scenario>, Box<dyn std::error::Error>> {
    let Some(filter) = filter else {
        return Ok(scenarios);
    };
    let selected = scenarios
        .into_iter()
        .filter(|scenario| {
            scenario.name == filter
                || scenario.path.to_string_lossy().contains(filter)
                || format!("{}/{}", scenario.bucket, scenario.name).contains(filter)
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(format!("no scenario matches --filter {filter:?}").into());
    }
    Ok(selected)
}

fn run_differential(
    scenarios: &[doltlite_oracle::Scenario],
    config: &RunnerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut failures = 0;
    for scenario in scenarios {
        let result = run_scenario(scenario, config)?;
        if result.is_match() {
            println!("PASS | {}/{}", scenario.bucket, scenario.name);
        } else {
            failures += 1;
            println!("FAIL | {}/{}", scenario.bucket, scenario.name);
            if let Some(diff) = structured_diff(
                &scenario.name,
                &result.turso.normalized(),
                &result.doltlite.normalized(),
            ) {
                println!("{diff}");
            }
        }
    }
    if failures != 0 {
        return Err(format!("{failures} oracle scenario(s) diverged").into());
    }
    println!("{} scenario(s) passed", scenarios.len());
    Ok(())
}

fn run_timing(
    scenarios: &[doltlite_oracle::Scenario],
    config: &RunnerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("scenario | reps | turso_ms | doltlite_ms | ratio");
    for scenario in scenarios {
        let timing = time_scenario(scenario, config)?;
        let turso_ms = timing.turso.as_secs_f64() * 1_000.0;
        let doltlite_ms = timing.doltlite.as_secs_f64() * 1_000.0;
        let ratio = if doltlite_ms == 0.0 {
            f64::INFINITY
        } else {
            turso_ms / doltlite_ms
        };
        println!(
            "{} | {} | {:.3} | {:.3} | {:.3}",
            timing.scenario, timing.repetitions, turso_ms, doltlite_ms, ratio
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_range_is_inclusive() {
        assert_eq!(parse_seed_range("1:10000").unwrap(), (1, 10_000));
    }

    #[test]
    fn seed_range_rejects_bad_order_and_shape() {
        assert!(parse_seed_range("10:1").is_err());
        assert!(parse_seed_range("10").is_err());
        assert!(parse_seed_range("0:1000000").is_err());
    }

    #[test]
    fn generated_seed_scenario_is_bounded_and_deterministic() {
        let first = generated_seed_scenario(u64::MAX);
        let second = generated_seed_scenario(u64::MAX);
        assert_eq!(first, second);
        assert!(first.sql.contains("CREATE TABLE sweep"));
        assert!(first.sql.contains("SELECT '18446744073709551615' AS seed"));
    }
}
