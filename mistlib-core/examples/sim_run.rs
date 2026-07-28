use mistlib_core::sim::{run_scenario, ScenarioConfig};
use std::env;
use std::path::PathBuf;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (config, virtual_time) = parse_args()?;
    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_time();
    if virtual_time {
        builder.start_paused(true);
    }
    let runtime = builder.build()?;
    runtime.block_on(run_scenario(config))?;
    Ok(())
}

fn parse_args() -> Result<(ScenarioConfig, bool), Box<dyn std::error::Error>> {
    let mut config = ScenarioConfig::default();
    let mut virtual_time = true;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--virtual-time" => virtual_time = true,
            "--real-time" => virtual_time = false,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--nodes" => config.nodes = next_value(&mut args, &arg)?.parse()?,
            "--seed" => config.seed = next_value(&mut args, &arg)?.parse()?,
            "--duration" => config.duration = next_value(&mut args, &arg)?.parse()?,
            "--tick" => config.tick = next_value(&mut args, &arg)?.parse()?,
            "--aoi-radius" => config.aoi_radius = next_value(&mut args, &arg)?.parse()?,
            "--latency" => config.latency = millis(&next_value(&mut args, &arg)?)?,
            "--jitter" => config.jitter = millis(&next_value(&mut args, &arg)?)?,
            "--loss" => config.loss = next_value(&mut args, &arg)?.parse()?,
            "--out-dir" => config.out_dir = PathBuf::from(next_value(&mut args, &arg)?),
            "--bounds" => config.bounds = parse_bounds(&next_value(&mut args, &arg)?)?,
            "--speed-min" => config.speed_min = next_value(&mut args, &arg)?.parse()?,
            "--speed-max" => config.speed_max = next_value(&mut args, &arg)?.parse()?,
            "--tau-dir" => config.tau_dir = next_value(&mut args, &arg)?.parse()?,
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    Ok((config, virtual_time))
}

fn next_value(
    args: &mut impl Iterator<Item = String>,
    arg: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    args.next()
        .ok_or_else(|| format!("missing value for {arg}").into())
}

fn millis(value: &str) -> Result<Duration, Box<dyn std::error::Error>> {
    Ok(Duration::from_secs_f64(value.parse::<f64>()? / 1000.0))
}

fn parse_bounds(value: &str) -> Result<[f64; 3], Box<dyn std::error::Error>> {
    let parts: Vec<_> = value.split(',').collect();
    if parts.len() != 3 {
        return Err("--bounds expects x,y,z".into());
    }
    Ok([parts[0].parse()?, parts[1].parse()?, parts[2].parse()?])
}

fn print_help() {
    println!(
        "Usage: cargo run -p mistlib-core --features sim --example sim_run -- \
  --nodes 100 --seed 42 --duration 10 --tick 0.1 --aoi-radius 10 \
  --latency 50 --jitter 20 --loss 0.03 --out-dir sim-out [--real-time]"
    );
    println!("Virtual time is enabled by default. Use --real-time to run against wall-clock time.");
}
