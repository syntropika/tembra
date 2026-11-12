//! Records the commit of the tembra repository this cargo-tembra is built from, as
//! `TEMBRA_COMMIT`. Outside a checkout, `cargo tembra new` writes a git dependency on the
//! repository at that commit, so that the macro a new crate compiles with is the one this CLI
//! packages.
//!
//! The commit is HEAD of the repository at the workspace root, read with git: a clone, a worktree
//! and the checkout `cargo install --git` builds in all keep their `.git`. Without git, or without
//! `.git` at the root, as in an archive of the sources, the variable is empty and the build goes on.
use std::{path::Path, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2);
    let commit = root.and_then(head).unwrap_or_default();
    println!("cargo:rustc-env=TEMBRA_COMMIT={commit}");
}

/// The full hash of the commit at HEAD of the repository whose root is `root`. The script runs
/// again when HEAD moves: its file changes on a checkout; its log and the branch it names, loose or
/// packed, on a commit.
fn head(root: &Path) -> Option<String> {
    // The repository of these sources, not one around them: an archive of the sources unpacked
    // inside another repository has no `.git` of its own.
    if !root.join(".git").exists() {
        return None;
    }
    let git = |arguments: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(arguments)
            .output()
            .ok()?;
        let text = String::from_utf8(output.stdout).ok()?;
        output.status.success().then(|| text.trim().to_owned())
    };
    let branch = git(&["symbolic-ref", "--quiet", "HEAD"]);
    for name in ["HEAD", "logs/HEAD", "packed-refs"]
        .into_iter()
        .chain(branch.as_deref())
    {
        // Only a file that exists: cargo runs the script on every build for one that does not.
        let file = git(&["rev-parse", "--git-path", name]).map(|path| root.join(path));
        if let Some(file) = file.filter(|file| file.exists()) {
            println!("cargo:rerun-if-changed={}", file.display());
        }
    }
    let commit = git(&["rev-parse", "--verify", "--quiet", "HEAD"])?;
    let full = commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit());
    full.then_some(commit)
}
