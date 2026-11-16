//! The files of a git repository whose change may move HEAD to another commit, which the build
//! script watches to record the new one. The CLI's tests share it, to lay out the repositories
//! git writes, a reftable one included, which the git running the tests may not create.
use std::path::{Path, PathBuf};

/// The files that change when HEAD moves, among those that exist: cargo runs the script on every
/// build for a file that does not. `git` is the git directory of the worktree, `common` the one
/// its worktrees share (the same for the main worktree), and `branch` the ref HEAD names, such as
/// `refs/heads/main`, or none when HEAD is detached.
///
/// A repository keeps its refs in files or, from git 2.45 on, in reftable stacks. In files, HEAD
/// changes on a checkout, its log on every move, and the branch's own file, there unless the
/// branch is packed, on a commit; `packed-refs` changes when refs are packed or deleted, never on
/// a commit.
/// In reftable, `HEAD` is a placeholder that never changes, and every update of a ref adds a table
/// to the stack holding the ref and rewrites the stack's `tables.list`: the worktree's own stack
/// for HEAD, the common one for the branches, a single stack in the main worktree.
pub fn files(git: &Path, common: &Path, branch: Option<&str>) -> Vec<PathBuf> {
    let tables = |directory: &Path| directory.join("reftable").join("tables.list");
    let head = [git.join("HEAD"), git.join("logs").join("HEAD"), tables(git)];
    let branch = branch
        .into_iter()
        .flat_map(|branch| [common.join(branch), tables(common)]);
    let mut files = Vec::new();
    for file in head.into_iter().chain(branch) {
        if file.exists() && !files.contains(&file) {
            files.push(file);
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::files;
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    /// A commit, as a branch's own file holds it.
    const COMMIT: &str = "b2189c1a7f3e4d5c6b7a8f9e0d1c2b3a4f5e6d7c\n";

    /// A line of HEAD's log: the commit before, the one after, who, when and why.
    const LOG: &str = "0000000000000000000000000000000000000000 \
        b2189c1a7f3e4d5c6b7a8f9e0d1c2b3a4f5e6d7c Ada <ada@example.com> 1763300000 +0000\t\
        commit (initial): greeter\n";

    /// Write each file, by its path under `directory`, with the directories it lies in.
    fn write(directory: &Path, files: &[(&str, &str)]) {
        for (path, text) in files {
            let file = directory.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text).unwrap();
        }
    }

    /// The list of the tables of the reftable stack in `directory`.
    fn tables(directory: &Path) -> PathBuf {
        directory.join("reftable").join("tables.list")
    }

    #[test]
    fn a_reftable_checkout_is_watched_through_the_tables_list_of_each_stack() {
        // What `git init --ref-format=reftable` writes from git 2.45 on, in the git directory and
        // in each linked worktree's: `HEAD` and the file `refs/heads` only stop a git that does
        // not read reftable, and never change. The refs and their logs are in the tables of a
        // stack; every update adds one and rewrites the stack's `tables.list`.
        let reftable = [
            ("HEAD", "ref: refs/heads/.invalid\n"),
            ("refs/heads", "this repository uses the reftable format\n"),
            (
                "reftable/tables.list",
                "0x000000000001-0x000000000002-6a1fb3c0.ref\n",
            ),
        ];
        let git = crate::scratch("reftable-checkout").join(".git");
        write(&git, &reftable);
        // The main worktree has one stack, for HEAD and the branches, and no log or branch file.
        let main = files(&git, &git, Some("refs/heads/main"));
        assert_eq!(main, [git.join("HEAD"), tables(&git)]);
        // A linked worktree keeps HEAD in a stack of its own; its branch is in the common one.
        let worktree = git.join("worktrees").join("feature");
        write(&worktree, &reftable);
        let linked = files(&worktree, &git, Some("refs/heads/feature"));
        assert_eq!(
            linked,
            [worktree.join("HEAD"), tables(&worktree), tables(&git)]
        );
        // A detached HEAD holds the commit itself.
        let detached = files(&worktree, &git, None);
        assert_eq!(detached, [worktree.join("HEAD"), tables(&worktree)]);
    }

    #[test]
    fn a_files_checkout_is_watched_through_head_its_log_and_the_loose_branch() {
        // A commit writes the branch's own file and HEAD's log; `packed-refs` changes when refs
        // are packed or deleted, never on a commit.
        let git = crate::scratch("files-checkout").join(".git");
        let packed = "# pack-refs with: peeled fully-peeled sorted \n\
            b2189c1a7f3e4d5c6b7a8f9e0d1c2b3a4f5e6d7c refs/remotes/origin/main\n";
        write(
            &git,
            &[("HEAD", "ref: refs/heads/main\n"), ("logs/HEAD", LOG)],
        );
        write(
            &git,
            &[("refs/heads/main", COMMIT), ("packed-refs", packed)],
        );
        let main = files(&git, &git, Some("refs/heads/main"));
        let branch = git.join("refs/heads/main");
        assert_eq!(main, [git.join("HEAD"), git.join("logs/HEAD"), branch]);
        // A linked worktree keeps HEAD and its log; its branch is in the common directory.
        let worktree = git.join("worktrees").join("feature");
        write(
            &worktree,
            &[("HEAD", "ref: refs/heads/feature\n"), ("logs/HEAD", LOG)],
        );
        write(&git, &[("refs/heads/feature", COMMIT)]);
        let linked = files(&worktree, &git, Some("refs/heads/feature"));
        let branch = git.join("refs/heads/feature");
        assert_eq!(
            linked,
            [worktree.join("HEAD"), worktree.join("logs/HEAD"), branch]
        );
    }
}
