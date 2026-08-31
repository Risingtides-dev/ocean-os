//! Offline operator command for preparing `memory.sqlite` for a pre-partition
//! Ocean binary.

use std::{env, path::PathBuf, process::ExitCode};

use ocean_memory::SqliteMemoryStore;

const CONFIRM: &str = "--offline-confirm";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ocean-memory-rollback: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args_os();
    let program = args
        .next()
        .and_then(|value| PathBuf::from(value).file_name().map(|name| name.to_owned()))
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| "ocean-memory-rollback".into());
    let confirmation = args.next().and_then(|value| value.into_string().ok());
    let path = args.next().map(PathBuf::from);
    if confirmation.as_deref() != Some(CONFIRM) || path.is_none() || args.next().is_some() {
        return Err(format!(
            "usage: {program} {CONFIRM} /absolute/path/to/memory.sqlite\n\
             stop ocean-daemon and every process using the database first"
        ));
    }
    let path = path.expect("validated above");
    if !path.is_absolute() {
        return Err("database path must be absolute".into());
    }
    let metadata = path
        .symlink_metadata()
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("database path must be an existing regular file, not a symlink".into());
    }

    let mut store = SqliteMemoryStore::open(&path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let report = store
        .prepare_legacy_rollback()
        .map_err(|error| format!("rollback preparation failed: {error}"))?;
    drop(store);
    println!(
        "prepared legacy memory rollback: operator_rows={} archived_room_rows={} path={}",
        report.operator_rows,
        report.archived_room_rows,
        path.display()
    );
    Ok(())
}
