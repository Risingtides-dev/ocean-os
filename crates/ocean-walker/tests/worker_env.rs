use std::process::Command;

#[test]
fn ocean_worker_environment_name_is_honored_in_an_isolated_process() {
    let output = Command::new(std::env::current_exe().expect("test executable should resolve"))
        .args(["worker_env_probe", "--exact", "--nocapture"])
        .env("OCEAN_WALKER_ENV_PROBE", "1")
        .env("OCEAN_WALK_WORKERS", "1")
        .env("PI_WALK_WORKERS", "9")
        .output()
        .expect("isolated worker environment probe should run");

    assert!(
        output.status.success(),
        "isolated probe failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn worker_env_probe() {
    if std::env::var_os("OCEAN_WALKER_ENV_PROBE").is_none() {
        return;
    }
    assert_eq!(
        ocean_walker::walk_workers(),
        1,
        "OCEAN_WALK_WORKERS must own worker-count configuration"
    );
}
