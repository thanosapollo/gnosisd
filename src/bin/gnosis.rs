use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gnosis_core::protocol::{
    ClientError, ErrorCode, PROTOCOL_VERSION, ProtocolError, Request, RequestEnvelope,
    ResponseOutcome, ResponseResult, request,
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(name = "gnosis")]
struct Cli {
    #[arg(long, value_name = "PATH")]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    SystemInfo,
    LegacyInfo,
    ImportOrg {
        document_id: String,
        #[arg(long, value_name = "N")]
        if_revision: Option<u64>,
    },
    GetDocument {
        document_id: String,
    },
    GetThema {
        thema_id: String,
    },
    DueThemata {
        day: u32,
        #[arg(long, default_value_t = 20)]
        limit: u16,
    },
}

#[derive(Debug, Error)]
enum CliError {
    #[error("could not read Org document from standard input")]
    ReadInput(#[source] io::Error),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("could not encode daemon response")]
    Encode(#[source] serde_json::Error),
    #[error("could not write daemon response")]
    WriteOutput(#[source] io::Error),
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
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), RunError> {
    let envelope = RequestEnvelope {
        version: PROTOCOL_VERSION,
        id: "gnosis-cli".to_owned(),
        request: command_request(cli.command)?,
    };
    let response = request(cli.socket, &envelope)
        .await
        .map_err(CliError::from)?;
    match response.outcome {
        ResponseOutcome::Success { result } => write_result(&result).map_err(Into::into),
        ResponseOutcome::Failure { error } => Err(RunError::Protocol(error)),
    }
}

fn command_request(command: Command) -> Result<Request, CliError> {
    match command {
        Command::SystemInfo => Ok(Request::SystemInfo {}),
        Command::LegacyInfo => Ok(Request::LegacyInfo {}),
        Command::ImportOrg {
            document_id,
            if_revision,
        } => Ok(Request::DocumentImport {
            document_id,
            org: read_org()?,
            expected_revision: if_revision,
        }),
        Command::GetDocument { document_id } => Ok(Request::DocumentGet { document_id }),
        Command::GetThema { thema_id } => Ok(Request::ThemaGet { thema_id }),
        Command::DueThemata { day, limit } => Ok(Request::ReviewDue { day, limit }),
    }
}

fn read_org() -> Result<String, CliError> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(CliError::ReadInput)?;
    Ok(input)
}

fn write_result(result: &ResponseResult) -> Result<(), CliError> {
    match result {
        ResponseResult::SystemInfo(value) => write_json(value),
        ResponseResult::DocumentImported(value) => write_json(value),
        ResponseResult::Document(value) => write_json(value),
        ResponseResult::LegacyInfo(value) => write_json(value),
        ResponseResult::Thema(value) => write_json(value),
        ResponseResult::DueThemata(value) => write_json(value),
    }
}

fn write_json(value: &impl Serialize) -> Result<(), CliError> {
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, value).map_err(CliError::Encode)?;
    writeln!(output).map_err(CliError::WriteOutput)
}

fn error_code(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::MalformedRequest => "malformed_request",
        ErrorCode::UnsupportedVersion => "unsupported_version",
        ErrorCode::InvalidDocument => "invalid_document",
        ErrorCode::NotFound => "not_found",
        ErrorCode::Conflict => "conflict",
        ErrorCode::Busy => "busy",
        ErrorCode::Internal => "internal",
    }
}

#[derive(Debug, Error)]
enum RunError {
    #[error(transparent)]
    Client(#[from] CliError),
    #[error("protocol request failed")]
    Protocol(ProtocolError),
}
