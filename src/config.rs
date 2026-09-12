use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const LIMA_VERSION: &str = "2.2.0";
pub const NAME: &str = "luhmen";
const OWNER: &str = "luhmen-state-v1\n";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub path: PathBuf,
    pub writable: bool,
}

impl Mount {
    pub fn parse(value: &str) -> Result<Self> {
        let (path, writable) = if let Some(path) = value.strip_suffix(":rw") {
            (path, true)
        } else {
            (value.strip_suffix(":ro").unwrap_or(value), false)
        };
        let path = fs::canonicalize(path).with_context(|| format!("mount directory {path}"))?;
        ensure!(path.is_dir(), "mount must be a directory");
        let text = path.to_str().context("mount path must be UTF-8")?;
        ensure!(
            !text.contains(['\n', '\r', '\0']) && !text.contains("{{"),
            "unsupported mount path"
        );
        ensure!(
            path != Path::new("/"),
            "sharing the filesystem root is not supported"
        );
        Ok(Self { path, writable })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub cpus: u16,
    pub memory_gib: u16,
    pub disk_gib: u16,
    pub mounts: Vec<Mount>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub nested_virtualization: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Config {
    pub fn validate_schema(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported configuration schema");
        ensure!(
            (1..=64).contains(&self.cpus),
            "cpus must be between 1 and 64"
        );
        ensure!(
            (2..=256).contains(&self.memory_gib),
            "memory must be between 2 and 256 GiB"
        );
        ensure!(
            (10..=2048).contains(&self.disk_gib),
            "disk must be between 10 and 2048 GiB"
        );
        ensure!(self.mounts.len() <= 32, "at most 32 mounts are supported");
        for mount in &self.mounts {
            ensure!(mount.path.is_absolute(), "mount path must be absolute");
        }
        Ok(())
    }

    pub fn validate(&self, state: &Path) -> Result<()> {
        self.validate_schema()?;
        let state = canonical_destination(state)?;
        for (index, mount) in self.mounts.iter().enumerate() {
            ensure!(
                mount.path.is_dir(),
                "mount directory is missing or not absolute: {}",
                mount.path.display()
            );
            ensure!(
                fs::canonicalize(&mount.path)? == mount.path,
                "mount path changed or contains a symlink; restore the original directory"
            );
            ensure!(
                !state.starts_with(&mount.path) && !mount.path.starts_with(&state),
                "mounts must not overlap luhmen's state directory"
            );
            let text = mount.path.to_str().context("mount path must be UTF-8")?;
            ensure!(
                !text.contains(['\n', '\r', '\0']) && !text.contains("{{"),
                "unsupported mount path"
            );
            for other in &self.mounts[..index] {
                ensure!(
                    !mount.path.starts_with(&other.path) && !other.path.starts_with(&mount.path),
                    "mount directories must not overlap"
                );
            }
        }
        Ok(())
    }
}

pub fn state_path() -> Result<PathBuf> {
    let path = match std::env::var_os("LUHMEN_HOME") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
            .join(".local/share/luhmen"),
    };
    ensure!(path.is_absolute(), "LUHMEN_HOME must be absolute");
    ensure!(
        !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "state path cannot contain '..'"
    );
    ensure!(path.to_str().is_some(), "state path must be UTF-8");
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        ensure!(
            !metadata.file_type().is_symlink(),
            "state directory cannot be a symlink"
        );
    }
    let path = canonical_destination(&path)?;
    // Darwin's sockaddr_un has 104 bytes, including its NUL terminator.
    ensure!(
        path.join("lima/luhmen/sock/docker.sock").as_os_str().len() < 104,
        "state path is too long for a Unix socket; set a shorter LUHMEN_HOME"
    );
    Ok(path)
}

fn canonical_destination(path: &Path) -> Result<PathBuf> {
    let ancestor = path
        .ancestors()
        .find(|parent| parent.exists())
        .context("path has no existing ancestor")?;
    Ok(fs::canonicalize(ancestor)?.join(path.strip_prefix(ancestor)?))
}

pub fn ensure_owned(path: &Path, create: bool) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "state path must be a real directory"
        );
        if path.join("owner").exists() {
            ensure!(
                fs::read_to_string(path.join("owner"))? == OWNER,
                "state ownership marker does not match"
            );
            ensure!(
                metadata.permissions().mode() & 0o077 == 0,
                "state directory must have mode 0700"
            );
            return Ok(());
        }
        ensure!(
            create && fs::read_dir(path)?.next().is_none(),
            "refusing an unowned state directory"
        );
    } else if !create {
        bail!("luhmen has not been created; run `luhmen create`");
    } else {
        fs::create_dir_all(path)?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    atomic_write(&path.join("owner"), OWNER.as_bytes())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(path.parent().context("file has no parent")?)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_refuses_existing_data() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("data"), b"keep").unwrap();
        assert!(ensure_owned(temp.path(), true).is_err());
        assert_eq!(fs::read(temp.path().join("data")).unwrap(), b"keep");
    }
    #[test]
    fn own_directory_and_atomic_config_survive_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        ensure_owned(&path, true).unwrap();
        ensure_owned(&path, false).unwrap();
        atomic_write(&path.join("config.json"), b"one").unwrap();
        atomic_write(&path.join("config.json"), b"two").unwrap();
        assert_eq!(fs::read(path.join("config.json")).unwrap(), b"two");
    }
    #[test]
    fn state_and_overlapping_mounts_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(temp.path()).unwrap();
        let config = Config {
            schema_version: 1,
            cpus: 4,
            memory_gib: 4,
            disk_gib: 30,
            mounts: vec![Mount {
                path: path.clone(),
                writable: true,
            }],
            nested_virtualization: false,
        };
        assert!(config.validate(&path.join("state")).is_err());
    }
}
