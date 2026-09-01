use repo_sandbox_core::snapshot::{
    CleanupPolicy, CommitSha, SnapshotError, SnapshotId, SnapshotOptions, SnapshotOrigin,
    SourceSnapshot, SourceSpec,
};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use tempfile::{Builder, TempDir};

/// Owns a materialized snapshot. Delete-policy snapshots disappear on drop;
/// keep-policy snapshots remain at `path` until an explicit later cleanup.
#[derive(Debug)]
pub struct MaterializedSnapshot {
    pub snapshot: SourceSnapshot,
    path: PathBuf,
    temporary: Option<TempDir>,
}

impl MaterializedSnapshot {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_automatically_cleaned(&self) -> bool {
        self.temporary.is_some()
    }
}

#[derive(Clone, Debug, Default)]
pub struct GitSnapshotter {
    temporary_parent: Option<PathBuf>,
}

impl GitSnapshotter {
    pub fn in_temporary_parent(parent: PathBuf) -> Self {
        Self {
            temporary_parent: Some(parent),
        }
    }

    pub fn create(
        &self,
        source: &SourceSpec,
        options: SnapshotOptions,
    ) -> Result<MaterializedSnapshot, SnapshotError> {
        let staging = self.new_tempdir()?;
        let checkout = staging.path().join("source");
        fs::create_dir(&checkout).map_err(io_error("create isolated snapshot directory"))?;

        let (origin, files) = match source {
            SourceSpec::LocalDirectory(root) => {
                let root =
                    fs::canonicalize(root).map_err(io_error("open local source directory"))?;
                if !root.is_dir() {
                    return Err(SnapshotError::InvalidInput(
                        "local source must be a directory".into(),
                    ));
                }
                ensure_git_root(&root)?;
                let files = collect_repository(&root, &root, Path::new(""), options)?;
                (
                    SnapshotOrigin::Local {
                        canonical_root: root,
                    },
                    files,
                )
            }
            SourceSpec::RemoteGit {
                repository,
                git_ref,
            } => {
                validate_remote_input(repository, git_ref)?;
                // Clone into a different directory than the final copy. This lets
                // the final tree omit all Git and submodule administrative data.
                let clone = staging.path().join("clone");
                git(
                    staging.path(),
                    [
                        OsString::from("clone"),
                        OsString::from("--no-checkout"),
                        OsString::from("--"),
                        OsString::from(repository),
                        clone.as_os_str().to_owned(),
                    ],
                    "clone remote repository",
                )?;
                let commit = resolve_commit(&clone, git_ref)?;
                git(
                    &clone,
                    [
                        OsString::from("checkout"),
                        OsString::from("--detach"),
                        OsString::from("--force"),
                        OsString::from(commit.as_str()),
                    ],
                    "checkout resolved commit",
                )?;
                if options.recurse_submodules {
                    git(
                        &clone,
                        [
                            OsString::from("submodule"),
                            OsString::from("update"),
                            OsString::from("--init"),
                            OsString::from("--recursive"),
                        ],
                        "initialize recursive submodules",
                    )?;
                }
                let clone =
                    fs::canonicalize(&clone).map_err(io_error("resolve cloned worktree"))?;
                let files = collect_repository(&clone, &clone, Path::new(""), options)?;
                (
                    SnapshotOrigin::RemoteGit {
                        repository: redact_repository(repository),
                        requested_ref: git_ref.clone(),
                        commit,
                    },
                    files,
                )
            }
        };

        reject_lfs(&files)?;
        let (id, file_count) = copy_and_digest(files, &checkout)?;
        let (path, temporary) = match options.cleanup {
            CleanupPolicy::Delete => (checkout, Some(staging)),
            CleanupPolicy::Keep => {
                let kept = staging.keep();
                (kept.join("source"), None)
            }
        };
        Ok(MaterializedSnapshot {
            snapshot: SourceSnapshot {
                id,
                origin,
                file_count,
                recurse_submodules: options.recurse_submodules,
            },
            path,
            temporary,
        })
    }

    fn new_tempdir(&self) -> Result<TempDir, SnapshotError> {
        let mut builder = Builder::new();
        builder.prefix("repo-sandbox-source-");
        match &self.temporary_parent {
            Some(parent) => builder.tempdir_in(parent),
            None => builder.tempdir(),
        }
        .map_err(io_error("create private temporary directory"))
    }
}

#[derive(Debug)]
struct SourceFile {
    relative: String,
    source: Option<PathBuf>,
    virtual_content: Option<Vec<u8>>,
    mode: u32,
}

fn ensure_git_root(root: &Path) -> Result<(), SnapshotError> {
    let output = git_output(
        root,
        ["rev-parse", "--is-inside-work-tree"],
        "inspect local Git worktree",
    )?;
    if output.stdout != b"true\n" && output.stdout != b"true\r\n" {
        return Err(SnapshotError::InvalidInput(
            "local source is not a Git worktree".into(),
        ));
    }
    let top_level = git_output(
        root,
        ["rev-parse", "--show-toplevel"],
        "locate local Git worktree",
    )?;
    let top_level = String::from_utf8(top_level.stdout)
        .map_err(|_| SnapshotError::Git("Git returned a non-UTF-8 worktree path".into()))?;
    let top_level = fs::canonicalize(top_level.trim()).map_err(io_error("resolve Git worktree"))?;
    if top_level != root {
        return Err(SnapshotError::InvalidInput(
            "local source must be the root of a Git worktree".into(),
        ));
    }
    Ok(())
}

fn collect_repository(
    security_root: &Path,
    repository_root: &Path,
    prefix: &Path,
    options: SnapshotOptions,
) -> Result<Vec<SourceFile>, SnapshotError> {
    let output = git_output(
        repository_root,
        [
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        "enumerate non-ignored source files",
    )?;
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for raw in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let value = std::str::from_utf8(raw).map_err(|_| {
            SnapshotError::Unsupported("non-UTF-8 Git paths are not supported in v1".into())
        })?;
        let relative = safe_relative_path(value)?;
        if relative.components().any(|part| part.as_os_str() == ".git") {
            continue;
        }
        let source = repository_root.join(&relative);
        let metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error("inspect source file")(error)),
        };
        if metadata.file_type().is_symlink() {
            return Err(SnapshotError::Unsupported(format!(
                "symbolic links are not supported in v1: {}",
                display_safe_path(&prefix.join(&relative))
            )));
        }
        if metadata.is_dir() {
            let Some(object_id) = gitlink_object(repository_root, &relative)? else {
                return Err(SnapshotError::Unsupported(format!(
                    "tracked directory is not a Git submodule: {}",
                    display_safe_path(&prefix.join(&relative))
                )));
            };
            files.push(SourceFile {
                relative: normalized_path(&prefix.join(&relative))?,
                source: None,
                virtual_content: Some(object_id.into_bytes()),
                mode: 0o160000,
            });
            if options.recurse_submodules {
                ensure_git_root(&source).map_err(|_| {
                    SnapshotError::Git(format!(
                        "submodule is not initialized: {}",
                        display_safe_path(&prefix.join(&relative))
                    ))
                })?;
                files.extend(collect_repository(
                    security_root,
                    &source,
                    &prefix.join(&relative),
                    options,
                )?);
            }
            continue;
        }
        if !metadata.is_file() {
            return Err(SnapshotError::Unsupported(format!(
                "special files are not supported: {}",
                display_safe_path(&prefix.join(&relative))
            )));
        }
        let canonical = fs::canonicalize(&source).map_err(io_error("resolve source file"))?;
        if !canonical.starts_with(security_root) {
            return Err(SnapshotError::InvalidInput(
                "source path resolves outside the selected worktree".into(),
            ));
        }
        let manifest_path = normalized_path(&prefix.join(&relative))?;
        if !seen.insert(manifest_path.clone()) {
            return Err(SnapshotError::InvalidInput(format!(
                "duplicate normalized path: {manifest_path}"
            )));
        }
        files.push(SourceFile {
            relative: manifest_path,
            source: Some(canonical),
            virtual_content: None,
            mode: indexed_mode(repository_root, &relative)?
                .unwrap_or_else(|| regular_mode(&metadata)),
        });
    }
    Ok(files)
}

fn safe_relative_path(value: &str) -> Result<PathBuf, SnapshotError> {
    if value.is_empty() || value.contains('\0') {
        return Err(SnapshotError::InvalidInput(
            "empty or NUL source path".into(),
        ));
    }
    let path = PathBuf::from(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(SnapshotError::InvalidInput(format!(
            "unsafe Git path: {}",
            display_safe_path(&path)
        )));
    }
    Ok(path)
}

fn normalized_path(path: &Path) -> Result<String, SnapshotError> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str().ok_or_else(|| {
                SnapshotError::Unsupported("non-UTF-8 paths are not supported in v1".into())
            })?),
            Component::CurDir => {}
            _ => return Err(SnapshotError::InvalidInput("unsafe source path".into())),
        }
    }
    if parts.is_empty() {
        return Err(SnapshotError::InvalidInput("empty source path".into()));
    }
    Ok(parts.join("/"))
}

#[cfg(unix)]
fn regular_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 == 0 {
        0o100644
    } else {
        0o100755
    }
}

#[cfg(not(unix))]
fn regular_mode(_metadata: &fs::Metadata) -> u32 {
    0o100644
}

fn reject_lfs(files: &[SourceFile]) -> Result<(), SnapshotError> {
    for file in files
        .iter()
        .filter(|file| file.relative.ends_with(".gitattributes"))
    {
        let Some(source) = &file.source else { continue };
        let text =
            fs::read_to_string(source).map_err(io_error("inspect Git attributes for LFS"))?;
        for line in text.lines() {
            let line = line.trim();
            if !line.starts_with('#')
                && line
                    .split_ascii_whitespace()
                    .any(|attribute| attribute == "filter=lfs")
            {
                return Err(SnapshotError::Unsupported(
                    "Git LFS sources are not supported in v1".into(),
                ));
            }
        }
    }
    Ok(())
}

fn copy_and_digest(
    mut files: Vec<SourceFile>,
    destination: &Path,
) -> Result<(SnapshotId, usize), SnapshotError> {
    files.sort_by(|left, right| left.relative.as_bytes().cmp(right.relative.as_bytes()));
    let mut manifest = Sha256::new();
    for file in &files {
        let mut content = Sha256::new();
        let mut length = 0_u64;
        if let Some(source) = &file.source {
            let target = destination.join(Path::new(&file.relative));
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(io_error("create snapshot directory"))?;
            }
            let mut input = File::open(source).map_err(io_error("read source file"))?;
            let mut output = File::create(&target).map_err(io_error("create snapshot file"))?;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = input
                    .read(&mut buffer)
                    .map_err(io_error("read source file"))?;
                if count == 0 {
                    break;
                }
                output
                    .write_all(&buffer[..count])
                    .map_err(io_error("write snapshot file"))?;
                content.update(&buffer[..count]);
                length += count as u64;
            }
            apply_mode(&target, file.mode)?;
        } else if let Some(bytes) = &file.virtual_content {
            content.update(bytes);
            length = bytes.len() as u64;
        }
        manifest.update(b"file\0");
        manifest.update(file.relative.as_bytes());
        manifest.update(b"\0");
        manifest.update(file.mode.to_be_bytes());
        manifest.update(length.to_be_bytes());
        manifest.update(content.finalize());
    }
    let digest = format!("{:x}", manifest.finalize());
    Ok((
        SnapshotId::parse(digest)?,
        files.iter().filter(|file| file.source.is_some()).count(),
    ))
}

fn indexed_mode(repository: &Path, relative: &Path) -> Result<Option<u32>, SnapshotError> {
    let output = git_output(
        repository,
        [
            OsString::from("ls-files"),
            OsString::from("--stage"),
            OsString::from("--"),
            relative.as_os_str().to_owned(),
        ],
        "inspect source file mode",
    )?;
    let Some(mode) = output
        .stdout
        .split(|byte| byte.is_ascii_whitespace())
        .next()
    else {
        return Ok(None);
    };
    if mode.is_empty() {
        return Ok(None);
    }
    let mode = std::str::from_utf8(mode)
        .ok()
        .and_then(|value| u32::from_str_radix(value, 8).ok())
        .ok_or_else(|| SnapshotError::Git("Git returned an invalid file mode".into()))?;
    Ok(Some(mode))
}

fn gitlink_object(repository: &Path, relative: &Path) -> Result<Option<String>, SnapshotError> {
    let output = git_output(
        repository,
        [
            OsString::from("ls-files"),
            OsString::from("--stage"),
            OsString::from("--"),
            relative.as_os_str().to_owned(),
        ],
        "inspect Git submodule entry",
    )?;
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| SnapshotError::Git("Git returned non-UTF-8 index data".into()))?;
    let mut fields = text.split_ascii_whitespace();
    if fields.next() != Some("160000") {
        return Ok(None);
    }
    let object = fields
        .next()
        .ok_or_else(|| SnapshotError::Git("Git omitted a submodule object id".into()))?;
    if !(object.len() == 40 || object.len() == 64)
        || !object.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(SnapshotError::Git(
            "Git returned an invalid submodule object id".into(),
        ));
    }
    Ok(Some(object.to_ascii_lowercase()))
}

#[cfg(unix)]
fn apply_mode(path: &Path, mode: u32) -> Result<(), SnapshotError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
        .map_err(io_error("apply snapshot file mode"))
}

#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: u32) -> Result<(), SnapshotError> {
    Ok(())
}

fn resolve_commit(repository: &Path, requested: &str) -> Result<CommitSha, SnapshotError> {
    let candidates = if requested.starts_with("refs/") || requested == "HEAD" {
        vec![requested.to_owned()]
    } else {
        vec![
            requested.to_owned(),
            format!("refs/remotes/origin/{requested}"),
        ]
    };
    for candidate in candidates {
        let revision = format!("{candidate}^{{commit}}");
        let output = git_raw(
            repository,
            [
                OsString::from("rev-parse"),
                OsString::from("--verify"),
                OsString::from("--end-of-options"),
                OsString::from(revision),
            ],
        )?;
        if output.status.success() {
            let value = String::from_utf8(output.stdout)
                .map_err(|_| SnapshotError::Git("Git returned a non-UTF-8 object id".into()))?;
            return CommitSha::parse(value.trim());
        }
    }
    Err(SnapshotError::Git(
        "requested ref did not resolve to a commit".into(),
    ))
}

fn validate_remote_input(repository: &str, git_ref: &str) -> Result<(), SnapshotError> {
    if repository.trim().is_empty() || repository.chars().any(char::is_control) {
        return Err(SnapshotError::InvalidInput(
            "remote repository must not be empty or contain control characters".into(),
        ));
    }
    if git_ref.trim().is_empty() || git_ref.chars().any(char::is_control) {
        return Err(SnapshotError::InvalidInput(
            "Git ref must not be empty or contain control characters".into(),
        ));
    }
    Ok(())
}

fn redact_repository(repository: &str) -> String {
    let Some(scheme) = repository.find("://") else {
        return repository.to_owned();
    };
    let authority_start = scheme + 3;
    let authority_end = repository[authority_start..]
        .find('/')
        .map_or(repository.len(), |offset| authority_start + offset);
    let authority = &repository[authority_start..authority_end];
    let Some(at) = authority.rfind('@') else {
        return repository.to_owned();
    };
    format!(
        "{}<redacted>@{}{}",
        &repository[..authority_start],
        &authority[at + 1..],
        &repository[authority_end..]
    )
}

fn git_output<I, S>(cwd: &Path, arguments: I, operation: &str) -> Result<Output, SnapshotError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_raw(cwd, arguments)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(SnapshotError::Git(format!(
            "{operation} (exit status {})",
            output
                .status
                .code()
                .map_or_else(|| "signal".into(), |code| code.to_string())
        )))
    }
}

fn git<I, S>(cwd: &Path, arguments: I, operation: &str) -> Result<(), SnapshotError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_output(cwd, arguments, operation).map(|_| ())
}

fn git_raw<I, S>(cwd: &Path, arguments: I) -> Result<Output, SnapshotError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git")
        .args(arguments)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .output()
        .map_err(|error| SnapshotError::Git(format!("could not execute Git: {error}")))
}

fn io_error(operation: &'static str) -> impl FnOnce(std::io::Error) -> SnapshotError {
    move |error| SnapshotError::Io(format!("{operation}: {error}"))
}

fn display_safe_path(path: &Path) -> String {
    path.to_string_lossy().replace(['\r', '\n'], "?")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn run_git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn repository() -> TempDir {
        let directory = tempfile::tempdir().unwrap();
        run_git(directory.path(), &["init", "-q"]);
        run_git(directory.path(), &["config", "user.name", "Snapshot Test"]);
        run_git(
            directory.path(),
            &["config", "user.email", "snapshot@example.test"],
        );
        fs::write(directory.path().join("tracked.txt"), "tracked\n").unwrap();
        run_git(directory.path(), &["add", "tracked.txt"]);
        run_git(directory.path(), &["commit", "-qm", "initial"]);
        directory
    }

    fn create_local(root: &Path) -> MaterializedSnapshot {
        GitSnapshotter::default()
            .create(
                &SourceSpec::LocalDirectory(root.to_owned()),
                SnapshotOptions::default(),
            )
            .unwrap()
    }

    #[test]
    fn identical_local_content_has_the_same_digest_and_changes_are_detected() {
        let repo = repository();
        fs::write(repo.path().join("untracked.txt"), "untracked\n").unwrap();
        let first = create_local(repo.path());
        let second = create_local(repo.path());
        assert_eq!(first.snapshot.id, second.snapshot.id);
        assert_eq!(first.snapshot.file_count, 2);

        fs::write(repo.path().join("untracked.txt"), "changed\n").unwrap();
        let content_changed = create_local(repo.path());
        assert_ne!(first.snapshot.id, content_changed.snapshot.id);

        fs::rename(
            repo.path().join("untracked.txt"),
            repo.path().join("renamed.txt"),
        )
        .unwrap();
        let path_changed = create_local(repo.path());
        assert_ne!(content_changed.snapshot.id, path_changed.snapshot.id);
    }

    #[test]
    fn ignore_rules_and_git_metadata_are_excluded() {
        let repo = repository();
        fs::write(repo.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(repo.path().join("ignored.txt"), "secret\n").unwrap();
        let snapshot = create_local(repo.path());
        assert!(!snapshot.path().join("ignored.txt").exists());
        assert!(!snapshot.path().join(".git").exists());
        assert!(snapshot.path().join("tracked.txt").exists());
    }

    #[test]
    fn remote_branch_records_the_commit_and_does_not_move_after_creation() {
        let repo = repository();
        let branch = String::from_utf8(
            git_output(repo.path(), ["branch", "--show-current"], "read branch")
                .unwrap()
                .stdout,
        )
        .unwrap();
        let expected = String::from_utf8(
            git_output(repo.path(), ["rev-parse", "HEAD"], "read head")
                .unwrap()
                .stdout,
        )
        .unwrap();
        let snapshot = GitSnapshotter::default()
            .create(
                &SourceSpec::RemoteGit {
                    repository: repo.path().to_string_lossy().into_owned(),
                    git_ref: branch.trim().into(),
                },
                SnapshotOptions::default(),
            )
            .unwrap();
        let SnapshotOrigin::RemoteGit { commit, .. } = &snapshot.snapshot.origin else {
            panic!("expected remote origin")
        };
        assert_eq!(commit.as_str(), expected.trim());

        fs::write(repo.path().join("tracked.txt"), "later\n").unwrap();
        run_git(repo.path(), &["commit", "-qam", "later"]);
        assert_eq!(commit.as_str(), expected.trim());
        assert_eq!(
            fs::read_to_string(snapshot.path().join("tracked.txt"))
                .unwrap()
                .trim(),
            "tracked"
        );
    }

    #[test]
    fn recursive_initialized_local_submodule_succeeds_and_uninitialized_fails() {
        let child = repository();
        let parent = repository();
        run_git(
            parent.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                child.path().to_str().unwrap(),
                "deps/child",
            ],
        );
        run_git(parent.path(), &["commit", "-qam", "add child"]);
        let snapshot = GitSnapshotter::default()
            .create(
                &SourceSpec::LocalDirectory(parent.path().to_owned()),
                SnapshotOptions {
                    recurse_submodules: true,
                    cleanup: CleanupPolicy::Delete,
                },
            )
            .unwrap();
        assert!(snapshot.path().join("deps/child/tracked.txt").exists());

        let clone = tempfile::tempdir().unwrap();
        run_git(
            clone.path(),
            &[
                "clone",
                "-q",
                parent.path().to_str().unwrap(),
                "uninitialized",
            ],
        );
        let error = GitSnapshotter::default()
            .create(
                &SourceSpec::LocalDirectory(clone.path().join("uninitialized")),
                SnapshotOptions {
                    recurse_submodules: true,
                    cleanup: CleanupPolicy::Delete,
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("submodule is not initialized"));
    }

    #[test]
    fn temporary_cleanup_policy_is_enforced_for_success_and_failure() {
        let repo = repository();
        let parent = tempfile::tempdir().unwrap();
        let snapshotter = GitSnapshotter::in_temporary_parent(parent.path().to_owned());
        let snapshot = snapshotter
            .create(
                &SourceSpec::LocalDirectory(repo.path().to_owned()),
                SnapshotOptions::default(),
            )
            .unwrap();
        let path = snapshot.path().to_owned();
        assert!(path.exists());
        drop(snapshot);
        assert!(!path.exists());

        let error = snapshotter.create(
            &SourceSpec::RemoteGit {
                repository: "definitely-missing-repository".into(),
                git_ref: "main".into(),
            },
            SnapshotOptions::default(),
        );
        assert!(error.is_err());
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);

        let kept = snapshotter
            .create(
                &SourceSpec::LocalDirectory(repo.path().to_owned()),
                SnapshotOptions {
                    recurse_submodules: false,
                    cleanup: CleanupPolicy::Keep,
                },
            )
            .unwrap();
        let kept_path = kept.path().to_owned();
        assert!(!kept.is_automatically_cleaned());
        drop(kept);
        assert!(kept_path.exists());
        fs::remove_dir_all(kept_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn lfs_and_credential_exposure_are_rejected_or_redacted() {
        let repo = repository();
        fs::write(
            repo.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs\n",
        )
        .unwrap();
        fs::write(repo.path().join("asset.bin"), "pointer-ish\n").unwrap();
        let error = GitSnapshotter::default()
            .create(
                &SourceSpec::LocalDirectory(repo.path().to_owned()),
                SnapshotOptions::default(),
            )
            .unwrap_err();
        assert!(matches!(error, SnapshotError::Unsupported(_)));
        assert_eq!(
            redact_repository("https://person:token@example.test/org/repo.git"),
            "https://<redacted>@example.test/org/repo.git"
        );
    }
}
