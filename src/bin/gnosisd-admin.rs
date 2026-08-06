use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, Read, Write},
    os::fd::OwnedFd,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    process::ExitCode,
};

use chrono::Utc;
use clap::{Parser, Subcommand};
use gnosis_core::protocol::{
    ClientError, ErrorCode, PROTOCOL_VERSION, ProtocolError, Request, RequestEnvelope,
    ResponseOutcome, ResponseResult, request,
};
use rustix::fs::{AtFlags, Mode, OFlags, fchmod, fsync, linkat, open, openat, unlinkat};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const MAX_CREDENTIAL_FILE_SIZE: u64 = 4 * 1024;

#[derive(Debug, Parser)]
#[command(name = "gnosisd-admin")]
struct Cli {
    #[arg(long, value_name = "PATH")]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    SystemInfo,
    DeviceCreate {
        name: String,
        #[arg(long = "scope", required = true)]
        scopes: Vec<String>,
        #[arg(long, value_name = "PATH")]
        credential_file: PathBuf,
    },
    DeviceList,
    DeviceRevoke {
        device_id: String,
    },
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    Repair {
        #[arg(long, default_value_t = 100, value_name = "N")]
        batch_size: u16,
    },
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
    },
}

#[derive(Debug, Subcommand)]
enum BackupCommand {
    Create {
        #[arg(required_unless_present = "directory", conflicts_with = "directory")]
        destination: Option<PathBuf>,
        #[arg(long, value_name = "PATH")]
        directory: Option<PathBuf>,
    },
    Verify {
        path: PathBuf,
    },
    Prune {
        #[arg(long, value_name = "PATH")]
        directory: PathBuf,
        #[arg(long, default_value_t = 14)]
        keep: u16,
    },
    RestoreValidate {
        path: PathBuf,
        #[arg(long, value_name = "PATH")]
        temporary_directory: PathBuf,
    },
}

#[derive(Debug, Error)]
enum CliError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("backup path is not valid UTF-8")]
    Path,
    #[error("could not encode daemon response")]
    Encode(#[source] serde_json::Error),
    #[error("could not write daemon response")]
    WriteOutput(#[source] io::Error),
    #[error("credential file is invalid")]
    Credential,
    #[error("credential file operation failed")]
    CredentialIo(#[source] io::Error),
    #[error("device credential generation failed")]
    CredentialGeneration,
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CredentialFile {
    device_id: String,
    token: String,
    name: String,
    scopes: Vec<String>,
}

struct PreparedRequest {
    request: Request,
    credential_path: Option<PathBuf>,
}

#[derive(Serialize)]
struct DeviceCreatedOutput<'a> {
    device: &'a gnosis_core::protocol::DeviceView,
    credential_path: &'a Path,
}

#[derive(Debug, Error)]
enum RunError {
    #[error(transparent)]
    Client(#[from] CliError),
    #[error("protocol request failed")]
    Protocol(ProtocolError),
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(RunError::Protocol(error)) => {
            eprintln!("{}: {}", error_code(error.code), error.message);
            ExitCode::FAILURE
        }
        Err(RunError::Client(error)) => {
            eprintln!("gnosisd-admin: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), RunError> {
    let prepared = command_request(cli.command)?;
    let envelope = RequestEnvelope {
        version: PROTOCOL_VERSION,
        id: correlation_id(),
        request: prepared.request,
    };
    let response = request(cli.socket, &envelope)
        .await
        .map_err(CliError::from)?;
    match response.outcome {
        ResponseOutcome::Success { result } => {
            write_result(&result, prepared.credential_path.as_deref()).map_err(Into::into)
        }
        ResponseOutcome::Failure { error } => Err(RunError::Protocol(error)),
    }
}

fn command_request(command: Command) -> Result<PreparedRequest, CliError> {
    let (request, credential_path) = match command {
        Command::SystemInfo => (Request::SystemInfo {}, None),
        Command::DeviceCreate {
            name,
            scopes,
            credential_file,
        } => {
            if credential_file.to_str().is_none() {
                return Err(CliError::Credential);
            }
            let credential = prepare_credential(&credential_file, &name, &scopes)?;
            (
                Request::DeviceCreate {
                    device_id: credential.device_id,
                    name: credential.name,
                    token: credential.token,
                    scopes: credential.scopes,
                },
                Some(credential_file),
            )
        }
        Command::DeviceList => (Request::DeviceList {}, None),
        Command::DeviceRevoke { device_id } => (Request::DeviceRevoke { device_id }, None),
        Command::Admin { command } => (admin_request(command)?, None),
    };
    Ok(PreparedRequest {
        request,
        credential_path,
    })
}

fn prepare_credential(
    path: &Path,
    name: &str,
    scopes: &[String],
) -> Result<CredentialFile, CliError> {
    match fs::symlink_metadata(path) {
        Ok(_) => load_credential(path, name, scopes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let generated = gnosis_core::generate_provisioning_credential(name, scopes)
                .map_err(|_| CliError::CredentialGeneration)?;
            let credential = CredentialFile {
                device_id: generated.device_id,
                token: generated.token,
                name: name.to_owned(),
                scopes: scopes.to_vec(),
            };
            match install_credential(path, &credential) {
                Ok(()) => Ok(credential),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    load_credential(path, name, scopes)
                }
                Err(error) => Err(CliError::CredentialIo(error)),
            }
        }
        Err(error) => Err(CliError::CredentialIo(error)),
    }
}

fn load_credential(path: &Path, name: &str, scopes: &[String]) -> Result<CredentialFile, CliError> {
    let (parent, file_name) = open_credential_parent(path).map_err(CliError::CredentialIo)?;
    let mut file = File::from(
        openat(
            &parent,
            &file_name,
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(io::Error::from)
        .map_err(CliError::CredentialIo)?,
    );
    let metadata = file.metadata().map_err(CliError::CredentialIo)?;
    if !metadata.is_file()
        || metadata.len() > MAX_CREDENTIAL_FILE_SIZE
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(CliError::Credential);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take(MAX_CREDENTIAL_FILE_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(CliError::CredentialIo)?;
    if bytes.len() as u64 > MAX_CREDENTIAL_FILE_SIZE {
        return Err(CliError::Credential);
    }
    let credential: CredentialFile =
        serde_json::from_slice(&bytes).map_err(|_| CliError::Credential)?;
    if credential.name != name || credential.scopes != scopes {
        return Err(CliError::Credential);
    }
    gnosis_core::validate_provisioning_credential(
        &credential.device_id,
        &credential.name,
        &credential.token,
        &credential.scopes,
    )
    .map_err(|_| CliError::Credential)?;
    file.sync_all().map_err(CliError::CredentialIo)?;
    fsync(&parent)
        .map_err(io::Error::from)
        .map_err(CliError::CredentialIo)?;
    let current = File::from(
        openat(
            &parent,
            &file_name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(io::Error::from)
        .map_err(CliError::CredentialIo)?,
    );
    let current_metadata = current.metadata().map_err(CliError::CredentialIo)?;
    if current_metadata.dev() != metadata.dev() || current_metadata.ino() != metadata.ino() {
        return Err(CliError::Credential);
    }
    Ok(credential)
}

fn install_credential(path: &Path, credential: &CredentialFile) -> io::Result<()> {
    let (parent, file_name) = open_credential_parent(path)?;
    let mut temporary = OsString::from(".");
    temporary.push(&file_name);
    temporary.push(format!(".{}.tmp", Uuid::now_v7()));
    let bytes = serde_json::to_vec(credential)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if bytes.len() as u64 > MAX_CREDENTIAL_FILE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "credential file exceeds size limit",
        ));
    }
    let result = (|| {
        let mut file = File::from(
            openat(
                &parent,
                &temporary,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(io::Error::from)?,
        );
        fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(io::Error::from)?;
        if file.metadata()?.mode() & 0o777 != 0o600 {
            return Err(io::Error::other("credential mode was not applied"));
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        linkat(&parent, &temporary, &parent, &file_name, AtFlags::empty())
            .map_err(io::Error::from)?;
        fsync(&parent).map_err(io::Error::from)?;
        unlinkat(&parent, &temporary, AtFlags::empty()).map_err(io::Error::from)?;
        fsync(&parent).map_err(io::Error::from)
    })();
    if result.is_err() {
        let _ = unlinkat(&parent, &temporary, AtFlags::empty());
    }
    result
}

fn open_credential_parent(path: &Path) -> io::Result<(OwnedFd, OsString)> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::other("invalid credential path"))?
        .to_owned();
    let parent = open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    Ok((parent, file_name))
}

fn admin_request(command: AdminCommand) -> Result<Request, CliError> {
    match command {
        AdminCommand::Repair { batch_size } => Ok(Request::Repair { batch_size }),
        AdminCommand::Backup { command } => match command {
            BackupCommand::Create {
                destination,
                directory,
            } => Ok(Request::BackupCreate {
                destination: utf8_path(backup_destination(destination, directory)?)?,
            }),
            BackupCommand::Verify { path } => Ok(Request::BackupVerify {
                path: utf8_path(path)?,
            }),
            BackupCommand::Prune { directory, keep } => Ok(Request::BackupPrune {
                directory: utf8_path(directory)?,
                keep,
            }),
            BackupCommand::RestoreValidate {
                path,
                temporary_directory,
            } => Ok(Request::BackupRestoreValidate {
                path: utf8_path(path)?,
                temporary_directory: utf8_path(temporary_directory)?,
            }),
        },
    }
}

fn backup_destination(
    destination: Option<PathBuf>,
    directory: Option<PathBuf>,
) -> Result<PathBuf, CliError> {
    match (destination, directory) {
        (Some(path), None) => Ok(path),
        (None, Some(directory)) => {
            Ok(directory.join(format!("gnosis-{}", Utc::now().format("%Y%m%dT%H%M%SZ"))))
        }
        _ => Err(CliError::Path),
    }
}

fn utf8_path(path: PathBuf) -> Result<String, CliError> {
    path.into_os_string()
        .into_string()
        .map_err(|_| CliError::Path)
}

fn correlation_id() -> String {
    format!("gnosisd-admin-{}", Uuid::now_v7())
}

fn write_result(result: &ResponseResult, credential_path: Option<&Path>) -> Result<(), CliError> {
    match result {
        ResponseResult::SystemInfo(value) => write_json(value),
        ResponseResult::DeviceCreated(value) => write_json(&DeviceCreatedOutput {
            device: &value.device,
            credential_path: credential_path.ok_or(CliError::Credential)?,
        }),
        ResponseResult::Devices(value) => write_json(value),
        ResponseResult::Device(value) => write_json(value),
        ResponseResult::Backup(value) => write_json(value),
        ResponseResult::BackupsPruned(value) => write_json(value),
        ResponseResult::Repair(value) => write_json(value),
    }
}

fn write_json(value: &impl Serialize) -> Result<(), CliError> {
    let body = serde_json::to_vec(value).map_err(CliError::Encode)?;
    let mut output = io::stdout().lock();
    output.write_all(&body).map_err(CliError::WriteOutput)?;
    writeln!(output).map_err(CliError::WriteOutput)
}

fn error_code(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::MalformedRequest => "malformed_request",
        ErrorCode::UnsupportedVersion => "unsupported_version",
        ErrorCode::InvalidDocument => "invalid_document",
        ErrorCode::NotFound => "not_found",
        ErrorCode::Conflict => "conflict",
        ErrorCode::RevisionConflict => "revision_conflict",
        ErrorCode::IdempotencyConflict => "idempotency_conflict",
        ErrorCode::StaleAttempt => "stale_attempt",
        ErrorCode::ReviewStateConflict => "review_state_conflict",
        ErrorCode::Busy => "busy",
        ErrorCode::Internal => "internal",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        ffi::OsString,
        fs,
        os::unix::{
            ffi::OsStringExt as _,
            fs::{PermissionsExt as _, symlink},
        },
        process::Command as ProcessCommand,
    };

    use clap::Parser as _;
    use gnosis_core::protocol::{
        DeviceCreatedView, DeviceView, MAX_REPAIR_BATCH_SIZE, ResponseResult,
    };
    use tempfile::tempdir;

    use super::{
        AdminCommand, BackupCommand, Cli, Command, DeviceCreatedOutput, Request, command_request,
        prepare_credential,
    };

    #[test]
    fn parses_device_and_maintenance_commands() {
        let directory = tempdir().unwrap();
        let credential_path = directory.path().join("device.json");
        let create = Cli::try_parse_from([
            "gnosisd-admin",
            "--socket",
            "/tmp/gnosis.sock",
            "device-create",
            "emacs",
            "--scope",
            "read",
            "--scope",
            "review",
            "--credential-file",
            credential_path.to_str().unwrap(),
        ])
        .unwrap();
        let prepared = command_request(create.command).unwrap();
        assert!(matches!(
            prepared.request,
            Request::DeviceCreate { device_id, name, token, scopes }
                if !device_id.is_empty()
                    && name == "emacs"
                    && token.starts_with("gnosis_")
                    && scopes == ["read", "review"]
        ));

        let repair = Cli::try_parse_from([
            "gnosisd-admin",
            "--socket",
            "/tmp/gnosis.sock",
            "admin",
            "repair",
            "--batch-size",
            &MAX_REPAIR_BATCH_SIZE.to_string(),
        ])
        .unwrap();
        assert!(matches!(
            command_request(repair.command).unwrap().request,
            Request::Repair {
                batch_size: MAX_REPAIR_BATCH_SIZE
            }
        ));
    }

    #[test]
    fn backup_defaults_remain_bounded_and_explicit() {
        let prune = command_request(Command::Admin {
            command: AdminCommand::Backup {
                command: BackupCommand::Prune {
                    directory: "/backups".into(),
                    keep: 14,
                },
            },
        })
        .unwrap()
        .request;
        assert_eq!(
            prune,
            Request::BackupPrune {
                directory: "/backups".into(),
                keep: 14,
            }
        );
    }

    #[test]
    fn credential_file_is_private_replayable_and_argument_bound() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("device.json");
        let scopes = vec!["read".into(), "review".into()];
        let first = prepare_credential(&path, "emacs", &scopes).unwrap();
        let bytes = fs::read(&path).unwrap();
        let replay = prepare_credential(&path, "emacs", &scopes).unwrap();

        assert!(first == replay);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(prepare_credential(&path, "other", &scopes).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn credential_file_forces_private_mode_under_restrictive_umask() {
        const PATH_ENV: &str = "GNOSIS_TEST_CREDENTIAL_PATH";
        if let Some(path) = env::var_os(PATH_ENV) {
            let path = std::path::PathBuf::from(path);
            let scopes = vec!["read".into()];
            let first = prepare_credential(&path, "emacs", &scopes).unwrap();
            let replay = prepare_credential(&path, "emacs", &scopes).unwrap();
            assert!(first == replay);
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            return;
        }

        let directory = tempdir().unwrap();
        let path = directory.path().join("device.json");
        let status = ProcessCommand::new("sh")
            .args([
                "-c",
                "umask 400; exec \"$1\" --exact tests::credential_file_forces_private_mode_under_restrictive_umask",
                "sh",
            ])
            .arg(env::current_exe().unwrap())
            .env(PATH_ENV, &path)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn device_create_rejects_non_utf8_credential_path_before_publication() {
        let directory = tempdir().unwrap();
        let path = directory
            .path()
            .join(OsString::from_vec(b"device-\xff.json".to_vec()));
        let result = command_request(Command::DeviceCreate {
            name: "emacs".into(),
            scopes: vec!["read".into()],
            credential_file: path.clone(),
        });
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn credential_file_rejects_unsafe_or_malformed_existing_paths_untouched() {
        let directory = tempdir().unwrap();
        let scopes = vec!["read".into()];
        let malformed = directory.path().join("malformed.json");
        fs::write(&malformed, b"not json").unwrap();
        fs::set_permissions(&malformed, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(prepare_credential(&malformed, "emacs", &scopes).is_err());
        assert_eq!(fs::read(&malformed).unwrap(), b"not json");

        let target = directory.path().join("target.json");
        fs::write(&target, b"target").unwrap();
        let link = directory.path().join("link.json");
        symlink(&target, &link).unwrap();
        assert!(prepare_credential(&link, "emacs", &scopes).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"target");

        let non_regular = directory.path().join("directory");
        fs::create_dir(&non_regular).unwrap();
        assert!(prepare_credential(&non_regular, "emacs", &scopes).is_err());

        let real_parent = directory.path().join("real-parent");
        fs::create_dir(&real_parent).unwrap();
        let linked_parent = directory.path().join("linked-parent");
        symlink(&real_parent, &linked_parent).unwrap();
        assert!(prepare_credential(&linked_parent.join("device.json"), "emacs", &scopes).is_err());
        assert!(!real_parent.join("device.json").exists());
    }

    #[test]
    fn device_created_output_contains_path_but_no_token() {
        let created = DeviceCreatedView {
            device: DeviceView {
                device_id: "device".into(),
                name: "emacs".into(),
                scopes: vec!["read".into()],
                created_at_us: 1,
                last_used_at_us: None,
                revoked_at_us: None,
            },
        };
        let json = serde_json::to_value(DeviceCreatedOutput {
            device: &created.device,
            credential_path: std::path::Path::new("/credentials/emacs.json"),
        })
        .unwrap();
        assert_eq!(json["credential_path"], "/credentials/emacs.json");
        assert!(json.get("token").is_none());
        assert!(json["device"].get("token").is_none());
        let _ = ResponseResult::DeviceCreated(created);
    }
}
