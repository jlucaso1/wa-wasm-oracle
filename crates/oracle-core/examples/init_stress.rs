//! Measures how reliably the VoIP stack initialises.
//!
//! The startup race this exists to quantify is described in
//! `tests/signaling.rs`; run it after touching threading or scheduling.
use oracle_core::{Catalog, Runtime, ThreadPolicy, Value};

fn main() -> anyhow::Result<()> {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20);

    let catalog = Catalog::discover()?;
    let entry = catalog.resolve("D5pLH9sfOOl")?;
    let bytes = std::fs::read(&entry.path)?;

    let (mut ok, mut forced) = (0usize, 0u64);
    for round in 1..=rounds {
        let mut r = Runtime::instantiate(&bytes)?;
        r.set_thread_policy(ThreadPolicy::Spawn);
        r.run_ctors()?;

        let started = std::time::Instant::now();
        let result = r.call_embind(
            "initVoipStack",
            &[
                Value::Str("15550002222@s.whatsapp.net".into()),
                Value::Str("0".into()),
                Value::Str("{}".into()),
            ],
        );
        let elapsed = started.elapsed();
        forced += r.forced_turns();

        match &result {
            Ok(Value::Int(0)) => ok += 1,
            other => println!(
                "{round:>3}: {:?} in {elapsed:?}\n     host log: {:?}",
                other
                    .as_ref()
                    .err()
                    .map(|_| "trap")
                    .unwrap_or("unexpected value"),
                r.logs()
                    .into_iter()
                    .filter(|line| {
                        line.contains("caught in")
                            || line.contains("export absent")
                            || line.contains("failed")
                    })
                    .rev()
                    .take(3)
                    .map(|line| line.chars().take(110).collect::<String>())
                    .collect::<Vec<_>>()
            ),
        }
    }

    println!("{ok}/{rounds} succeeded, {forced} forced turns");
    Ok(())
}
