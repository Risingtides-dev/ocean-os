//! Git-plane offshore tools: workspace, ship, fetch, clean.
//!
//! workspace/ship/clean run small shell scripts on the remote box over ssh;
//! fetch runs LOCAL git against the remote mirror's ssh URL. The scripts are
//! assembled by pure functions so their exact shape — including the two
//! hard-won mirror-clone fixes (push-by-URL with an explicit refspec; heads-only
//! no-prune mirror refresh) — is pinned by unit tests, no network required.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{
    generate_job_id, head, is_clone_url, job_paths, json_text, repo_name, tail, validate_name,
    OffshoreToolCtx, SUBPROCESS_TIMEOUT,
};
use crate::types::{AgentTool, AgentToolResult};

pub struct OffshoreWorkspaceTool {
    pub ctx: OffshoreToolCtx,
}

#[async_trait]
impl AgentTool for OffshoreWorkspaceTool {
    fn name(&self) -> &str {
        "offshore_workspace"
    }
    fn description(&self) -> &str {
        "Provision an isolated git worktree for one offshore job on the remote box — step 1 of the offshore lifecycle (workspace → dispatch → events/sessions → ship or fetch → clean; one job per task, one session per job). 'repo' is a clone URL the first time a repo is used (the box keeps a mirror clone of it); afterwards the bare repo name is enough. Creates branch offshore/<job> from 'base' in a fresh worktree and returns {job, repo, branch, cwd}. Dispatch ONLY into the returned cwd."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Clone URL (first use) or existing mirror name, e.g. 'ocean-os'" },
                "base": { "type": "string", "description": "Base ref for the job branch", "default": "main" },
                "job": { "type": "string", "description": "Explicit job id (default: generated timestamp-hex)" }
            },
            "required": ["repo"]
        })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let repo = args
            .get("repo")
            .and_then(|v| v.as_str())
            .ok_or("missing 'repo'")?;
        let base = args.get("base").and_then(|v| v.as_str()).unwrap_or("main");
        let name = repo_name(repo);
        validate_name("repo name", &name)?;
        let job = match args.get("job").and_then(|v| v.as_str()) {
            Some(j) => {
                validate_name("job id", j)?;
                j.to_string()
            }
            None => generate_job_id(),
        };

        let root = &self.ctx.cfg.remote_root;
        let script = workspace_script(root, repo, base, &job);
        let out = self.ctx.run_ssh(&script).await?;
        if !out.status.success() {
            return Err(format!(
                "workspace failed: {}",
                head(&String::from_utf8_lossy(&out.stderr), 600)
            ));
        }
        // The script's last stdout line is the remote $HOME, which anchors the
        // absolute cwd the dispatch tool needs.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let home = stdout
            .trim()
            .lines()
            .last()
            .ok_or("workspace produced no output (expected the remote $HOME)")?
            .to_string();
        Ok(json_text(&json!({
            "job": job,
            "repo": name,
            "branch": format!("offshore/{job}"),
            "cwd": format!("{home}/{root}/jobs/{job}/work"),
        })))
    }
}

pub struct OffshoreShipTool {
    pub ctx: OffshoreToolCtx,
}

#[async_trait]
impl AgentTool for OffshoreShipTool {
    fn name(&self) -> &str {
        "offshore_ship"
    }
    fn description(&self) -> &str {
        "Push an offshore job's branch (offshore/<job>) from the remote box to the repo's origin. With pr=true, also opens a draft PR via 'gh' on the remote box (title/body come from the head commit). Only COMMITTED work ships — if the job's agent hasn't committed yet, dispatch a turn telling it to commit first."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job": { "type": "string", "description": "Job id from offshore_workspace" },
                "repo": { "type": "string", "description": "Repo mirror name (or its clone URL)" },
                "pr": { "type": "boolean", "description": "Also open a draft PR", "default": false }
            },
            "required": ["job", "repo"]
        })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let (job, repo) = job_repo_args(&args)?;
        let pr = args.get("pr").and_then(|v| v.as_bool()).unwrap_or(false);
        let script = ship_script(&self.ctx.cfg.remote_root, repo, job, pr);
        let out = self.ctx.run_ssh(&script).await?;
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if !out.status.success() {
            return Err(format!("ship failed: {}", head(&combined, 600)));
        }
        Ok(json_text(&json!({
            "pushed": format!("offshore/{job}"),
            "output": tail(combined.trim(), 400),
        })))
    }
}

pub struct OffshoreFetchTool {
    pub ctx: OffshoreToolCtx,
}

#[async_trait]
impl AgentTool for OffshoreFetchTool {
    fn name(&self) -> &str {
        "offshore_fetch"
    }
    fn description(&self) -> &str {
        "Fetch an offshore job's branch from the remote mirror into a LOCAL git repo ('dest'), creating local branch offshore/<job> — the no-PR way to bring results onto this machine. Only committed work on the job branch is fetched."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job": { "type": "string", "description": "Job id from offshore_workspace" },
                "repo": { "type": "string", "description": "Repo mirror name (or its clone URL)" },
                "dest": { "type": "string", "description": "Local git repo to fetch into" }
            },
            "required": ["job", "repo", "dest"]
        })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let (job, repo) = job_repo_args(&args)?;
        let dest = args
            .get("dest")
            .and_then(|v| v.as_str())
            .ok_or("missing 'dest'")?;
        let dest = std::path::absolute(dest)
            .map_err(|e| format!("resolving dest '{dest}': {e}"))?
            .display()
            .to_string();
        let url = fetch_remote_url(&self.ctx.cfg.ssh_host, &self.ctx.cfg.remote_root, repo);
        let branch = format!("offshore/{job}");
        let refspec = format!("{branch}:{branch}");
        let out = run_local_git(&["-C", &dest, "fetch", &url, &refspec]).await?;
        if !out.status.success() {
            return Err(format!(
                "fetch failed: {}",
                head(&String::from_utf8_lossy(&out.stderr), 600)
            ));
        }
        Ok(json_text(&json!({ "fetched": branch, "into": dest })))
    }
}

pub struct OffshoreCleanTool {
    pub ctx: OffshoreToolCtx,
}

#[async_trait]
impl AgentTool for OffshoreCleanTool {
    fn name(&self) -> &str {
        "offshore_clean"
    }
    fn description(&self) -> &str {
        "Tear down a finished offshore job on the remote box: remove its worktree, delete the offshore/<job> branch from the mirror, and remove the job directory. Ship or fetch the branch FIRST — clean deletes unshipped work irrecoverably."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job": { "type": "string", "description": "Job id from offshore_workspace" },
                "repo": { "type": "string", "description": "Repo mirror name (or its clone URL)" }
            },
            "required": ["job", "repo"]
        })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let (job, repo) = job_repo_args(&args)?;
        let script = clean_script(&self.ctx.cfg.remote_root, repo, job);
        let out = self.ctx.run_ssh(&script).await?;
        if !out.status.success() {
            return Err(format!(
                "clean failed: {}",
                head(&String::from_utf8_lossy(&out.stderr), 600)
            ));
        }
        Ok(json_text(&json!({ "cleaned": job })))
    }
}

/// Extract and validate the `job`/`repo` pair shared by ship/fetch/clean. Both
/// end up interpolated into remote scripts or paths, so both are charset-checked.
fn job_repo_args(args: &Value) -> Result<(&str, &str), String> {
    let job = args
        .get("job")
        .and_then(|v| v.as_str())
        .ok_or("missing 'job'")?;
    let repo = args
        .get("repo")
        .and_then(|v| v.as_str())
        .ok_or("missing 'repo'")?;
    validate_name("job id", job)?;
    validate_name("repo name", &repo_name(repo))?;
    Ok((job, repo))
}

/// Run one LOCAL git command with output capture and the family's subprocess
/// deadline (`kill_on_drop` reaps it on timeout).
async fn run_local_git(args: &[&str]) -> Result<std::process::Output, String> {
    let mut command = tokio::process::Command::new("git");
    command.args(args).kill_on_drop(true);
    match tokio::time::timeout(SUBPROCESS_TIMEOUT, command.output()).await {
        Ok(out) => out.map_err(|e| format!("could not run git: {e}")),
        Err(_) => Err(format!(
            "git timed out after {}s",
            SUBPROCESS_TIMEOUT.as_secs()
        )),
    }
}

// ── remote script assembly (pure, unit-tested) ──────────────────────────────

/// The workspace-provisioning script: ensure the mirror, add a per-job
/// worktree on branch `offshore/<job>`, echo the remote `$HOME`.
///
/// The mirror refresh is heads-only with NO `--prune`: a `remote update
/// --prune` under the mirror refspec (`+refs/*:refs/*`) would delete the local
/// `offshore/*` job branches, which don't exist on origin.
pub(crate) fn workspace_script(remote_root: &str, repo: &str, base: &str, job: &str) -> String {
    let name = repo_name(repo);
    let root = format!("$HOME/{remote_root}");
    let (mirror, work, _branch) = job_paths(remote_root, repo, job);
    let init = if is_clone_url(repo) {
        format!("git clone --mirror {} {mirror}", shell_words::quote(repo))
    } else {
        format!("echo 'no mirror for {name}; pass a clone URL first' >&2; exit 2")
    };
    let base_q = shell_words::quote(base);
    format!(
        "set -e\n\
mkdir -p {root}/mirrors {root}/jobs\n\
if [ ! -d {mirror} ]; then\n\
  {init}\n\
else\n\
  # No `remote update --prune`: the mirror refspec (+refs/*:refs/*) would prune\n\
  # local offshore/* job branches that don't exist on origin.\n\
  git -C {mirror} fetch origin '+refs/heads/*:refs/heads/*' >/dev/null 2>&1 || true\n\
fi\n\
git -C {mirror} worktree add -b offshore/{job} {work} {base_q} >/dev/null\n\
echo \"$HOME\"\n"
    )
}

/// The ship script: push the job branch and optionally open a draft PR.
///
/// Push to the URL, not the remote name: the mirror clone has
/// `remote.origin.mirror=true`, which rejects per-branch refspecs. Same reason
/// gh's `--fill` can't work here (no `refs/remotes/origin/*`), so the PR
/// title/body come from the head commit instead.
pub(crate) fn ship_script(remote_root: &str, repo: &str, job: &str, pr: bool) -> String {
    let (_mirror, work, branch) = job_paths(remote_root, repo, job);
    let pr_cmd = if pr {
        format!(
            "gh pr create --draft --head {branch} \
             --title \"$(git log -1 --format=%s)\" \
             --body \"offshore job {job} — $(git log -1 --format=%H)\""
        )
    } else {
        "true".to_string()
    };
    format!(
        "set -e\n\
cd {work}\n\
url=$(git config remote.origin.url)\n\
git push \"$url\" {branch}:refs/heads/{branch}\n\
{pr_cmd}\n"
    )
}

/// The clean script: drop the worktree (force), delete the job branch, remove
/// the job directory. Every step tolerates partial prior cleanup.
pub(crate) fn clean_script(remote_root: &str, repo: &str, job: &str) -> String {
    let (mirror, work, branch) = job_paths(remote_root, repo, job);
    format!(
        "set -e\n\
git -C {mirror} worktree remove --force {work} 2>/dev/null || rm -rf {work}\n\
git -C {mirror} branch -D {branch} 2>/dev/null || true\n\
rm -rf $(dirname {work})\n"
    )
}

/// The ssh URL a LOCAL `git fetch` uses to reach the remote mirror.
pub(crate) fn fetch_remote_url(ssh_host: &str, remote_root: &str, repo: &str) -> String {
    format!(
        "ssh://{ssh_host}/~/{remote_root}/mirrors/{}.git",
        repo_name(repo)
    )
}

#[cfg(test)]
mod tests {
    use super::super::test_ctx;
    use super::*;

    #[test]
    fn workspace_script_clones_a_mirror_on_first_url_use() {
        let s = workspace_script(
            "offshore",
            "https://github.com/me/ocean-os.git",
            "main",
            "j1",
        );
        assert!(
            s.contains(
                "git clone --mirror https://github.com/me/ocean-os.git $HOME/offshore/mirrors/ocean-os.git"
            ),
            "{s}"
        );
        assert!(
            s.contains(
                "git -C $HOME/offshore/mirrors/ocean-os.git worktree add -b offshore/j1 $HOME/offshore/jobs/j1/work main >/dev/null"
            ),
            "{s}"
        );
        assert!(s.contains("echo \"$HOME\""), "{s}");
    }

    #[test]
    fn workspace_script_mirror_refresh_is_heads_only_and_never_prunes() {
        let s = workspace_script("offshore", "ocean-os", "main", "j1");
        // The hard-won fix: heads-only refspec, no prune, no `remote update`.
        assert!(
            s.contains("git -C $HOME/offshore/mirrors/ocean-os.git fetch origin '+refs/heads/*:refs/heads/*' >/dev/null 2>&1 || true"),
            "{s}"
        );
        // The script's comment may (and does) mention these; no COMMAND may.
        let commands: Vec<&str> = s
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect();
        assert!(
            !commands.iter().any(|l| l.contains("--prune")),
            "prune would delete job branches: {s}"
        );
        assert!(!commands.iter().any(|l| l.contains("remote update")), "{s}");
    }

    #[test]
    fn workspace_script_without_a_mirror_demands_a_clone_url() {
        let s = workspace_script("offshore", "ocean-os", "main", "j1");
        assert!(
            s.contains("echo 'no mirror for ocean-os; pass a clone URL first' >&2; exit 2"),
            "{s}"
        );
        assert!(!s.contains("git clone"), "{s}");
    }

    #[test]
    fn workspace_script_shell_quotes_the_base_ref() {
        let s = workspace_script("offshore", "ocean-os", "release branch", "j1");
        assert!(s.contains("'release branch' >/dev/null"), "{s}");
    }

    #[test]
    fn ship_script_pushes_to_the_url_with_an_explicit_refspec() {
        let s = ship_script("offshore", "ocean-os", "j1", false);
        // The hard-won fix: mirror clones (remote.origin.mirror=true) reject
        // per-branch refspecs through the named remote — push by URL.
        assert!(s.contains("url=$(git config remote.origin.url)"), "{s}");
        assert!(
            s.contains("git push \"$url\" offshore/j1:refs/heads/offshore/j1"),
            "{s}"
        );
        assert!(
            !s.contains("push origin"),
            "never push the named remote: {s}"
        );
        assert!(s.ends_with("true\n"), "no-PR variant is a no-op tail: {s}");
        assert!(!s.contains("gh pr create"), "{s}");
    }

    #[test]
    fn ship_script_pr_variant_builds_the_draft_pr_from_the_head_commit() {
        let s = ship_script("offshore", "ocean-os", "j1", true);
        assert!(s.contains("gh pr create --draft --head offshore/j1"), "{s}");
        assert!(s.contains("--title \"$(git log -1 --format=%s)\""), "{s}");
        assert!(
            s.contains("--body \"offshore job j1 — $(git log -1 --format=%H)\""),
            "{s}"
        );
    }

    #[test]
    fn clean_script_removes_worktree_branch_and_job_dir() {
        let s = clean_script("offshore", "ocean-os", "j1");
        assert!(
            s.contains("git -C $HOME/offshore/mirrors/ocean-os.git worktree remove --force $HOME/offshore/jobs/j1/work 2>/dev/null || rm -rf $HOME/offshore/jobs/j1/work"),
            "{s}"
        );
        assert!(
            s.contains("git -C $HOME/offshore/mirrors/ocean-os.git branch -D offshore/j1 2>/dev/null || true"),
            "{s}"
        );
        assert!(
            s.contains("rm -rf $(dirname $HOME/offshore/jobs/j1/work)"),
            "{s}"
        );
    }

    #[test]
    fn fetch_url_targets_the_remote_mirror_over_ssh() {
        assert_eq!(
            fetch_remote_url("smathdaddy@100.90.205.60", "offshore", "ocean-os"),
            "ssh://smathdaddy@100.90.205.60/~/offshore/mirrors/ocean-os.git"
        );
        // A clone URL for `repo` resolves to the same mirror name.
        assert_eq!(
            fetch_remote_url("host", "offshore", "https://github.com/me/ocean-os.git"),
            "ssh://host/~/offshore/mirrors/ocean-os.git"
        );
    }

    #[tokio::test]
    async fn workspace_requires_repo_and_validates_job() {
        let tool = OffshoreWorkspaceTool { ctx: test_ctx() };
        let err = tool
            .execute("t", json!({}))
            .await
            .expect_err("missing repo must error before any ssh call");
        assert!(err.contains("repo"), "{err}");
        let err = tool
            .execute("t", json!({ "repo": "ocean-os", "job": "j; rm -rf ~" }))
            .await
            .expect_err("a shell-metachar job id must be rejected");
        assert!(err.contains("job id"), "{err}");
    }

    #[tokio::test]
    async fn ship_fetch_clean_validate_job_and_repo_before_running_anything() {
        let ship = OffshoreShipTool { ctx: test_ctx() };
        let err = ship
            .execute("t", json!({ "repo": "ocean-os" }))
            .await
            .expect_err("missing job must error");
        assert!(err.contains("job"), "{err}");

        let clean = OffshoreCleanTool { ctx: test_ctx() };
        let err = clean
            .execute("t", json!({ "job": "j1", "repo": "bad name" }))
            .await
            .expect_err("a repo name with spaces must be rejected");
        assert!(err.contains("repo name"), "{err}");

        let fetch = OffshoreFetchTool { ctx: test_ctx() };
        let err = fetch
            .execute("t", json!({ "job": "j1", "repo": "ocean-os" }))
            .await
            .expect_err("missing dest must error before running git");
        assert!(err.contains("dest"), "{err}");
    }
}
