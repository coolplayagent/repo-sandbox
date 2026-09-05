use crate::buildkit::{Cancellation, NeverCancelled};
use repo_sandbox_core::snapshot::{
    CleanupPolicy, CommitSha, ExternalSecret, GitAuthentication, SnapshotError, SnapshotId,
    SnapshotOptions, SnapshotOrigin, SourceSnapshot, SourceSpec,
};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;
use tempfile::{Builder, TempDir};

/// Owns a materialized snapshot. Delete-policy snapshots disappear on drop;
/// keep-policy snapshots remain at `path` until an explicit later cleanup.
#[derive(Debug)]
pub struct MaterializedSnapshot {
    pub snapshot: SourceSnapshot,
    path: PathBuf,
    temporary: Option<TempDir>,
    manifest: Vec<SnapshotManifestEntry>,
}

impl MaterializedSnapshot {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_automatically_cleaned(&self) -> bool {
        self.temporary.is_some()
    }

    /// Convert a task-owned delete-on-drop snapshot into retained diagnostics.
    /// Callers use this only after a failed run when `--keep-on-failure` is set.
    /// Local user repositories are never owned by this guard and are never removed.
    pub fn retain_on_failure(&mut self) {
        let Some(temporary) = self.temporary.take() else {
            return;
        };
        let kept = temporary.keep();
        // Keep the canonical source path already established at creation.
        let _ = kept;
    }

    /// Recompute the #5 normalized manifest while copying the snapshot.
    ///
    /// This detects same-file-count content, path, and (where representable)
    /// mode changes made after materialization. The destination may contain
    /// partial data on error and must remain private until this returns `Ok`.
    pub fn copy_verified_to(&self, destination: &Path) -> Result<usize, SnapshotError> {
        self.copy_verified_to_cancellable(destination, &NeverCancelled)
    }
    pub fn copy_verified_to_cancellable(
        &self,
        destination: &Path,
        cancellation: &dyn Cancellation,
    ) -> Result<usize, SnapshotError> {
        copy_and_verify_materialized(
            &self.path,
            destination,
            &self.manifest,
            &self.snapshot.id,
            cancellation,
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct GitSnapshotter {
    temporary_parent: Option<PathBuf>,
    authentication: GitAuthentication,
}

impl GitSnapshotter {
    pub fn in_temporary_parent(parent: PathBuf) -> Self {
        Self {
            temporary_parent: Some(parent),
            authentication: GitAuthentication::None,
        }
    }

    pub fn with_authentication(mut self, authentication: GitAuthentication) -> Self {
        self.authentication = authentication;
        self
    }

    pub fn create(
        &self,
        source: &SourceSpec,
        options: SnapshotOptions,
    ) -> Result<MaterializedSnapshot, SnapshotError> {
        self.create_cancellable(source, options, &NeverCancelled)
    }

    pub fn create_cancellable(
        &self,
        source: &SourceSpec,
        options: SnapshotOptions,
        cancellation: &dyn Cancellation,
    ) -> Result<MaterializedSnapshot, SnapshotError> {
        if options.recurse_submodules
            && matches!(source, SourceSpec::RemoteGit { .. })
            && matches!(&self.authentication, GitAuthentication::HttpsToken { .. })
        {
            return Err(SnapshotError::InvalidInput(
                "--recurse-submodules with an HTTPS token requires separately scoped submodule credentials, which are not supported in v1".into(),
            ));
        }
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
                ensure_git_root(&root, cancellation)?;
                let files = collect_repository(
                    &root,
                    &root,
                    Path::new(""),
                    options,
                    ModePolicy::LocalWorktree,
                    cancellation,
                )?;
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
                let authentication = AuthenticationContext::prepare(
                    &self.authentication,
                    staging.path(),
                    repository,
                )?;
                git_remote(
                    staging.path(),
                    [
                        OsString::from("clone"),
                        OsString::from("--no-checkout"),
                        OsString::from("--"),
                        OsString::from(repository),
                        clone.as_os_str().to_owned(),
                    ],
                    "clone remote repository",
                    &authentication,
                    cancellation,
                )?;
                let resolved_ref = if git_ref.starts_with("refs/") {
                    git_remote(
                        &clone,
                        [
                            OsString::from("fetch"),
                            OsString::from("--no-tags"),
                            OsString::from("--"),
                            OsString::from(repository),
                            OsString::from(format!("+{git_ref}:refs/repo-sandbox/requested")),
                        ],
                        "fetch requested remote ref",
                        &authentication,
                        cancellation,
                    )?;
                    "refs/repo-sandbox/requested"
                } else {
                    git_ref.as_str()
                };
                let commit = resolve_commit(&clone, resolved_ref, cancellation)?;
                git(
                    &clone,
                    [
                        OsString::from("checkout"),
                        OsString::from("--detach"),
                        OsString::from("--force"),
                        OsString::from(commit.as_str()),
                    ],
                    "checkout resolved commit",
                    cancellation,
                )?;
                if options.recurse_submodules {
                    git_remote(
                        &clone,
                        [
                            OsString::from("submodule"),
                            OsString::from("update"),
                            OsString::from("--init"),
                            OsString::from("--recursive"),
                        ],
                        "initialize recursive submodules",
                        &authentication,
                        cancellation,
                    )?;
                }
                let clone =
                    fs::canonicalize(&clone).map_err(io_error("resolve cloned worktree"))?;
                let files = collect_repository(
                    &clone,
                    &clone,
                    Path::new(""),
                    options,
                    ModePolicy::CommittedCheckout,
                    cancellation,
                )?;
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

        reject_lfs(&files, cancellation)?;
        let (id, file_count, manifest) = copy_and_digest(files, &checkout, cancellation)?;
        let (path, temporary) = match options.cleanup {
            CleanupPolicy::Delete => (checkout, Some(staging)),
            CleanupPolicy::Keep => {
                let kept = staging.keep();
                (kept.join("source"), None)
            }
        };
        let path = fs::canonicalize(path).map_err(io_error("bind private snapshot root"))?;
        Ok(MaterializedSnapshot {
            snapshot: SourceSnapshot {
                id,
                origin,
                file_count,
                recurse_submodules: options.recurse_submodules,
            },
            path,
            temporary,
            manifest,
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

#[derive(Clone, Debug)]
struct SnapshotManifestEntry {
    relative: String,
    virtual_content: Option<Vec<u8>>,
    mode: u32,
}

#[derive(Clone, Copy, Debug)]
enum ModePolicy {
    /// Reflect unstaged chmod changes where the host filesystem represents them.
    LocalWorktree,
    /// Use the selected commit's index mode, independent of checkout behavior.
    CommittedCheckout,
}

#[derive(Debug)]
struct IndexEntry {
    mode: u32,
    object_id: String,
}

fn ensure_git_root(root: &Path, cancellation: &dyn Cancellation) -> Result<(), SnapshotError> {
    let output = git_output_cancellable(
        root,
        ["rev-parse", "--is-inside-work-tree"],
        "inspect local Git worktree",
        cancellation,
    )?;
    if output.stdout != b"true\n" && output.stdout != b"true\r\n" {
        return Err(SnapshotError::InvalidInput(
            "local source is not a Git worktree".into(),
        ));
    }
    let top_level = git_output_cancellable(
        root,
        ["rev-parse", "--show-toplevel"],
        "locate local Git worktree",
        cancellation,
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
    mode_policy: ModePolicy,
    cancellation: &dyn Cancellation,
) -> Result<Vec<SourceFile>, SnapshotError> {
    let output = git_output_cancellable(
        repository_root,
        [
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        "enumerate non-ignored source files",
        cancellation,
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
        if relative.components().next().is_some_and(|part| {
            let name = part.as_os_str();
            #[cfg(windows)]
            {
                name.to_string_lossy().eq_ignore_ascii_case(".repo-sandbox")
            }
            #[cfg(not(windows))]
            {
                name == ".repo-sandbox"
            }
        }) {
            continue;
        }
        if relative.components().any(|part| part.as_os_str() == ".git") {
            continue;
        }
        let index_entry = index_entry(repository_root, &relative, cancellation)?;
        if index_entry
            .as_ref()
            .is_some_and(|entry| entry.mode == 0o120000)
        {
            return Err(SnapshotError::Unsupported(format!(
                "symbolic links are not supported in v1: {}",
                display_safe_path(&prefix.join(&relative))
            )));
        }
        let source = repository_root.join(&relative);
        if let Some(entry) = index_entry.as_ref().filter(|entry| entry.mode == 0o160000) {
            files.push(SourceFile {
                relative: normalized_path(&prefix.join(&relative))?,
                source: None,
                virtual_content: Some(entry.object_id.as_bytes().to_vec()),
                mode: entry.mode,
            });
            if options.recurse_submodules {
                ensure_git_root(&source, cancellation).map_err(|_| {
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
                    mode_policy,
                    cancellation,
                )?);
            }
            continue;
        }
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
            return Err(SnapshotError::Unsupported(format!(
                "directories emitted as files are not supported: {}",
                display_safe_path(&prefix.join(&relative))
            )));
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
            mode: snapshot_file_mode(mode_policy, index_entry.as_ref(), &metadata),
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

#[cfg(unix)]
fn snapshot_file_mode(
    policy: ModePolicy,
    index_entry: Option<&IndexEntry>,
    metadata: &fs::Metadata,
) -> u32 {
    match policy {
        ModePolicy::LocalWorktree => regular_mode(metadata),
        ModePolicy::CommittedCheckout => index_entry
            .map(|entry| entry.mode)
            .unwrap_or_else(|| regular_mode(metadata)),
    }
}

#[cfg(not(unix))]
fn snapshot_file_mode(
    _policy: ModePolicy,
    index_entry: Option<&IndexEntry>,
    metadata: &fs::Metadata,
) -> u32 {
    // Windows has no portable executable bit. Git's index is the authoritative
    // representation for tracked files; untracked files use the regular default.
    index_entry
        .map(|entry| entry.mode)
        .unwrap_or_else(|| regular_mode(metadata))
}

fn ensure_snapshot_not_cancelled(cancellation: &dyn Cancellation) -> Result<(), SnapshotError> {
    if cancellation.is_cancelled() {
        Err(SnapshotError::Io("snapshot creation cancelled".into()))
    } else {
        Ok(())
    }
}

fn open_source_file(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        unsafe extern "C" {
            fn openat(fd: i32, path: *const std::ffi::c_char, flags: i32, ...) -> i32;
        }
        #[cfg(target_os = "linux")]
        const NOFOLLOW: i32 = 0x0002_0000;
        #[cfg(not(target_os = "linux"))]
        const NOFOLLOW: i32 = 0x0000_0100;
        #[cfg(target_os = "linux")]
        const DIRECTORY: i32 = 0x0001_0000;
        #[cfg(not(target_os = "linux"))]
        const DIRECTORY: i32 = 0x0010_0000;
        #[cfg(target_os = "linux")]
        const CLOEXEC: i32 = 0x0008_0000;
        #[cfg(not(target_os = "linux"))]
        const CLOEXEC: i32 = 0x0100_0000;
        let mut current = File::open("/")?;
        let parts = path
            .components()
            .filter_map(|part| match part {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect::<Vec<_>>();
        for (index, part) in parts.iter().enumerate() {
            let name = CString::new(part.as_bytes()).map_err(io::Error::other)?;
            let flags = NOFOLLOW
                | CLOEXEC
                | if index + 1 == parts.len() {
                    0
                } else {
                    DIRECTORY
                };
            let fd = unsafe { openat(current.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            current = unsafe { File::from_raw_fd(fd) };
        }
        if !current.metadata()?.is_file() {
            return Err(io::Error::other("source is not a regular file"));
        }
        Ok(current)
    }
    #[cfg(windows)]
    {
        use std::os::windows::{ffi::OsStringExt, fs::OpenOptionsExt, io::AsRawHandle};
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(0x0020_0000)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("source is not a regular file"));
        }
        unsafe extern "system" {
            fn GetFinalPathNameByHandleW(
                handle: *mut std::ffi::c_void,
                buffer: *mut u16,
                count: u32,
                flags: u32,
            ) -> u32;
        }
        let mut buffer = vec![0_u16; 32768];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                0,
            )
        };
        if length == 0 || length as usize >= buffer.len() {
            return Err(io::Error::last_os_error());
        }
        let actual = PathBuf::from(OsString::from_wide(&buffer[..length as usize]));
        let expected = fs::canonicalize(path)?;
        if !actual
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected.to_string_lossy())
            || !actual
                .to_string_lossy()
                .trim_start_matches(r"\\?\")
                .eq_ignore_ascii_case(path.to_string_lossy().trim_start_matches(r"\\?\"))
        {
            return Err(io::Error::other("source path changed while opening"));
        }
        Ok(file)
    }
}

fn reject_lfs(files: &[SourceFile], cancellation: &dyn Cancellation) -> Result<(), SnapshotError> {
    for file in files {
        ensure_snapshot_not_cancelled(cancellation)?;
        if let Some(source) = &file.source {
            let mut input = open_source_file(source).map_err(io_error("inspect LFS pointer"))?;
            let mut header = [0_u8; 128];
            let count = input
                .read(&mut header)
                .map_err(io_error("inspect LFS pointer"))?;
            if header[..count].starts_with(b"version https://git-lfs.github.com/spec/v1") {
                return Err(SnapshotError::Unsupported(
                    "Git LFS sources are not supported in v1".into(),
                ));
            }
        }
    }
    for file in files
        .iter()
        .filter(|file| file.relative.ends_with(".gitattributes"))
    {
        ensure_snapshot_not_cancelled(cancellation)?;
        let Some(source) = &file.source else { continue };
        let mut input =
            open_source_file(source).map_err(io_error("inspect Git attributes for LFS"))?;
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            ensure_snapshot_not_cancelled(cancellation)?;
            let count = input
                .read(&mut buffer)
                .map_err(io_error("inspect Git attributes for LFS"))?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| SnapshotError::InvalidInput(".gitattributes is not valid UTF-8".into()))?;
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
    cancellation: &dyn Cancellation,
) -> Result<(SnapshotId, usize, Vec<SnapshotManifestEntry>), SnapshotError> {
    files.sort_by(|left, right| left.relative.as_bytes().cmp(right.relative.as_bytes()));
    let mut manifest = Sha256::new();
    for file in &files {
        ensure_snapshot_not_cancelled(cancellation)?;
        let mut content = Sha256::new();
        let mut length = 0_u64;
        if let Some(source) = &file.source {
            let target = destination.join(Path::new(&file.relative));
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(io_error("create snapshot directory"))?;
            }
            let mut input = open_source_file(source).map_err(io_error("read source file"))?;
            let mut output = File::create(&target).map_err(io_error("create snapshot file"))?;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                ensure_snapshot_not_cancelled(cancellation)?;
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
        hash_manifest_entry(
            &mut manifest,
            &file.relative,
            file.mode,
            length,
            content.finalize(),
        );
    }
    let digest = format!("{:x}", manifest.finalize());
    let normalized_manifest = files
        .iter()
        .map(|file| SnapshotManifestEntry {
            relative: file.relative.clone(),
            virtual_content: file.virtual_content.clone(),
            mode: file.mode,
        })
        .collect();
    Ok((
        SnapshotId::parse(digest)?,
        files.iter().filter(|file| file.source.is_some()).count(),
        normalized_manifest,
    ))
}

fn copy_and_verify_materialized(
    source: &Path,
    destination: &Path,
    expected: &[SnapshotManifestEntry],
    expected_id: &SnapshotId,
    cancellation: &dyn Cancellation,
) -> Result<usize, SnapshotError> {
    let mut actual = Vec::new();
    collect_materialized_files(source, Path::new(""), &mut actual, cancellation)?;
    actual.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let expected_paths = expected
        .iter()
        .filter(|entry| entry.virtual_content.is_none())
        .map(|entry| entry.relative.as_str())
        .collect::<Vec<_>>();
    let actual_paths = actual
        .iter()
        .map(|(relative, _, _)| relative.as_str())
        .collect::<Vec<_>>();
    if actual_paths != expected_paths {
        return Err(SnapshotError::InvalidInput(
            "materialized snapshot paths changed after creation".into(),
        ));
    }

    let actual_by_path = actual
        .into_iter()
        .map(|entry| (entry.0.clone(), entry))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut digest = Sha256::new();
    for entry in expected {
        ensure_snapshot_not_cancelled(cancellation)?;
        let (length, content_digest) = if let Some(content) = &entry.virtual_content {
            (content.len() as u64, Sha256::digest(content))
        } else {
            let (_, path, actual_mode) = &actual_by_path[&entry.relative];
            if mode_changed(entry.mode, *actual_mode) {
                return Err(SnapshotError::InvalidInput(format!(
                    "materialized snapshot mode changed: {}",
                    entry.relative
                )));
            }
            let target = destination.join(Path::new(&entry.relative));
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(io_error("create verified snapshot directory"))?;
            }
            let mut input =
                open_source_file(path).map_err(io_error("read materialized snapshot file"))?;
            let mut output =
                File::create(&target).map_err(io_error("copy verified snapshot file"))?;
            let mut content = Sha256::new();
            let mut length = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                ensure_snapshot_not_cancelled(cancellation)?;
                let count = input
                    .read(&mut buffer)
                    .map_err(io_error("read materialized snapshot file"))?;
                if count == 0 {
                    break;
                }
                output
                    .write_all(&buffer[..count])
                    .map_err(io_error("copy verified snapshot file"))?;
                content.update(&buffer[..count]);
                length += count as u64;
            }
            apply_mode(&target, entry.mode)?;
            (length, content.finalize())
        };
        hash_manifest_entry(
            &mut digest,
            &entry.relative,
            entry.mode,
            length,
            content_digest,
        );
    }
    let actual_id = SnapshotId::parse(format!("{:x}", digest.finalize()))?;
    if &actual_id != expected_id {
        return Err(SnapshotError::InvalidInput(format!(
            "materialized snapshot digest changed: expected {expected_id}, got {actual_id}"
        )));
    }
    Ok(expected_paths.len())
}

fn collect_materialized_files(
    root: &Path,
    prefix: &Path,
    files: &mut Vec<(String, PathBuf, u32)>,
    cancellation: &dyn Cancellation,
) -> Result<(), SnapshotError> {
    ensure_snapshot_not_cancelled(cancellation)?;
    for entry in fs::read_dir(root).map_err(io_error("read materialized snapshot"))? {
        ensure_snapshot_not_cancelled(cancellation)?;
        let entry = entry.map_err(io_error("read materialized snapshot entry"))?;
        let relative = prefix.join(entry.file_name());
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(io_error("inspect materialized snapshot entry"))?;
        if metadata.file_type().is_symlink() {
            return Err(SnapshotError::Unsupported(format!(
                "symbolic links are not supported in a materialized snapshot: {}",
                display_safe_path(&relative)
            )));
        }
        if metadata.is_dir() {
            collect_materialized_files(&entry.path(), &relative, files, cancellation)?;
        } else if metadata.is_file() {
            files.push((
                normalized_path(&relative)?,
                entry.path(),
                regular_mode(&metadata),
            ));
        } else {
            return Err(SnapshotError::Unsupported(format!(
                "special files are not supported in a materialized snapshot: {}",
                display_safe_path(&relative)
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn mode_changed(expected: u32, actual: u32) -> bool {
    expected != actual
}

#[cfg(not(unix))]
fn mode_changed(_expected: u32, _actual: u32) -> bool {
    false
}

fn hash_manifest_entry(
    manifest: &mut Sha256,
    relative: &str,
    mode: u32,
    length: u64,
    content_digest: impl AsRef<[u8]>,
) {
    manifest.update(b"file\0");
    manifest.update(relative.as_bytes());
    manifest.update(b"\0");
    manifest.update(mode.to_be_bytes());
    manifest.update(length.to_be_bytes());
    manifest.update(content_digest);
}

fn index_entry(
    repository: &Path,
    relative: &Path,
    cancellation: &dyn Cancellation,
) -> Result<Option<IndexEntry>, SnapshotError> {
    let output = git_output_cancellable(
        repository,
        [
            OsString::from("ls-files"),
            OsString::from("--stage"),
            OsString::from("--"),
            relative.as_os_str().to_owned(),
        ],
        "inspect source file mode",
        cancellation,
    )?;
    if output.stdout.is_empty() {
        return Ok(None);
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| SnapshotError::Git("Git returned non-UTF-8 index data".into()))?;
    let mut fields = text.split_ascii_whitespace();
    let mode = fields
        .next()
        .and_then(|value| u32::from_str_radix(value, 8).ok())
        .ok_or_else(|| SnapshotError::Git("Git returned an invalid file mode".into()))?;
    let object = fields
        .next()
        .ok_or_else(|| SnapshotError::Git("Git omitted an index object id".into()))?;
    if !(object.len() == 40 || object.len() == 64)
        || !object.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(SnapshotError::Git(
            "Git returned an invalid object id".into(),
        ));
    }
    Ok(Some(IndexEntry {
        mode,
        object_id: object.to_ascii_lowercase(),
    }))
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

fn resolve_commit(
    repository: &Path,
    requested: &str,
    cancellation: &dyn Cancellation,
) -> Result<CommitSha, SnapshotError> {
    let candidates = if let Some(branch) = requested.strip_prefix("refs/heads/") {
        vec![
            requested.to_owned(),
            format!("refs/remotes/origin/{branch}"),
        ]
    } else if requested.starts_with("refs/") || requested == "HEAD" {
        vec![requested.to_owned()]
    } else {
        vec![
            requested.to_owned(),
            format!("refs/remotes/origin/{requested}"),
        ]
    };
    for candidate in candidates {
        let revision = format!("{candidate}^{{commit}}");
        let output = git_raw_cancellable(
            repository,
            [
                OsString::from("rev-parse"),
                OsString::from("--verify"),
                OsString::from("--end-of-options"),
                OsString::from(revision),
            ],
            cancellation,
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

#[derive(Default)]
struct AuthenticationContext {
    environment: Vec<(OsString, OsString)>,
    secrets: Vec<String>,
}

impl AuthenticationContext {
    fn prepare(
        authentication: &GitAuthentication,
        temporary_root: &Path,
        repository: &str,
    ) -> Result<Self, SnapshotError> {
        match authentication {
            GitAuthentication::None => Ok(Self {
                environment: unauthenticated_environment(temporary_root)?,
                secrets: Vec::new(),
            }),
            GitAuthentication::SshAgent { known_hosts } => {
                require_ssh_repository(repository)?;
                let environment = ssh_environment(None, known_hosts.as_deref(), temporary_root)?;
                Ok(Self {
                    environment,
                    secrets: Vec::new(),
                })
            }
            GitAuthentication::SshKey {
                private_key,
                known_hosts,
            } => {
                require_ssh_repository(repository)?;
                require_regular_file(private_key, "SSH private key")?;
                let environment =
                    ssh_environment(Some(private_key), known_hosts.as_deref(), temporary_root)?;
                Ok(Self {
                    environment,
                    secrets: Vec::new(),
                })
            }
            GitAuthentication::HttpsToken { username, token } => {
                require_https_repository(repository)?;
                if username.is_empty() || username.chars().any(char::is_control) {
                    return Err(SnapshotError::InvalidInput(
                        "HTTPS token username must not be empty or contain control characters"
                            .into(),
                    ));
                }
                let token = resolve_secret(token)?;
                if token.contains(['\r', '\n']) {
                    return Err(SnapshotError::Authentication(
                        "external HTTPS token must be a single line".into(),
                    ));
                }
                let askpass = write_askpass(temporary_root)?;
                Ok(Self {
                    environment: {
                        let mut environment = unauthenticated_environment(temporary_root)?;
                        environment.extend(vec![
                            ("GIT_ASKPASS".into(), askpass.into_os_string()),
                            ("REPO_SANDBOX_GIT_TOKEN".into(), token.as_str().into()),
                            // A short-lived token must never be approved into a configured
                            // persistent helper.
                            ("GIT_CONFIG_COUNT".into(), "2".into()),
                            ("GIT_CONFIG_KEY_0".into(), "credential.helper".into()),
                            ("GIT_CONFIG_VALUE_0".into(), "".into()),
                            ("GIT_CONFIG_KEY_1".into(), "credential.username".into()),
                            ("GIT_CONFIG_VALUE_1".into(), username.into()),
                        ]);
                        environment
                    },
                    secrets: vec![token],
                })
            }
            GitAuthentication::HttpsCredentialHelper => {
                require_https_repository(repository)?;
                let config = temporary_root.join("https-no-ssh-config");
                fs::write(&config, "Host *\n  BatchMode yes\n  IdentitiesOnly yes\n  IdentityAgent none\n  IdentityFile none\n  StrictHostKeyChecking yes\n").map_err(io_error("write HTTPS SSH isolation"))?;
                Ok(Self {
                    environment: vec![
                        (
                            "GIT_SSH_COMMAND".into(),
                            "ssh -F \"$REPO_SANDBOX_SSH_CONFIG\"".into(),
                        ),
                        (
                            "REPO_SANDBOX_SSH_CONFIG".into(),
                            ssh_config_path(&config)?.into(),
                        ),
                        ("GIT_SSH_VARIANT".into(), "ssh".into()),
                    ],
                    secrets: Vec::new(),
                })
            }
        }
    }
}

fn require_ssh_repository(repository: &str) -> Result<(), SnapshotError> {
    if repository.starts_with("ssh://")
        || (!repository.contains("://") && repository.contains('@') && repository.contains(':'))
    {
        Ok(())
    } else {
        Err(SnapshotError::InvalidInput(
            "SSH authentication requires an ssh:// or user@host:path repository".into(),
        ))
    }
}

fn require_https_repository(repository: &str) -> Result<(), SnapshotError> {
    if repository.starts_with("https://") {
        Ok(())
    } else {
        Err(SnapshotError::InvalidInput(
            "HTTPS authentication requires an https:// repository".into(),
        ))
    }
}

fn require_regular_file(path: &Path, description: &str) -> Result<(), SnapshotError> {
    if path.is_file() {
        Ok(())
    } else {
        Err(SnapshotError::InvalidInput(format!(
            "{description} reference is not a regular file"
        )))
    }
}

fn resolve_secret(reference: &ExternalSecret) -> Result<String, SnapshotError> {
    let value = match reference {
        ExternalSecret::Environment(name) => {
            if name.is_empty() || name.chars().any(char::is_control) {
                return Err(SnapshotError::InvalidInput(
                    "secret environment reference is invalid".into(),
                ));
            }
            env::var(name).map_err(|_| {
                SnapshotError::Authentication(format!(
                    "secret environment reference `{name}` is unavailable"
                ))
            })?
        }
        ExternalSecret::File(path) => {
            require_regular_file(path, "secret file")?;
            fs::read_to_string(path)
                .map_err(|error| {
                    SnapshotError::Authentication(format!("read secret file: {error}"))
                })?
                .trim_end_matches(['\r', '\n'])
                .to_owned()
        }
    };
    if value.is_empty() {
        Err(SnapshotError::Authentication(
            "external secret resolved to an empty value".into(),
        ))
    } else {
        Ok(value)
    }
}

fn ssh_environment(
    private_key: Option<&Path>,
    known_hosts: Option<&Path>,
    temporary_root: &Path,
) -> Result<Vec<(OsString, OsString)>, SnapshotError> {
    if let Some(path) = known_hosts {
        require_regular_file(path, "known_hosts")?;
    }
    let config = temporary_root.join("ssh-config");
    let mut contents = String::from("Host *\n  BatchMode yes\n  StrictHostKeyChecking yes\n");
    if let Some(path) = private_key {
        contents.push_str(&format!(
            "  IdentityFile \"{}\"\n  IdentitiesOnly yes\n",
            ssh_config_path(path)?
        ));
    }
    if let Some(path) = known_hosts {
        contents.push_str(&format!(
            "  UserKnownHostsFile \"{}\"\n",
            ssh_config_path(path)?
        ));
    }
    fs::write(&config, contents).map_err(io_error("write temporary SSH configuration"))?;
    restrict_file(&config)?;
    // Git asks a shell to parse GIT_SSH_COMMAND. Keep its text constant: the
    // untrusted path is expanded from the environment inside double quotes,
    // and expansion results are not parsed again as shell syntax.
    Ok(vec![
        (
            "GIT_SSH_COMMAND".into(),
            "ssh -F \"$REPO_SANDBOX_SSH_CONFIG\"".into(),
        ),
        (
            "REPO_SANDBOX_SSH_CONFIG".into(),
            ssh_config_path(&config)?.into(),
        ),
        ("GIT_SSH_VARIANT".into(), "ssh".into()),
    ])
}

fn unauthenticated_environment(
    temporary_root: &Path,
) -> Result<Vec<(OsString, OsString)>, SnapshotError> {
    let home = temporary_root.join("unauthenticated-home");
    fs::create_dir(&home).map_err(io_error("create unauthenticated Git home"))?;
    let global_config = temporary_root.join("unauthenticated-gitconfig");
    fs::write(&global_config, b"").map_err(io_error("write isolated Git config"))?;
    let config = temporary_root.join("ssh-no-credentials-config");
    fs::write(
        &config,
        "Host *\n  BatchMode yes\n  IdentitiesOnly yes\n  IdentityAgent none\n  IdentityFile none\n  StrictHostKeyChecking yes\n",
    )
    .map_err(io_error("write unauthenticated SSH configuration"))?;
    restrict_file(&config)?;
    Ok(vec![
        ("HOME".into(), home.as_os_str().to_owned()),
        ("USERPROFILE".into(), home.as_os_str().to_owned()),
        ("XDG_CONFIG_HOME".into(), home.as_os_str().to_owned()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_PARAMETERS".into(), "".into()),
        ("GIT_CONFIG_GLOBAL".into(), global_config.into_os_string()),
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "credential.helper".into()),
        ("GIT_CONFIG_VALUE_0".into(), "".into()),
        // Preserve proxy routing without inheriting credentials embedded in URLs.
        ("HTTP_PROXY".into(), credential_free_proxy("HTTP_PROXY")),
        ("HTTPS_PROXY".into(), credential_free_proxy("HTTPS_PROXY")),
        ("ALL_PROXY".into(), credential_free_proxy("ALL_PROXY")),
        ("http_proxy".into(), credential_free_proxy("http_proxy")),
        ("https_proxy".into(), credential_free_proxy("https_proxy")),
        ("all_proxy".into(), credential_free_proxy("all_proxy")),
        (
            "GIT_SSH_COMMAND".into(),
            "ssh -F \"$REPO_SANDBOX_SSH_CONFIG\"".into(),
        ),
        (
            "REPO_SANDBOX_SSH_CONFIG".into(),
            ssh_config_path(&config)?.into(),
        ),
        ("GIT_SSH_VARIANT".into(), "ssh".into()),
    ])
}

fn credential_free_proxy(name: &str) -> OsString {
    env::var_os(name)
        .filter(|value| !value.to_string_lossy().contains('@'))
        .unwrap_or_default()
}

fn ssh_config_path(path: &Path) -> Result<String, SnapshotError> {
    let value = path.to_string_lossy().replace('\\', "/");
    if value.contains(['\r', '\n', '"']) {
        Err(SnapshotError::InvalidInput(
            "SSH file reference contains unsupported characters".into(),
        ))
    } else {
        Ok(value)
    }
}

#[cfg(unix)]
fn write_askpass(root: &Path) -> Result<PathBuf, SnapshotError> {
    let path = root.join("git-askpass");
    fs::write(
        &path,
        "#!/bin/sh\nprintf '%s\\n' \"$REPO_SANDBOX_GIT_TOKEN\"\n",
    )
    .map_err(io_error("write temporary Git askpass helper"))?;
    restrict_file(&path)?;
    Ok(path)
}

#[cfg(windows)]
fn write_askpass(root: &Path) -> Result<PathBuf, SnapshotError> {
    let path = root.join("git-askpass.cmd");
    fs::write(
        &path,
        "@echo off\r\npowershell.exe -NoLogo -NoProfile -NonInteractive -Command \"[Console]::Out.WriteLine([Environment]::GetEnvironmentVariable('REPO_SANDBOX_GIT_TOKEN'))\"\r\n",
    )
    .map_err(io_error("write temporary Git askpass helper"))?;
    restrict_file(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<(), SnapshotError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(io_error("restrict temporary credential helper permissions"))
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> Result<(), SnapshotError> {
    // tempfile creates the containing directory for the current user. Windows
    // inherits that directory's ACL; the file is never persisted beyond it.
    Ok(())
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
    if repository.starts_with("http://") || repository.starts_with("https://") {
        let authority = repository
            .split_once("://")
            .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
            .unwrap_or_default();
        if authority.contains('@') {
            return Err(SnapshotError::InvalidInput(
                "HTTP repository URLs must not contain inline credentials".into(),
            ));
        }
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

#[cfg(test)]
fn git_output<I, S>(cwd: &Path, arguments: I, operation: &str) -> Result<Output, SnapshotError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_output_cancellable(cwd, arguments, operation, &NeverCancelled)
}

fn git_output_cancellable<I, S>(
    cwd: &Path,
    arguments: I,
    operation: &str,
    cancellation: &dyn Cancellation,
) -> Result<Output, SnapshotError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_raw_cancellable(cwd, arguments, cancellation)?;
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

fn git<I, S>(
    cwd: &Path,
    arguments: I,
    operation: &str,
    cancellation: &dyn Cancellation,
) -> Result<(), SnapshotError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_output_cancellable(cwd, arguments, operation, cancellation).map(|_| ())
}

fn git_remote<I, S>(
    cwd: &Path,
    arguments: I,
    operation: &str,
    authentication: &AuthenticationContext,
    cancellation: &dyn Cancellation,
) -> Result<(), SnapshotError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_raw_with_environment_cancellable(
        cwd,
        arguments,
        &authentication.environment,
        cancellation,
    )?;
    if output.status.success() {
        Ok(())
    } else {
        Err(classify_remote_failure(
            operation,
            &output.stderr,
            &authentication.secrets,
        ))
    }
}

fn classify_remote_failure(operation: &str, stderr: &[u8], secrets: &[String]) -> SnapshotError {
    let mut detail = String::from_utf8_lossy(stderr).into_owned();
    for secret in secrets {
        detail = detail.replace(secret, "<redacted>");
    }
    let normalized = detail.to_ascii_lowercase();
    let message = format!("{operation}; remote diagnostics were redacted");
    if [
        "authentication failed",
        "invalid username or password",
        "could not read username",
        "error: 401",
        "permission denied (publickey",
        "no such identity",
    ]
    .iter()
    .any(|pattern| normalized.contains(pattern))
    {
        SnapshotError::Authentication(message)
    } else if [
        "repository not found",
        "does not appear to be a git repository",
    ]
    .iter()
    .any(|pattern| normalized.contains(pattern))
    {
        SnapshotError::RepositoryNotFound(message)
    } else if [
        "access denied",
        "permission denied",
        "not allowed",
        "not granted",
        "error: 403",
    ]
    .iter()
    .any(|pattern| normalized.contains(pattern))
    {
        SnapshotError::PermissionDenied(message)
    } else if [
        "could not resolve host",
        "failed to connect",
        "connection timed out",
        "network is unreachable",
        "connection refused",
    ]
    .iter()
    .any(|pattern| normalized.contains(pattern))
    {
        SnapshotError::Network(message)
    } else {
        SnapshotError::Git(message)
    }
}

fn git_raw_cancellable<I, S>(
    cwd: &Path,
    arguments: I,
    cancellation: &dyn Cancellation,
) -> Result<Output, SnapshotError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_raw_with_environment_cancellable(cwd, arguments, &[], cancellation)
}

fn git_raw_with_environment_cancellable<I, S>(
    cwd: &Path,
    arguments: I,
    environment: &[(OsString, OsString)],
    cancellation: &dyn Cancellation,
) -> Result<Output, SnapshotError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command
        .args(arguments)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        // Never inherit interactive helpers from the caller. Explicit auth
        // modes install only the narrowly scoped mechanisms they need.
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_tree(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| SnapshotError::Git(format!("could not execute Git: {error}")))?;
    let process_tree = ProcessTree::attach(&mut child).map_err(|error| {
        let _ = child.kill();
        let _ = child.wait();
        SnapshotError::Git(format!("could not bind Git process tree: {error}"))
    })?;
    let stdout = child.stdout.take().expect("Git stdout is piped");
    let stderr = child.stderr.take().expect("Git stderr is piped");
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stream = stdout;
        stream.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stream = stderr;
        stream.read_to_end(&mut bytes).map(|_| bytes)
    });
    let status = loop {
        if cancellation.is_cancelled() {
            process_tree.terminate();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(SnapshotError::Git(
                "Git operation cancelled or timed out".into(),
            ));
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| SnapshotError::Git(format!("wait for Git: {error}")))?
        {
            break status;
        }
        thread::sleep(Duration::from_millis(20));
    };
    // A helper may outlive Git while retaining its output pipe. Terminating the
    // now-childless process group/job bounds reader joins on every exit path.
    process_tree.terminate();
    let stdout = stdout_reader
        .join()
        .map_err(|_| SnapshotError::Git("Git stdout reader panicked".into()))?
        .map_err(|error| SnapshotError::Git(format!("read Git stdout: {error}")))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| SnapshotError::Git("Git stderr reader panicked".into()))?
        .map_err(|error| SnapshotError::Git(format!("read Git stderr: {error}")))?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(unix)]
pub(crate) fn configure_process_tree(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
pub(crate) fn configure_process_tree(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    command.creation_flags(CREATE_SUSPENDED);
}

#[cfg(unix)]
pub(crate) struct ProcessTree {
    pid: u32,
}

#[cfg(unix)]
impl ProcessTree {
    pub(crate) fn attach(child: &mut std::process::Child) -> std::io::Result<Self> {
        Ok(Self { pid: child.id() })
    }

    pub(crate) fn terminate(&self) {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        // SAFETY: configure_process_tree made the child's pid the distinct
        // process-group id, retained even after the group leader exits.
        let _ = unsafe { kill(-(self.pid as i32), SIGKILL) };
    }
}

#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(windows)]
pub(crate) struct ProcessTree {
    job: *mut std::ffi::c_void,
}

#[cfg(windows)]
impl ProcessTree {
    pub(crate) fn attach(child: &mut std::process::Child) -> std::io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        unsafe extern "system" {
            fn CreateJobObjectW(
                attributes: *const std::ffi::c_void,
                name: *const u16,
            ) -> *mut std::ffi::c_void;
            fn AssignProcessToJobObject(
                job: *mut std::ffi::c_void,
                process: *mut std::ffi::c_void,
            ) -> i32;
        }
        // SAFETY: null attributes/name request a private unnamed Job Object.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // The process was created suspended, so it cannot create an untracked
        // descendant before assignment to the Job Object.
        if unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) } == 0 {
            close_windows_handle(job);
            return Err(std::io::Error::last_os_error());
        }
        if let Err(error) = resume_windows_process_threads(child.id()) {
            unsafe extern "system" {
                fn TerminateJobObject(job: *mut std::ffi::c_void, exit_code: u32) -> i32;
            }
            let _ = unsafe { TerminateJobObject(job, 1) };
            close_windows_handle(job);
            return Err(error);
        }
        Ok(Self { job })
    }

    pub(crate) fn terminate(&self) {
        unsafe extern "system" {
            fn TerminateJobObject(job: *mut std::ffi::c_void, exit_code: u32) -> i32;
        }
        // SAFETY: self.job is a live Job Object handle owned by this value.
        let _ = unsafe { TerminateJobObject(self.job, 1) };
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.terminate();
        close_windows_handle(self.job);
    }
}

#[cfg(windows)]
fn resume_windows_process_threads(process_id: u32) -> std::io::Result<()> {
    #[repr(C)]
    struct ThreadEntry32 {
        size: u32,
        usage: u32,
        thread_id: u32,
        owner_process_id: u32,
        base_priority: i32,
        delta_priority: i32,
        flags: u32,
    }
    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut std::ffi::c_void;
        fn Thread32First(snapshot: *mut std::ffi::c_void, entry: *mut ThreadEntry32) -> i32;
        fn Thread32Next(snapshot: *mut std::ffi::c_void, entry: *mut ThreadEntry32) -> i32;
        fn OpenThread(access: u32, inherit: i32, thread_id: u32) -> *mut std::ffi::c_void;
        fn ResumeThread(thread: *mut std::ffi::c_void) -> u32;
    }
    const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
    const THREAD_SUSPEND_RESUME: u32 = 0x0002;
    let invalid_handle = -1_isize as *mut std::ffi::c_void;
    // SAFETY: the returned snapshot is checked and closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == invalid_handle {
        return Err(std::io::Error::last_os_error());
    }
    let mut entry = ThreadEntry32 {
        size: std::mem::size_of::<ThreadEntry32>() as u32,
        usage: 0,
        thread_id: 0,
        owner_process_id: 0,
        base_priority: 0,
        delta_priority: 0,
        flags: 0,
    };
    let mut found = false;
    // SAFETY: entry has the documented size/layout and snapshot is live.
    let mut available = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while available {
        if entry.owner_process_id == process_id {
            // SAFETY: OpenThread returns an independently owned handle.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.thread_id) };
            if thread.is_null() {
                close_windows_handle(snapshot);
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: thread is a valid handle with suspend/resume access.
            let resumed = unsafe { ResumeThread(thread) };
            close_windows_handle(thread);
            if resumed == u32::MAX {
                close_windows_handle(snapshot);
                return Err(std::io::Error::last_os_error());
            }
            found = true;
        }
        // SAFETY: entry and snapshot remain valid for enumeration.
        available = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    close_windows_handle(snapshot);
    if found {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "suspended Git process thread was not found",
        ))
    }
}

#[cfg(windows)]
fn close_windows_handle(handle: *mut std::ffi::c_void) {
    unsafe extern "system" {
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }
    // SAFETY: callers pass an owned, non-null kernel handle exactly once.
    let _ = unsafe { CloseHandle(handle) };
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
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::process::Stdio;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread;
    use std::time::Duration;

    #[test]
    fn materialized_copy_honors_cancellation_during_traversal() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let id = SnapshotId::parse("0".repeat(64)).unwrap();
        let error =
            copy_and_verify_materialized(source.path(), target.path(), &[], &id, &AlwaysCancelled)
                .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }

    #[cfg(unix)]
    #[test]
    fn source_handle_rejects_replaced_parent_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "private").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("changed")).unwrap();
        assert!(open_source_file(&root.path().join("changed/secret")).is_err());
    }

    #[test]
    fn lfs_pointer_is_rejected_without_attributes() {
        let source = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            source.path(),
            "version https://git-lfs.github.com/spec/v1\noid sha256:0000\nsize 9\n",
        )
        .unwrap();
        let files = vec![SourceFile {
            relative: "data.bin".into(),
            source: Some(fs::canonicalize(source.path()).unwrap()),
            virtual_content: None,
            mode: 0o100644,
        }];
        assert!(
            reject_lfs(&files, &NeverCancelled)
                .unwrap_err()
                .to_string()
                .contains("LFS")
        );
    }

    #[test]
    fn isolated_auth_overrides_inherited_git_configuration_parameters() {
        let directory = tempfile::tempdir().unwrap();
        let context = AuthenticationContext::prepare(
            &GitAuthentication::None,
            directory.path(),
            "https://example.test/repo",
        )
        .unwrap();
        let result = Command::new("git")
            .args(["config", "--get-all", "http.extraHeader"])
            .current_dir(directory.path())
            .env(
                "GIT_CONFIG_PARAMETERS",
                "'http.extraHeader=Authorization: leaked'",
            )
            .envs(context.environment.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(1));
        assert!(result.stdout.is_empty());
    }

    #[test]
    fn https_helper_preserves_git_config_but_disables_ssh_authentication() {
        let root = tempfile::tempdir().unwrap();
        let context = AuthenticationContext::prepare(
            &GitAuthentication::HttpsCredentialHelper,
            root.path(),
            "https://example.test/repo",
        )
        .unwrap();
        let config = context
            .environment
            .iter()
            .find(|(key, _)| key == "REPO_SANDBOX_SSH_CONFIG")
            .unwrap();
        assert!(
            fs::read_to_string(&config.1)
                .unwrap()
                .contains("IdentityAgent none")
        );
        assert!(
            !context
                .environment
                .iter()
                .any(|(key, _)| key == "GIT_CONFIG_PARAMETERS")
        );
    }

    struct AlwaysCancelled;

    impl Cancellation for AlwaysCancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
    }

    #[test]
    fn snapshot_copy_observes_cancellation_before_publishing_content() {
        struct CancelAfterTwoChecks(std::sync::atomic::AtomicUsize);
        impl Cancellation for CancelAfterTwoChecks {
            fn is_cancelled(&self) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst) >= 2
            }
        }
        let source = tempfile::NamedTempFile::new().unwrap();
        fs::write(source.path(), vec![b'x'; 128 * 1024]).unwrap();
        let destination = tempfile::tempdir().unwrap();
        let error = copy_and_digest(
            vec![SourceFile {
                relative: "large.bin".into(),
                source: Some(fs::canonicalize(source.path()).unwrap()),
                virtual_content: None,
                mode: 0o100644,
            }],
            destination.path(),
            &CancelAfterTwoChecks(std::sync::atomic::AtomicUsize::new(0)),
        )
        .unwrap_err();
        assert_eq!(
            error,
            SnapshotError::Io("snapshot creation cancelled".into())
        );
        assert_eq!(
            fs::metadata(destination.path().join("large.bin"))
                .unwrap()
                .len(),
            64 * 1024
        );
    }

    struct LocalPublicGitHttp {
        port: u16,
        stop: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<String>>>,
        server: Option<thread::JoinHandle<()>>,
    }

    impl LocalPublicGitHttp {
        fn start(root: &Path) -> Self {
            Self::start_with_mode(root, false)
        }

        fn start_rejecting_credentials(root: &Path) -> Self {
            Self::start_with_mode(root, true)
        }

        fn start_with_mode(root: &Path, reject_credentials: bool) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            let root = root.to_owned();
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let thread_stop = Arc::clone(&stop);
            let thread_requests = Arc::clone(&requests);
            let server = thread::spawn(move || {
                let mut workers = Vec::new();
                while !thread_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nodelay(true).unwrap();
                            let root = root.clone();
                            let requests = Arc::clone(&thread_requests);
                            workers.push(thread::spawn(move || {
                                serve_public_git_backend(
                                    stream,
                                    &root,
                                    &requests,
                                    reject_credentials,
                                )
                            }));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept local public Git HTTP request: {error}"),
                    }
                }
                for worker in workers {
                    worker.join().expect("public Git HTTP request worker");
                }
            });
            Self {
                port,
                stop,
                requests,
                server: Some(server),
            }
        }

        fn repository(&self) -> String {
            format!("http://127.0.0.1:{}/repo.git", self.port)
        }

        fn stop(&mut self) {
            self.shutdown();
            let address = std::net::SocketAddr::from(([127, 0, 0, 1], self.port));
            for _ in 0..20 {
                if TcpStream::connect_timeout(&address, Duration::from_millis(20)).is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!(
                "task-owned public Git HTTP port {} remained open after server join",
                self.port
            );
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }

        fn shutdown(&mut self) {
            if let Some(server) = self.server.take() {
                self.stop.store(true, Ordering::Release);
                server.join().expect("task-owned public Git HTTP thread");
            }
        }
    }

    impl Drop for LocalPublicGitHttp {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    #[test]
    fn local_public_git_http_shutdown_closes_each_listener_after_join() {
        let repository = tempfile::tempdir().unwrap();
        for _ in 0..20 {
            let mut server = LocalPublicGitHttp::start(repository.path());
            server.stop();
        }
    }

    fn serve_public_git_backend(
        mut stream: TcpStream,
        root: &Path,
        captured: &Arc<Mutex<Vec<String>>>,
        reject_credentials: bool,
    ) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        let header_end = loop {
            if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break position + 4;
            }
            if request.len() >= 16 * 1024 {
                return;
            }
            let count = match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(count) => count,
            };
            request.extend_from_slice(&chunk[..count]);
        };
        let headers = String::from_utf8_lossy(&request[..header_end]).into_owned();
        captured.lock().unwrap().push(headers.clone());
        if reject_credentials {
            let response = b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=repo-sandbox-test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response).unwrap();
            stream.flush().unwrap();
            return;
        }
        let mut lines = headers.split("\r\n");
        let first = lines.next().unwrap_or_default();
        let mut fields = first.split_whitespace();
        let method = fields.next().unwrap_or_default();
        let target = fields.next().unwrap_or_default();
        let (path_info, query) = target.split_once('?').unwrap_or((target, ""));
        let relative = path_info.trim_start_matches('/');
        let safe = matches!(method, "GET" | "POST")
            && !relative.is_empty()
            && Path::new(relative)
                .components()
                .all(|component| matches!(component, Component::Normal(_)));
        if !safe {
            write_http_response(&mut stream, "404 Not Found", b"");
            return;
        }
        let mut content_length = 0_usize;
        let mut content_type = String::new();
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            match name.to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                "content-type" => content_type = value.trim().to_owned(),
                _ => {}
            }
        }
        while request.len().saturating_sub(header_end) < content_length {
            let count = stream.read(&mut chunk).unwrap();
            if count == 0 {
                return;
            }
            request.extend_from_slice(&chunk[..count]);
        }
        let body = &request[header_end..header_end + content_length];
        let mut child = Command::new("git")
            .arg("http-backend")
            .env("GIT_PROJECT_ROOT", root)
            .env("GIT_HTTP_EXPORT_ALL", "1")
            .env("REQUEST_METHOD", method)
            .env("PATH_INFO", path_info)
            .env("QUERY_STRING", query)
            .env("CONTENT_TYPE", content_type)
            .env("CONTENT_LENGTH", content_length.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(body).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "git http-backend: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let split = output
            .stdout
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .map(|position| position + 4)
            .expect("CGI header terminator");
        let cgi_headers = String::from_utf8_lossy(&output.stdout[..split]);
        let response_body = &output.stdout[split..];
        let mut status = "200 OK";
        let mut forwarded = Vec::new();
        for line in cgi_headers.split("\r\n").filter(|line| !line.is_empty()) {
            if let Some(value) = line.strip_prefix("Status: ") {
                status = value;
            } else if !line.to_ascii_lowercase().starts_with("content-length:") {
                forwarded.push(line);
            }
        }
        let headers = format!(
            "HTTP/1.1 {status}\r\n{}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            forwarded.join("\r\n"),
            response_body.len()
        );
        stream.write_all(headers.as_bytes()).unwrap();
        stream.write_all(response_body).unwrap();
        stream.flush().unwrap();
        // Git/libcurl can reuse this connection for upload-pack. Keeping it
        // alive avoids a Windows loopback RST between smart-HTTP requests; the
        // client process closes it when the clone finishes, and the read
        // timeout remains a bounded fallback.
        serve_public_git_backend(stream, root, captured, reject_credentials);
    }

    fn write_http_response(stream: &mut TcpStream, status: &str, body: &[u8]) {
        let headers = format!(
            "HTTP/1.0 {status}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
        finish_http_connection(stream);
    }

    fn finish_http_connection(stream: &mut TcpStream) {
        stream.shutdown(Shutdown::Write).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut discarded = [0_u8; 256];
        loop {
            match stream.read(&mut discarded) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }

    fn run_git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn git_stdout(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
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
    fn local_snapshot_excludes_all_repo_sandbox_owned_state() {
        let repo = repository();
        let owned = repo.path().join(".repo-sandbox/tasks");
        fs::create_dir_all(&owned).unwrap();
        fs::write(
            owned.join("task-with-host-path.json"),
            r#"{"path":"C:\\secret"}"#,
        )
        .unwrap();
        let snapshot = create_local(repo.path());
        assert!(!snapshot.path().join(".repo-sandbox").exists());
        assert_eq!(snapshot.snapshot.file_count, 1);
    }

    #[cfg(windows)]
    #[test]
    fn local_snapshot_excludes_case_aliased_repo_sandbox_state_on_windows() {
        let repo = repository();
        let owned = repo.path().join(".Repo-Sandbox/tasks");
        fs::create_dir_all(&owned).unwrap();
        fs::write(owned.join("host-secret"), "must-not-enter-snapshot").unwrap();
        let snapshot = create_local(repo.path());
        assert!(!snapshot.path().join(".Repo-Sandbox").exists());
        assert!(!snapshot.path().join(".repo-sandbox").exists());
        assert_eq!(snapshot.snapshot.file_count, 1);
    }

    #[test]
    fn recursive_https_token_is_rejected_before_secret_or_network_access() {
        let error = GitSnapshotter::default()
            .with_authentication(GitAuthentication::HttpsToken {
                username: "robot".into(),
                token: ExternalSecret::Environment(
                    "REPO_SANDBOX_TEST_INTENTIONALLY_MISSING_TOKEN".into(),
                ),
            })
            .create(
                &SourceSpec::RemoteGit {
                    repository: "https://example.invalid/repository.git".into(),
                    git_ref: "HEAD".into(),
                },
                SnapshotOptions {
                    recurse_submodules: true,
                    cleanup: CleanupPolicy::Delete,
                },
            )
            .unwrap_err();
        assert!(matches!(error, SnapshotError::InvalidInput(_)));
        assert!(error.to_string().contains("separately scoped"));
        assert!(!error.to_string().contains("unavailable"));
    }

    #[test]
    fn snapshot_git_processes_observe_cancellation() {
        let repo = repository();
        let error = GitSnapshotter::default()
            .create_cancellable(
                &SourceSpec::LocalDirectory(repo.path().to_owned()),
                SnapshotOptions::default(),
                &AlwaysCancelled,
            )
            .unwrap_err();
        assert!(error.to_string().contains("cancelled or timed out"));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_descendants_that_hold_git_output_pipes() {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30 & wait"])
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = std::time::Instant::now();
        let mut child = command.spawn().unwrap();
        let process_tree = ProcessTree::attach(&mut child).unwrap();
        let stdout = child.stdout.take().unwrap();
        let reader = thread::spawn(move || {
            let mut output = Vec::new();
            let mut stdout = stdout;
            stdout.read_to_end(&mut output).unwrap();
        });
        process_tree.terminate();
        child.wait().unwrap();
        reader.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(windows)]
    #[test]
    fn job_termination_bounds_descendants_that_hold_output_pipes_without_taskkill() {
        let mut command = Command::new("cmd");
        command
            .args([
                "/d",
                "/s",
                "/c",
                "start \"\" /b cmd /d /s /c \"ping -n 30 127.0.0.1 >NUL\"",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_tree(&mut command);
        let started = std::time::Instant::now();
        let mut child = command.spawn().unwrap();
        let process_tree = ProcessTree::attach(&mut child).unwrap();
        let stdout = child.stdout.take().unwrap();
        let reader = thread::spawn(move || {
            let mut output = Vec::new();
            let mut stdout = stdout;
            stdout.read_to_end(&mut output).unwrap();
        });
        process_tree.terminate();
        child.wait().unwrap();
        reader.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn unstaged_local_executable_mode_changes_identity_but_remote_commit_stays_stable() {
        use std::os::unix::fs::PermissionsExt;

        let repo = repository();
        let branch = git_stdout(repo.path(), &["branch", "--show-current"]);
        let remote_spec = SourceSpec::RemoteGit {
            repository: repo.path().to_string_lossy().into_owned(),
            git_ref: branch,
        };
        let local_before = create_local(repo.path());
        let remote_before = GitSnapshotter::default()
            .create(&remote_spec, SnapshotOptions::default())
            .unwrap();

        let tracked = repo.path().join("tracked.txt");
        fs::set_permissions(&tracked, fs::Permissions::from_mode(0o755)).unwrap();
        let local_after = create_local(repo.path());
        let remote_after = GitSnapshotter::default()
            .create(&remote_spec, SnapshotOptions::default())
            .unwrap();

        assert_ne!(local_before.snapshot.id, local_after.snapshot.id);
        assert_eq!(remote_before.snapshot.id, remote_after.snapshot.id);
    }

    #[test]
    fn index_symlink_is_rejected_even_when_the_worktree_entry_is_a_regular_file() {
        let repo = repository();
        let link = repo.path().join("link");
        fs::write(&link, "tracked.txt").unwrap();
        let object = git_stdout(repo.path(), &["hash-object", "-w", "link"]);
        run_git(
            repo.path(),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "120000",
                &object,
                "link",
            ],
        );

        let error = GitSnapshotter::default()
            .create(
                &SourceSpec::LocalDirectory(repo.path().to_owned()),
                SnapshotOptions::default(),
            )
            .unwrap_err();
        assert!(matches!(error, SnapshotError::Unsupported(_)));
        assert!(error.to_string().contains("symbolic links"));
    }

    #[test]
    fn non_recursive_missing_submodule_gitlink_still_changes_identity() {
        let repo = repository();
        let first_commit = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
        fs::write(repo.path().join("tracked.txt"), "second\n").unwrap();
        run_git(repo.path(), &["commit", "-qam", "second"]);
        let second_commit = git_stdout(repo.path(), &["rev-parse", "HEAD"]);
        assert!(!repo.path().join("deps/child").exists());

        run_git(
            repo.path(),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                &first_commit,
                "deps/child",
            ],
        );
        let first = create_local(repo.path());
        assert!(!first.path().join("deps/child").exists());

        run_git(
            repo.path(),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                &second_commit,
                "deps/child",
            ],
        );
        let second = create_local(repo.path());
        assert_ne!(first.snapshot.id, second.snapshot.id);
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
                    git_ref: format!("refs/heads/{}", branch.trim()),
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
    fn public_http_remote_is_materialized_without_external_network() {
        let source = repository();
        let served = tempfile::tempdir().unwrap();
        let bare = served.path().join("repo.git");
        run_git(
            served.path(),
            &[
                "clone",
                "--bare",
                "-q",
                source.path().to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        run_git(&bare, &["update-server-info"]);
        let mut server = LocalPublicGitHttp::start(served.path());
        let source = SourceSpec::RemoteGit {
            repository: server.repository(),
            git_ref: "HEAD".to_owned(),
        };
        // Git for Windows/libcurl can occasionally reset a just-closed
        // loopback HTTP/1.x connection. Keep the fixture deterministic with a
        // small bounded retry; every attempt still exercises the real HTTP
        // transport and GitSnapshotter clone path.
        let mut result = GitSnapshotter::default().create(&source, SnapshotOptions::default());
        for _ in 1..3 {
            if result.is_ok() {
                break;
            }
            result = GitSnapshotter::default().create(&source, SnapshotOptions::default());
        }
        server.stop();
        assert!(server.requests().iter().all(|request| {
            let lower = request.to_ascii_lowercase();
            !lower.contains("authorization:") && !lower.contains("cookie:")
        }));
        let snapshot = result.unwrap();
        assert_eq!(
            fs::read_to_string(snapshot.path().join("tracked.txt"))
                .unwrap()
                .trim_end(),
            "tracked"
        );
        assert!(matches!(
            snapshot.snapshot.origin,
            SnapshotOrigin::RemoteGit { .. }
        ));
    }

    #[test]
    fn unauthenticated_remote_child_process() {
        let Some(repository) = env::var_os("REPO_SANDBOX_UNAUTH_CHILD_URL") else {
            return;
        };
        let result = GitSnapshotter::default().create(
            &SourceSpec::RemoteGit {
                repository: repository.to_string_lossy().into_owned(),
                git_ref: "HEAD".into(),
            },
            SnapshotOptions::default(),
        );
        assert!(
            result.is_err(),
            "credential trap must reject anonymous access"
        );
    }

    #[test]
    fn unauthenticated_remote_ignores_malicious_global_config_and_netrc() {
        let source = repository();
        let served = tempfile::tempdir().unwrap();
        let bare = served.path().join("repo.git");
        run_git(
            served.path(),
            &[
                "clone",
                "--bare",
                "-q",
                source.path().to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        run_git(&bare, &["update-server-info"]);
        let mut server = LocalPublicGitHttp::start_rejecting_credentials(served.path());
        let malicious = tempfile::tempdir().unwrap();
        let sentinel = malicious.path().join("helper-invoked");
        let sentinel_text = sentinel.to_string_lossy().replace('\\', "/");
        let config = malicious.path().join("gitconfig");
        fs::write(
            &config,
            format!(
                "[http]\n\textraHeader = Authorization: Basic should-not-leak\n[credential]\n\thelper = !echo invoked > \"{sentinel_text}\"\n"
            ),
        )
        .unwrap();
        fs::write(
            malicious.path().join(".netrc"),
            "machine 127.0.0.1 login malicious password should-not-leak\n",
        )
        .unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "snapshot::tests::unauthenticated_remote_child_process",
                "--nocapture",
            ])
            .env("REPO_SANDBOX_UNAUTH_CHILD_URL", server.repository())
            .env("HOME", malicious.path())
            .env("USERPROFILE", malicious.path())
            .env("GIT_CONFIG_GLOBAL", &config)
            .output()
            .unwrap();
        server.stop();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!sentinel.exists());
        assert!(server.requests().iter().all(|request| {
            let lower = request.to_ascii_lowercase();
            !lower.contains("authorization:")
                && !lower.contains("cookie:")
                && !lower.contains("should-not-leak")
        }));
    }

    #[test]
    #[ignore = "opt-in: requires REPO_SANDBOX_E2E_HTTPS_URL/REF/USER/TOKEN for a disposable private repository"]
    fn private_https_remote_opt_in() {
        let repository = env::var("REPO_SANDBOX_E2E_HTTPS_URL").unwrap();
        let git_ref = env::var("REPO_SANDBOX_E2E_HTTPS_REF").unwrap();
        let username = env::var("REPO_SANDBOX_E2E_HTTPS_USER").unwrap();
        let secret = env::var("REPO_SANDBOX_E2E_HTTPS_TOKEN").unwrap();
        let snapshot = GitSnapshotter::default()
            .with_authentication(GitAuthentication::HttpsToken {
                username,
                token: ExternalSecret::Environment("REPO_SANDBOX_E2E_HTTPS_TOKEN".to_owned()),
            })
            .create(
                &SourceSpec::RemoteGit {
                    repository,
                    git_ref,
                },
                SnapshotOptions::default(),
            )
            .unwrap_or_else(|error| {
                assert!(!error.to_string().contains(&secret));
                panic!("private HTTPS snapshot failed: {error}")
            });
        assert!(snapshot.snapshot.file_count > 0);
        assert!(!format!("{:?}", snapshot.snapshot).contains(&secret));
    }

    #[test]
    #[ignore = "opt-in: requires a disposable private HTTPS repository and an explicitly invalid token"]
    fn private_https_invalid_authentication_opt_in() {
        let repository = env::var("REPO_SANDBOX_E2E_HTTPS_URL").unwrap();
        let git_ref = env::var("REPO_SANDBOX_E2E_HTTPS_REF").unwrap();
        let username = env::var("REPO_SANDBOX_E2E_HTTPS_USER").unwrap();
        let secret = env::var("REPO_SANDBOX_E2E_HTTPS_INVALID_TOKEN").unwrap();
        let error = GitSnapshotter::default()
            .with_authentication(GitAuthentication::HttpsToken {
                username,
                token: ExternalSecret::Environment(
                    "REPO_SANDBOX_E2E_HTTPS_INVALID_TOKEN".to_owned(),
                ),
            })
            .create(
                &SourceSpec::RemoteGit {
                    repository,
                    git_ref,
                },
                SnapshotOptions::default(),
            )
            .unwrap_err();
        assert!(matches!(error, SnapshotError::Authentication(_)));
        assert!(!error.to_string().contains(&secret));
        println!("stage=snapshot authentication_failure=redacted");
    }

    #[test]
    #[ignore = "opt-in: requires REPO_SANDBOX_E2E_SSH_URL/REF/KEY/KNOWN_HOSTS for a disposable private repository"]
    fn private_ssh_remote_opt_in() {
        let repository = env::var("REPO_SANDBOX_E2E_SSH_URL").unwrap();
        let git_ref = env::var("REPO_SANDBOX_E2E_SSH_REF").unwrap();
        let private_key = PathBuf::from(env::var("REPO_SANDBOX_E2E_SSH_KEY").unwrap());
        let known_hosts = PathBuf::from(env::var("REPO_SANDBOX_E2E_SSH_KNOWN_HOSTS").unwrap());
        let key_material = fs::read_to_string(&private_key).unwrap();
        let snapshot = GitSnapshotter::default()
            .with_authentication(GitAuthentication::SshKey {
                private_key,
                known_hosts: Some(known_hosts),
            })
            .create(
                &SourceSpec::RemoteGit {
                    repository,
                    git_ref,
                },
                SnapshotOptions::default(),
            )
            .unwrap_or_else(|error| {
                assert!(!error.to_string().contains(&key_material));
                panic!("private SSH snapshot failed: {error}")
            });
        assert!(snapshot.snapshot.file_count > 0);
        assert!(!format!("{:?}", snapshot.snapshot).contains(&key_material));
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

        let mut failed = snapshotter
            .create(
                &SourceSpec::LocalDirectory(repo.path().to_owned()),
                SnapshotOptions::default(),
            )
            .unwrap();
        failed.retain_on_failure();
        let failed_path = failed.path().to_owned();
        assert!(!failed.is_automatically_cleaned());
        drop(failed);
        assert!(failed_path.exists());
        fs::remove_dir_all(failed_path.parent().unwrap()).unwrap();

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

    #[test]
    fn https_token_uses_an_ephemeral_helper_without_persisting_the_secret() {
        let root = tempfile::tempdir().unwrap();
        let secret_file = root.path().join("token");
        let token = "super-secret-token& echo owned>unexpected-side-effect &|<>%^$()`";
        fs::write(&secret_file, format!("{token}\n")).unwrap();
        let context = AuthenticationContext::prepare(
            &GitAuthentication::HttpsToken {
                username: "git-user".into(),
                token: ExternalSecret::File(secret_file),
            },
            root.path(),
            "https://example.test/org/repo.git",
        )
        .unwrap();

        let askpass = context
            .environment
            .iter()
            .find(|(key, _)| key == "GIT_ASKPASS")
            .map(|(_, value)| PathBuf::from(value))
            .unwrap();
        assert!(!fs::read_to_string(&askpass).unwrap().contains(token));
        assert!(
            context
                .environment
                .iter()
                .any(|(key, value)| { key == "GIT_CONFIG_VALUE_0" && value.is_empty() })
        );

        #[cfg(unix)]
        let output = Command::new(&askpass)
            .arg("Password for remote")
            .current_dir(root.path())
            .envs(context.environment.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap();
        #[cfg(windows)]
        let output = Command::new("cmd")
            .arg("/c")
            .arg(&askpass)
            .arg("Password for remote")
            .current_dir(root.path())
            .envs(context.environment.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), token);
        assert!(!root.path().join("unexpected-side-effect").exists());
    }

    #[test]
    fn unauthenticated_remote_disables_implicit_helpers_and_ssh_identities() {
        let root = tempfile::tempdir().unwrap();
        let context = AuthenticationContext::prepare(
            &GitAuthentication::None,
            root.path(),
            "https://example.test/org/repo.git",
        )
        .unwrap();
        let values = context
            .environment
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(values.get("GIT_CONFIG_KEY_0").unwrap(), "credential.helper");
        assert_eq!(values.get("GIT_CONFIG_VALUE_0").unwrap(), "");
        let ssh_config =
            fs::read_to_string(values.get("REPO_SANDBOX_SSH_CONFIG").unwrap()).unwrap();
        assert!(ssh_config.contains("IdentitiesOnly yes"));
        assert!(ssh_config.contains("IdentityAgent none"));
        assert!(ssh_config.contains("IdentityFile none"));
        assert!(!values.contains_key("GIT_ASKPASS"));
    }

    #[test]
    fn ssh_configuration_is_strict_and_references_the_key() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("id # test");
        let hosts = root.path().join("known # hosts");
        fs::write(&key, "fake-key-not-a-secret").unwrap();
        fs::write(&hosts, "example.test ssh-ed25519 fake").unwrap();
        let context = AuthenticationContext::prepare(
            &GitAuthentication::SshKey {
                private_key: key,
                known_hosts: Some(hosts),
            },
            root.path(),
            "git@example.test:org/repo.git",
        )
        .unwrap();
        let command = context
            .environment
            .iter()
            .find(|(key, _)| key == "GIT_SSH_COMMAND")
            .unwrap()
            .1
            .to_string_lossy();
        assert!(!command.contains("fake-key-not-a-secret"));
        assert!(!command.contains(root.path().to_string_lossy().as_ref()));
        let config = fs::read_to_string(root.path().join("ssh-config")).unwrap();
        assert!(config.contains("StrictHostKeyChecking yes"));
        assert!(config.contains("IdentitiesOnly yes"));
        assert!(config.contains("UserKnownHostsFile"));
        assert!(config.contains("IdentityFile \""));
        assert!(config.contains("id # test\""));
        assert!(config.contains("known # hosts\""));

        let output = Command::new("ssh")
            .args(["-G", "-F"])
            .arg(root.path().join("ssh-config"))
            .arg("example.test")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("id # test"));
    }

    #[test]
    fn shell_metacharacters_in_temporary_parent_never_enter_ssh_command_syntax() {
        let base = tempfile::tempdir().unwrap();
        let parent = base
            .path()
            .join("ssh $(touch injected) `touch injected-too` # & space");
        fs::create_dir(&parent).unwrap();
        let context = AuthenticationContext::prepare(
            &GitAuthentication::SshAgent { known_hosts: None },
            &parent,
            "git@example.test:org/repo.git",
        )
        .unwrap();
        let command = context
            .environment
            .iter()
            .find(|(key, _)| key == "GIT_SSH_COMMAND")
            .unwrap()
            .1
            .to_string_lossy();
        assert_eq!(command, "ssh -F \"$REPO_SANDBOX_SSH_CONFIG\"");
        assert!(!command.contains(parent.to_string_lossy().as_ref()));
        let config = parent.join("ssh-config");
        let mut file = fs::OpenOptions::new().append(true).open(config).unwrap();
        file.write_all(b"Host example.test\n  HostName 127.0.0.1\n  Port 1\n  ConnectTimeout 1\n")
            .unwrap();

        // Exercise Git's real SSH command boundary. The local refusal is
        // expected; the assertion is that parsing never executes path text.
        let output = Command::new("git")
            .args(["ls-remote", "git@example.test:org/repo.git"])
            .current_dir(base.path())
            .envs(context.environment.iter().map(|(key, value)| (key, value)))
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!base.path().join("injected").exists());
        assert!(!base.path().join("injected-too").exists());
    }

    #[test]
    fn remote_failures_have_stable_safe_categories() {
        let cases = [
            ("Authentication failed for token-123", "authentication"),
            ("repository not found", "not-found"),
            ("remote: access denied", "permission"),
            ("fatal: could not resolve host", "network"),
        ];
        for (stderr, expected) in cases {
            let error = classify_remote_failure("clone", stderr.as_bytes(), &["token-123".into()]);
            assert!(!error.to_string().contains("token-123"));
            assert!(matches!(
                (expected, error),
                ("authentication", SnapshotError::Authentication(_))
                    | ("not-found", SnapshotError::RepositoryNotFound(_))
                    | ("permission", SnapshotError::PermissionDenied(_))
                    | ("network", SnapshotError::Network(_))
            ));
        }
    }

    #[test]
    fn inline_https_credentials_are_rejected() {
        let error =
            validate_remote_input("https://user:token@example.test/repo", "main").unwrap_err();
        assert!(matches!(error, SnapshotError::InvalidInput(_)));
        assert!(!error.to_string().contains("token"));
    }
}
