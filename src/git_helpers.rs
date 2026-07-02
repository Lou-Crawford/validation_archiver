use std::process::Command;
use std::env;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn check_git_repo(path: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn check_git_dirty(path: &Path) -> bool {
    !Command::new("git")
        .args(["diff", "--quiet"])
        .current_dir(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(true)
}

pub fn check_ssh_agent() -> bool {
    env::var("SSH_AUTH_SOCK").is_ok() && 
    Command::new("ssh-add").arg("-l").status().map(|s| s.success()).unwrap_or(false)
}

pub fn git_commit_and_push(path: &Path) -> std::io::Result<()> {
    // Check for dirty tree
    if check_git_dirty(path) {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "Working tree is dirty. Please commit or stash changes before pushing."));
    }

    // Pull
    let pull = Command::new("git").args(["pull", "--rebase", "origin", "main"]).current_dir(path).status()?;
    if !pull.success() { return Err(std::io::Error::new(std::io::ErrorKind::Other, "Git pull failed")); }

    // Add
    Command::new("git").args(["add", "."]).current_dir(path).status()?;

    // Check for changes
    let status = Command::new("git").args(["diff", "--cached", "--quiet"]).current_dir(path).status()?;
    if status.success() {
        println!("ℹ No changes to commit, skipping Git commit/push.");
        return Ok(());
    }

    // Commit
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let commit_msg = format!("Auto-archive: {}", timestamp);
    let commit = Command::new("git").args(["commit", "-m", &commit_msg]).current_dir(path).status()?;
    if !commit.success() { return Err(std::io::Error::new(std::io::ErrorKind::Other, "Git commit failed")); }

    // Push
    let push = Command::new("git").args(["push", "origin", "main"]).current_dir(path).status()?;
    if !push.success() { return Err(std::io::Error::new(std::io::ErrorKind::Other, "Git push failed")); }
    
    println!("🚀 Changes pushed to GitHub.");
    Ok(())
}
