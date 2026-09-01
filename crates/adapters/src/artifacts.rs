//! Safe, declaration-based artifact export and task-owned source cleanup.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs::{self, File, Metadata};
use std::io;
use std::path::{Component, Path, PathBuf};

pub const OWNER_MARKER: &str = ".repo-sandbox-owner";

#[derive(Debug)]
pub struct ArtifactError(String);

impl Display for ArtifactError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ArtifactError {}

impl From<io::Error> for ArtifactError {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}

/// Export only explicitly requested directories that exactly match a declaration.
/// Links and Windows reparse points are rejected at every level. Files are copied
/// from an already-open handle after a final metadata check.
pub fn export_declared_artifacts(
    workspace: &Path,
    declared: &[PathBuf],
    requested: &[PathBuf],
    export_root: &Path,
) -> Result<Vec<PathBuf>, ArtifactError> {
    let workspace = canonical_directory(workspace, "workspace")?;
    fs::create_dir_all(export_root)?;
    refuse_link(export_root)?;
    let export_root = canonical_directory(export_root, "artifact export root")?;
    let mut exported = Vec::new();
    for request in requested {
        validate_artifact_path(request)?;
        if !declared.iter().any(|item| item == request) {
            return Err(ArtifactError(format!(
                "artifact `{}` was not declared",
                request.display()
            )));
        }
        let source = workspace.join(request);
        refuse_link_chain(&workspace, request)?;
        let source = canonical_directory(&source, "declared artifact")?;
        if !source.starts_with(&workspace) {
            return Err(ArtifactError(
                "declared artifact escapes the workspace".into(),
            ));
        }
        let destination = export_root.join(request);
        if destination.exists() {
            return Err(ArtifactError(format!(
                "artifact destination `{}` already exists",
                destination.display()
            )));
        }
        create_safe_parents(
            &export_root,
            request.parent().unwrap_or_else(|| Path::new("")),
        )?;
        copy_tree(&source, &destination, &workspace)?;
        exported.push(destination);
    }
    Ok(exported)
}

fn create_safe_parents(root: &Path, relative: &Path) -> Result<(), ArtifactError> {
    let mut current = root.to_path_buf();
    for part in relative.components() {
        let Component::Normal(part) = part else {
            return Err(ArtifactError(
                "invalid artifact destination component".into(),
            ));
        };
        current.push(part);
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => refuse_link(&current)?,
            Err(error) => return Err(error.into()),
        }
        if !current.canonicalize()?.starts_with(root) {
            return Err(ArtifactError(
                "artifact destination escapes export root".into(),
            ));
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path, boundary: &Path) -> Result<(), ArtifactError> {
    refuse_link(source)?;
    let canonical = source.canonicalize()?;
    if !canonical.starts_with(boundary) {
        return Err(ArtifactError(format!(
            "artifact entry `{}` escapes the workspace",
            source.display()
        )));
    }
    fs::create_dir(destination)?;
    for entry in fs::read_dir(&canonical)? {
        let entry = entry?;
        let path = entry.path();
        refuse_link(&path)?;
        let target = destination.join(entry.file_name());
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            copy_tree(&path, &target, boundary)?;
        } else if metadata.is_file() {
            // Open only after the no-link check, re-check the handle, then copy
            // from it. This narrows replacement races without following a later path.
            let mut input = File::open(&path)?;
            refuse_reparse_metadata(&input.metadata()?, &path)?;
            let mut output = File::create_new(&target)?;
            io::copy(&mut input, &mut output)?;
        } else {
            return Err(ArtifactError(format!(
                "unsupported artifact entry `{}`",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Delete a materialized source only when it is a direct child of the supplied
/// task-temp parent and carries the exact task ownership marker. The directory
/// is first atomically renamed within that parent to bind the checked object.
pub fn cleanup_owned_temp_source(
    temp_parent: &Path,
    source: &Path,
    task_id: &str,
) -> Result<(), ArtifactError> {
    validate_task_id(task_id)?;
    let parent = canonical_directory(temp_parent, "temporary source parent")?;
    refuse_link(source)?;
    let canonical = canonical_directory(source, "temporary source")?;
    if canonical.parent() != Some(parent.as_path()) {
        return Err(ArtifactError(
            "temporary source is not a direct child of its ownership boundary".into(),
        ));
    }
    refuse_links_recursively(&canonical, &canonical)?;
    let owner = fs::read_to_string(canonical.join(OWNER_MARKER))?;
    if owner != task_id {
        return Err(ArtifactError(
            "temporary source ownership marker mismatch".into(),
        ));
    }
    let tombstone = parent.join(format!(".repo-sandbox-cleanup-{task_id}"));
    if tombstone.exists() {
        return Err(ArtifactError(
            "task cleanup tombstone already exists".into(),
        ));
    }
    fs::rename(&canonical, &tombstone)?;
    fs::remove_dir_all(tombstone)?;
    Ok(())
}

fn refuse_links_recursively(path: &Path, boundary: &Path) -> Result<(), ArtifactError> {
    refuse_link(path)?;
    if !path.canonicalize()?.starts_with(boundary) {
        return Err(ArtifactError(
            "temporary source entry escapes its boundary".into(),
        ));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        refuse_link(&child)?;
        if entry.metadata()?.is_dir() {
            refuse_links_recursively(&child, boundary)?;
        }
    }
    Ok(())
}

fn validate_task_id(task_id: &str) -> Result<(), ArtifactError> {
    if !task_id.is_empty()
        && task_id.len() <= 48
        && task_id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        Ok(())
    } else {
        Err(ArtifactError("invalid task ownership id".into()))
    }
}

pub fn validate_artifact_path(path: &Path) -> Result<(), ArtifactError> {
    let text = path.to_string_lossy();
    let portable_segments: Vec<_> = text.split(['/', '\\']).collect();
    let portable = !text.is_empty()
        && !text.starts_with(['/', '\\'])
        && !text.contains(':')
        && portable_segments
            .iter()
            .all(|part| !part.is_empty() && *part != "." && *part != "..");
    let native = !path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
    if portable && native {
        Ok(())
    } else {
        Err(ArtifactError(format!(
            "artifact path `{}` is not a portable safe relative path",
            path.display()
        )))
    }
}

fn refuse_link_chain(root: &Path, relative: &Path) -> Result<(), ArtifactError> {
    let mut current = root.to_path_buf();
    for part in relative.components() {
        let Component::Normal(part) = part else {
            return Err(ArtifactError("invalid artifact path component".into()));
        };
        current.push(part);
        refuse_link(&current)?;
    }
    Ok(())
}

fn canonical_directory(path: &Path, description: &str) -> Result<PathBuf, ArtifactError> {
    refuse_link(path)?;
    let canonical = path.canonicalize()?;
    if !canonical.is_dir() {
        return Err(ArtifactError(format!("{description} must be a directory")));
    }
    Ok(canonical)
}

fn refuse_link(path: &Path) -> Result<(), ArtifactError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(ArtifactError(format!(
            "link or reparse point `{}` is not exportable",
            path.display()
        )));
    }
    refuse_reparse_metadata(&metadata, path)
}

#[cfg(windows)]
fn refuse_reparse_metadata(metadata: &Metadata, path: &Path) -> Result<(), ArtifactError> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Err(ArtifactError(format!(
            "link or reparse point `{}` is not exportable",
            path.display()
        )))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn refuse_reparse_metadata(_metadata: &Metadata, _path: &Path) -> Result<(), ArtifactError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_only_exactly_declared_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let export = temp.path().join("export");
        fs::create_dir_all(workspace.join("target/release")).unwrap();
        fs::write(workspace.join("target/release/app"), b"binary").unwrap();
        fs::create_dir_all(&export).unwrap();
        let declared = vec![PathBuf::from("target/release")];
        let paths = export_declared_artifacts(&workspace, &declared, &declared, &export).unwrap();
        assert_eq!(fs::read(paths[0].join("app")).unwrap(), b"binary");
        assert!(
            export_declared_artifacts(
                &workspace,
                &declared,
                &[PathBuf::from("target")],
                &temp.path().join("other"),
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_portable_traversal_and_windows_absolute_forms() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("workspace")).unwrap();
        for unsafe_path in ["../secret", r"..\secret", r"C:\secret", r"\\server\share"] {
            let request = PathBuf::from(unsafe_path);
            assert!(
                export_declared_artifacts(
                    &temp.path().join("workspace"),
                    std::slice::from_ref(&request),
                    std::slice::from_ref(&request),
                    &temp.path().join("export"),
                )
                .is_err(),
                "accepted {unsafe_path}"
            );
        }
    }

    #[test]
    fn cleanup_requires_direct_child_and_exact_owner() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join(OWNER_MARKER), "other-task").unwrap();
        assert!(cleanup_owned_temp_source(temp.path(), &source, "task-7").is_err());
        assert!(source.exists());
        fs::write(source.join(OWNER_MARKER), "task-7").unwrap();
        cleanup_owned_temp_source(temp.path(), &source, "task-7").unwrap();
        assert!(!source.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, workspace.join("reports")).unwrap();
        let declaration = vec![PathBuf::from("reports")];
        assert!(
            export_declared_artifacts(
                &workspace,
                &declaration,
                &declaration,
                &temp.path().join("export"),
            )
            .is_err()
        );
    }
}
