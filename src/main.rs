use relay::{Claim, MAX_PAYLOAD_BYTES, MAX_RESULT_BYTES, Store};
use std::{error::Error, fs::File, io::Read};
const USAGE: &str = "usage: relay <db> submit <key> <payload-file> | claim <owner> | active | get <id> | finish <id> <generation> <owner> <result-file> | confirm-stopped-and-requeue <id> <generation> <owner>";
fn read_bounded(path: &str, limit: usize) -> Result<String, Box<dyn Error>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err("input file exceeds byte limit".into());
    }
    Ok(String::from_utf8(bytes)?)
}
fn run() -> Result<serde_json::Value, Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [db, command, rest @ ..] = args.as_slice() else {
        return Err(USAGE.into());
    };
    let valid = match command.as_str() {
        "submit" => rest.len() == 2,
        "claim" | "get" => rest.len() == 1,
        "finish" => rest.len() == 4,
        "confirm-stopped-and-requeue" => rest.len() == 3,
        "active" => rest.is_empty(),
        _ => false,
    };
    if !valid {
        return Err(USAGE.into());
    }
    let mut store = Store::open(db)?;
    let value = match command.as_str() {
        "submit" => serde_json::to_value(
            store.submit(&rest[0], &read_bounded(&rest[1], MAX_PAYLOAD_BYTES)?)?,
        )?,
        "claim" => serde_json::to_value(store.claim_next(&rest[0])?)?,
        "active" => serde_json::to_value(store.active_claim()?)?,
        "get" => serde_json::to_value(store.get(rest[0].parse()?)?)?,
        "finish" | "confirm-stopped-and-requeue" => {
            let claim = Claim {
                task_id: rest[0].parse()?,
                generation: rest[1].parse()?,
                owner: rest[2].clone(),
            };
            let task = if command == "finish" {
                store.finish(&claim, &read_bounded(&rest[3], MAX_RESULT_BYTES)?)?
            } else {
                store.confirm_stopped_and_requeue(&claim)?
            };
            serde_json::to_value(task)?
        }
        _ => unreachable!(),
    };
    Ok(value)
}
fn main() {
    match run() {
        Ok(value) => println!("{value}"),
        Err(error) => {
            eprintln!("{}", serde_json::json!({"error": error.to_string()}));
            std::process::exit(1);
        }
    }
}
