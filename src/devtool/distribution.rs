use anyhow::{Context, Result, ensure};
use flate2::{Compression, GzBuilder};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use tar::{Archive, Builder, Header};
use tempfile::TempDir;

const RELEASE_TARGET: &str = "aarch64-apple-darwin";

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn command_output(command: &mut Command) -> Result<Output> {
    let display = format!("{command:?}");
    let output = command.output().with_context(|| format!("run {display}"))?;
    ensure!(
        output.status.success(),
        "{display} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output)
}

fn text(command: &mut Command) -> Result<String> {
    Ok(String::from_utf8(command_output(command)?.stdout)?
        .trim()
        .to_owned())
}

pub fn build_release(
    repo: &Path,
    target_dir: &Path,
    selected_target: Option<&str>,
) -> Result<(PathBuf, Vec<(String, String)>)> {
    let repo = repo.canonicalize()?;
    let target_dir = if target_dir.is_absolute() {
        target_dir.to_path_buf()
    } else {
        repo.join(target_dir)
    };
    let home =
        PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).canonicalize()?;
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cargo"));
    let sysroot = PathBuf::from(text(Command::new("rustc").args(["--print", "sysroot"]))?);
    let target = selected_target
        .map(str::to_owned)
        .unwrap_or(text(Command::new("rustc").args(["--print", "host-tuple"]))?);
    let mappings = [
        (&home, "/usr/local/home"),
        (&target_dir, "/tmp/luhmen-build"),
        (&cargo_home, "/usr/local/cargo"),
        (&sysroot, "/usr/local/rust"),
        (&repo, "/usr/src/luhmen"),
    ];
    let rustflags = mappings
        .iter()
        .map(|(source, destination)| {
            format!("--remap-path-prefix={}={destination}", source.display())
        })
        .collect::<Vec<_>>()
        .join("\x1f");
    let environment = vec![
        ("MACOSX_DEPLOYMENT_TARGET".to_owned(), "14.0".to_owned()),
        (
            "CARGO_TARGET_DIR".to_owned(),
            target_dir.display().to_string(),
        ),
        ("CARGO_INCREMENTAL".to_owned(), "0".to_owned()),
        (
            "CARGO_PROFILE_RELEASE_STRIP".to_owned(),
            "symbols".to_owned(),
        ),
        ("CARGO_PROFILE_RELEASE_DEBUG".to_owned(), "0".to_owned()),
        ("CARGO_ENCODED_RUSTFLAGS".to_owned(), rustflags),
    ];
    let mut command = Command::new("cargo");
    command
        .args([
            "build",
            "--release",
            "--locked",
            "--package",
            "luhmen",
            "--target",
            &target,
        ])
        .current_dir(&repo)
        .env_remove("RUSTFLAGS");
    for (key, value) in &environment {
        command.env(key, value);
    }
    command_output(&mut command)?;
    let executable = target_dir.join(&target).join("release/luhmen");
    let contents = fs::read(&executable)?;
    for (source, _) in mappings {
        ensure!(
            !contents
                .windows(source.as_os_str().as_encoded_bytes().len())
                .any(|part| { part == source.as_os_str().as_encoded_bytes() }),
            "binary contains an unremapped local source path: {}",
            source.display()
        );
    }
    Ok((executable, environment))
}

pub fn check_source() -> Result<()> {
    let repo = repo();
    let output = command_output(
        Command::new("git")
            .args([
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ])
            .current_dir(&repo),
    )?;
    let mut failures = Vec::new();
    for name in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = std::str::from_utf8(name)?;
        let path = repo.join(name);
        if !path.is_file() || path.is_symlink() {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for (index, line) in content.lines().enumerate() {
            if machine_home_path(line) {
                failures.push(format!("{name}:{}: machine-specific home path", index + 1));
            }
        }
    }
    ensure!(failures.is_empty(), "{}", failures.join("\n"));
    println!("Source path check passed.");
    Ok(())
}

fn machine_home_path(line: &str) -> bool {
    [format!("/{}/", "Users"), format!("/{}/", "home")]
        .iter()
        .any(|prefix| {
            line.match_indices(prefix).any(|(index, prefix)| {
                let rest = &line[index + prefix.len()..];
                let length = rest
                    .bytes()
                    .take_while(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(byte))
                    .count();
                length > 0 && rest.as_bytes().get(length).is_none_or(|byte| *byte == b'/')
            })
        })
}

#[derive(Deserialize)]
struct Metadata {
    workspace_members: BTreeSet<String>,
    packages: Vec<Package>,
}

#[derive(Clone, Deserialize)]
struct Package {
    id: String,
    name: String,
    version: String,
    manifest_path: PathBuf,
    license: Option<String>,
    repository: Option<String>,
    license_file: Option<PathBuf>,
}

fn license_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_lowercase();
    [
        "license",
        "licence",
        "copying",
        "copyright",
        "notice",
        "unlicense",
    ]
    .iter()
    .any(|prefix| {
        name == *prefix
            || name.strip_prefix(prefix).is_some_and(|rest| {
                rest.starts_with('.') || rest.starts_with('-') || rest.starts_with('_')
            })
    })
}

fn walk_files(root: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            walk_files(&entry.path(), output)?;
        } else if kind.is_file() && entry.metadata()?.len() > 0 {
            output.push(entry.path());
        }
    }
    Ok(())
}

fn license_files(package: &Package, root: &Path) -> Result<Vec<PathBuf>> {
    let root = root.canonicalize()?;
    let mut files = BTreeSet::new();
    if let Some(declared) = &package.license_file {
        let candidate = root.join(declared).canonicalize()?;
        ensure!(
            candidate.starts_with(&root),
            "license path escapes package: {}",
            package.name
        );
        if candidate.is_file() && candidate.metadata()?.len() > 0 {
            files.insert(candidate);
        }
    }
    let mut candidates = Vec::new();
    walk_files(&root, &mut candidates)?;
    files.extend(
        candidates
            .into_iter()
            .filter(|path| license_name(path.file_name().unwrap_or_default())),
    );
    Ok(files.into_iter().collect())
}

pub fn collect_licenses(output: &Path, target: Option<&str>) -> Result<()> {
    let repo = repo();
    let target = target
        .map(str::to_owned)
        .unwrap_or(text(Command::new("rustc").args(["--print", "host-tuple"]))?);
    let metadata: Metadata = serde_json::from_slice(
        &command_output(
            Command::new("cargo")
                .args([
                    "metadata",
                    "--locked",
                    "--format-version",
                    "1",
                    "--filter-platform",
                    &target,
                ])
                .current_dir(&repo),
        )?
        .stdout,
    )?;
    let mut packages: Vec<_> = metadata
        .packages
        .into_iter()
        .filter(|package| !metadata.workspace_members.contains(&package.id))
        .collect();
    packages.sort_by(|left, right| (&left.name, &left.version).cmp(&(&right.name, &right.version)));
    ensure!(
        !output.exists(),
        "output must not already exist: {}",
        output.display()
    );
    let supplements = repo.join("licenses");
    let mut selections = Vec::new();
    let mut missing = Vec::new();
    for package in packages {
        let package_root = package
            .manifest_path
            .parent()
            .context("package manifest has no parent")?;
        let mut root = package_root.to_path_buf();
        let mut files = license_files(&package, &root)?;
        let supplement = supplements.join(format!("{}-{}", package.name, package.version));
        if files.is_empty() && supplement.is_dir() {
            root = supplement;
            files = license_files(&package, &root)?;
            if root.join("SOURCE").is_file() {
                files.push(root.join("SOURCE"));
            }
        }
        if files.is_empty() {
            missing.push(format!("{} {}", package.name, package.version));
        }
        selections.push((package, root, files));
    }
    ensure!(
        missing.is_empty(),
        "no license text found for: {}",
        missing.join(", ")
    );
    fs::create_dir_all(output)?;
    let mut index = Vec::new();
    for (package, root, files) in &selections {
        let directory = format!("{}-{}", package.name, package.version);
        let mut copied = Vec::new();
        for source in files {
            let relative = source.strip_prefix(root)?;
            let destination = output.join(&directory).join(relative);
            fs::create_dir_all(destination.parent().unwrap())?;
            fs::copy(source, &destination)?;
            copied.push(format!("{directory}/{}", relative.display()));
        }
        index.push(json!({
            "name": package.name,
            "version": package.version,
            "license": package.license,
            "repository": package.repository,
            "files": copied,
        }));
    }
    fs::write(
        output.join("index.json"),
        serde_json::to_vec_pretty(&index)?
            .into_iter()
            .chain([b'\n'])
            .collect::<Vec<_>>(),
    )?;
    println!(
        "Collected license texts for {} locked dependencies.",
        index.len()
    );
    Ok(())
}

pub fn check_cargo_package(path: &Path) -> Result<usize> {
    let decoder = flate2::read::GzDecoder::new(File::open(path)?);
    let mut archive = Archive::new(decoder);
    let mut names = Vec::new();
    for entry in archive.entries()? {
        names.push(entry?.path()?.into_owned());
    }
    let forbidden: Vec<_> = names
        .iter()
        .filter(|name| {
            name.components().any(|part| {
                matches!(part, Component::Normal(value) if value == "dist" || value == "target")
            })
        })
        .collect();
    ensure!(
        forbidden.is_empty(),
        "generated files in {}:\n{}",
        path.display(),
        forbidden
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    ensure!(!names.is_empty(), "empty Cargo archive: {}", path.display());
    Ok(names.len())
}

fn append(
    builder: &mut Builder<flate2::write::GzEncoder<File>>,
    root: &str,
    relative: &Path,
    source: &Path,
    epoch: u64,
) -> Result<()> {
    let metadata = source.symlink_metadata()?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "release input must be a regular file: {}",
        relative.display()
    );
    let contents = fs::read(source)?;
    let mut header = Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(if metadata.permissions().mode() & 0o111 != 0 {
        0o755
    } else {
        0o644
    });
    header.set_mtime(epoch);
    header.set_uid(0);
    header.set_gid(0);
    header.set_username("root")?;
    header.set_groupname("root")?;
    header.set_cksum();
    builder.append_data(
        &mut header,
        Path::new(root).join(relative),
        Cursor::new(contents),
    )?;
    Ok(())
}

fn archive(destination: &Path, root: &str, files: &[(PathBuf, PathBuf)], epoch: u64) -> Result<()> {
    let encoder = GzBuilder::new()
        .mtime(u32::try_from(epoch).context("SOURCE_DATE_EPOCH exceeds gzip range")?)
        .write(File::create(destination)?, Compression::best());
    let mut builder = Builder::new(encoder);
    let mut files = files.to_vec();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    for (relative, source) in files {
        append(&mut builder, root, &relative, &source, epoch)?;
    }
    builder.into_inner()?.finish()?;
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn tracked_files(repo: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let output = command_output(
        Command::new("git")
            .args(["ls-files", "-z"])
            .current_dir(repo),
    )?;
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| {
            let relative = PathBuf::from(std::str::from_utf8(name)?);
            Ok((relative.clone(), repo.join(relative)))
        })
        .collect()
}

fn files_under(root: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut files = Vec::new();
    let mut absolute = Vec::new();
    walk_files(root, &mut absolute)?;
    for path in absolute {
        files.push((path.strip_prefix(root)?.to_path_buf(), path));
    }
    Ok(files)
}

fn copy(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<()> {
    fs::copy(source.as_ref(), destination.as_ref()).with_context(|| {
        format!(
            "copy {} to {}",
            source.as_ref().display(),
            destination.as_ref().display()
        )
    })?;
    Ok(())
}

pub fn release(output: &Path) -> Result<()> {
    let repo = repo();
    ensure!(
        text(Command::new("uname").arg("-s"))? == "Darwin",
        "binary releases must be built on macOS"
    );
    ensure!(
        text(Command::new("uname").arg("-m"))? == "arm64",
        "binary releases must be built on an Apple Silicon Mac"
    );
    ensure!(
        text(
            Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=all"])
                .current_dir(&repo)
        )?
        .is_empty(),
        "release requires a clean, committed checkout"
    );
    ensure!(!output.exists(), "release output must not already exist");
    let metadata: serde_json::Value = serde_json::from_slice(
        &command_output(
            Command::new("cargo")
                .args(["metadata", "--no-deps", "--format-version", "1"])
                .current_dir(&repo),
        )?
        .stdout,
    )?;
    let version = metadata["packages"]
        .as_array()
        .context("Cargo metadata has no packages array")?
        .iter()
        .find(|package| package["name"] == "luhmen")
        .and_then(|package| package["version"].as_str())
        .context("luhmen package missing from Cargo metadata")?;
    let epoch: u64 = match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(value) => value.parse().context("invalid SOURCE_DATE_EPOCH")?,
        Err(_) => text(
            Command::new("git")
                .args(["show", "-s", "--format=%ct", "HEAD"])
                .current_dir(&repo),
        )?
        .parse()?,
    };
    let temporary = TempDir::new_in(std::env::temp_dir())?;
    let (executable, environment) = build_release(
        &repo,
        &temporary.path().join("target"),
        Some(RELEASE_TARGET),
    )?;
    let package = temporary.path().join("package");
    fs::create_dir_all(package.join("bin"))?;
    copy(&executable, package.join("bin/luhmen"))?;
    let notices = package.join("share/licenses/luhmen");
    fs::create_dir_all(&notices)?;
    for name in ["LICENSE", "NOTICE", "THIRD_PARTY.md"] {
        copy(repo.join(name), notices.join(name))?;
    }
    collect_licenses(&notices.join("dependencies"), Some(RELEASE_TARGET))?;
    for name in [
        "README.md",
        "CONTRIBUTING.md",
        "SECURITY.md",
        "LICENSE",
        "NOTICE",
        "THIRD_PARTY.md",
    ] {
        copy(repo.join(name), package.join(name))?;
    }
    copy_tree(&repo.join("docs"), &package.join("docs"))?;
    let environment: std::collections::BTreeMap<_, _> = environment.into_iter().collect();
    let build_info = json!({
        "version": version,
        "commit": text(Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&repo))?,
        "source_date_epoch": epoch,
        "target": RELEASE_TARGET,
        "rustc": text(Command::new("rustc").arg("--version"))?,
        "sdk_version": text(Command::new("xcrun").args(["--sdk", "macosx", "--show-sdk-version"]))?,
        "macos_deployment_target": environment["MACOSX_DEPLOYMENT_TARGET"],
    });
    fs::write(
        package.join("build-info.json"),
        serde_json::to_vec_pretty(&build_info)?
            .into_iter()
            .chain([b'\n'])
            .collect::<Vec<_>>(),
    )?;
    fs::create_dir_all(output)?;
    let binary_name = format!("luhmen-{version}-{RELEASE_TARGET}");
    archive(
        &output.join(format!("{binary_name}.tar.gz")),
        &binary_name,
        &files_under(&package)?,
        epoch,
    )?;
    let source_name = format!("luhmen-{version}-source");
    archive(
        &output.join(format!("{source_name}.tar.gz")),
        &source_name,
        &tracked_files(&repo)?,
        epoch,
    )?;
    let mut archives: Vec<_> = fs::read_dir(output)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("gz")))
        .collect();
    archives.sort();
    let mut sums = String::new();
    for path in archives {
        let digest = Sha256::digest(fs::read(&path)?);
        sums.push_str(&format!(
            "{digest:x}  {}\n",
            path.file_name().unwrap().to_string_lossy()
        ));
    }
    fs::write(output.join("SHA256SUMS"), sums)?;
    println!(
        "Created release archives and checksums in {}",
        output.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn home_path_detection_is_bounded() {
        assert!(machine_home_path(&format!(
            "path=/{}/alice/project",
            "Users"
        )));
        assert!(machine_home_path(&format!(
            "path=/{}/alice/project",
            "home"
        )));
        assert!(!machine_home_path("https://example.test/homepage"));
        assert!(!machine_home_path(&format!("/{}/$USER/project", "Users")));
    }

    #[test]
    fn archive_is_reproducible_and_normalized() -> Result<()> {
        let root = TempDir::new()?;
        let data = root.path().join("data");
        let executable = root.path().join("executable");
        fs::write(&data, "payload\n")?;
        fs::write(&executable, "#!/bin/sh\nexit 0\n")?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
        let files = vec![
            (PathBuf::from("data"), data),
            (PathBuf::from("bin/luhmen"), executable),
        ];
        let first = root.path().join("first.tar.gz");
        let second = root.path().join("second.tar.gz");
        archive(&first, "luhmen-test", &files, 1_000_000_000)?;
        archive(
            &second,
            "luhmen-test",
            &files.into_iter().rev().collect::<Vec<_>>(),
            1_000_000_000,
        )?;
        assert_eq!(fs::read(&first)?, fs::read(&second)?);
        let mut archive = Archive::new(flate2::read::GzDecoder::new(File::open(first)?));
        let entries = archive
            .entries()?
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.path().unwrap().into_owned(),
                    entry.header().mode().unwrap(),
                    entry.header().uid().unwrap(),
                    entry.header().gid().unwrap(),
                    entry.header().mtime().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            entries[0],
            (
                PathBuf::from("luhmen-test/bin/luhmen"),
                0o755,
                0,
                0,
                1_000_000_000
            )
        );
        assert_eq!(
            entries[1],
            (
                PathBuf::from("luhmen-test/data"),
                0o644,
                0,
                0,
                1_000_000_000
            )
        );
        Ok(())
    }

    #[test]
    fn archive_refuses_symlinks() -> Result<()> {
        let root = TempDir::new()?;
        fs::write(root.path().join("file"), "payload")?;
        symlink(root.path().join("file"), root.path().join("link"))?;
        let result = archive(
            &root.path().join("output.tar.gz"),
            "test",
            &[(PathBuf::from("link"), root.path().join("link"))],
            1,
        );
        assert!(result.unwrap_err().to_string().contains("regular file"));
        Ok(())
    }

    #[test]
    fn cargo_archive_rejects_generated_directories() -> Result<()> {
        let root = TempDir::new()?;
        let archive_path = root.path().join("fixture.crate");
        let encoder = GzBuilder::new().write(File::create(&archive_path)?, Compression::fast());
        let mut builder = Builder::new(encoder);
        let payload = b"generated";
        let mut header = Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, "example-1.0.0/target/output", &payload[..])?;
        builder.into_inner()?.finish()?;
        assert!(
            check_cargo_package(&archive_path)
                .unwrap_err()
                .to_string()
                .contains("generated files")
        );
        Ok(())
    }

    #[test]
    fn declared_license_cannot_escape_package() -> Result<()> {
        let root = TempDir::new()?;
        let package_root = root.path().join("package");
        fs::create_dir(&package_root)?;
        fs::write(package_root.join("Cargo.toml"), "")?;
        let outside = root.path().join("outside-license");
        fs::write(&outside, "license")?;
        let package = Package {
            id: "example@1.0.0".to_owned(),
            name: "example".to_owned(),
            version: "1.0.0".to_owned(),
            manifest_path: package_root.join("Cargo.toml"),
            license: Some("MIT".to_owned()),
            repository: None,
            license_file: Some(PathBuf::from("../outside-license")),
        };
        let error = license_files(&package, &package_root).unwrap_err();
        assert!(error.to_string().contains("escapes package"));
        Ok(())
    }
}
