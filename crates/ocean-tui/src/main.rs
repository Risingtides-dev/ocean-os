use std::{env, path::PathBuf};

use clap::Parser;

mod shell;
mod splash;

const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:4780";

/// Ocean's terminal workbench.
#[derive(Debug, Parser)]
#[command(name = "ocean-tui", about = "Ocean terminal workbench")]
struct Cli {
    #[arg(
        long,
        env = "OCEAN_DAEMON_URL",
        default_value = DEFAULT_DAEMON_URL
    )]
    url: String,

    /// Working directory / project root. Overrides OCEAN_PROJECT and the
    /// current directory. Switch projects at runtime from the session rail.
    #[arg(long, env = "OCEAN_PROJECT")]
    project: Option<String>,

    /// Resume an Ocean session by exact id or unambiguous id prefix.
    #[arg(long, env = "OCEAN_SESSION")]
    session: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = resolve_project_root(cli.project.as_deref());
    shell::run(
        &cli.url,
        root.to_string_lossy().into_owned(),
        cli.session.as_deref(),
    )
}

fn resolve_project_root(cli_project: Option<&str>) -> PathBuf {
    let requested = cli_project
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var("OCEAN_PROJECT")
                .ok()
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
        });

    match requested {
        Some(path) if path.is_absolute() => path,
        Some(path) => env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path),
        None => env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_project_passes_through() {
        assert_eq!(
            resolve_project_root(Some("/tmp/ocean-project")),
            PathBuf::from("/tmp/ocean-project")
        );
    }

    #[test]
    fn relative_project_resolves_from_launch_cwd() {
        let cwd = env::current_dir().expect("current dir");
        assert_eq!(
            resolve_project_root(Some("nested/project")),
            cwd.join("nested/project")
        );
    }

    #[test]
    fn session_flag_targets_the_native_workbench() {
        let cli = Cli::try_parse_from(["ocean", "--session", "abc123"]).expect("valid CLI");
        assert_eq!(cli.session.as_deref(), Some("abc123"));
    }

    #[test]
    fn removed_legacy_and_mesh_surfaces_are_rejected() {
        assert!(Cli::try_parse_from(["ocean", "--legacy"]).is_err());
        assert!(Cli::try_parse_from(["ocean", "mesh"]).is_err());
    }
}
