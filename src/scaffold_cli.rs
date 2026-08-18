use std::{collections::BTreeMap, path::Path, time::Duration};

use serde_json::{json, Value};

use crate::{
    parse_request,
    store::{ReadOnlyDatabasePermit, WritableDatabasePermit},
    DatabaseAccess, ErrorCategory, KernelError, KernelOutcome, MissionKernel, Operation,
    OperationKind, OperationResult, OutcomeBody, BINARY_CONTRACT, PROTOCOL_VERSION,
};

pub const EXIT_UNSUPPORTED_OPERATION: i32 = 64;
pub const EXIT_MALFORMED_INPUT: i32 = 65;
pub const EXIT_UNKNOWN_PROTOCOL: i32 = 66;
pub const EXIT_INCOMPATIBLE_CONTRACT: i32 = 67;
pub const EXIT_SCAFFOLD_ONLY: i32 = 70;

#[derive(Debug)]
pub struct CliResponse {
    pub outcome: KernelOutcome,
    pub diagnostic: String,
    pub exit_code: i32,
}

pub fn process_fixture_request(command: Option<&str>, input: &str) -> CliResponse {
    let value: Value = match serde_json::from_str(input) {
        Ok(value) => value,
        Err(error) => {
            return error_response(
                None,
                ErrorCategory::Transport,
                "malformed_json",
                format!("stdin is not one valid JSON request: {error}"),
                EXIT_MALFORMED_INPUT,
                BTreeMap::new(),
            );
        }
    };

    let request_id = value
        .get("request_id")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let protocol = value.get("protocol").and_then(Value::as_str);
    if protocol != Some(PROTOCOL_VERSION) {
        return mismatch_response(
            request_id,
            ErrorCategory::Protocol,
            "unknown_protocol",
            "unsupported protocol version",
            EXIT_UNKNOWN_PROTOCOL,
            protocol,
            PROTOCOL_VERSION,
        );
    }

    let binary_contract = value.get("binary_contract").and_then(Value::as_str);
    if binary_contract != Some(BINARY_CONTRACT) {
        return mismatch_response(
            request_id,
            ErrorCategory::Contract,
            "incompatible_binary_contract",
            "incompatible binary contract",
            EXIT_INCOMPATIBLE_CONTRACT,
            binary_contract,
            BINARY_CONTRACT,
        );
    }

    let operation_tag = value.pointer("/operation/type").and_then(Value::as_str);
    let Some(operation_kind) = operation_tag.and_then(OperationKind::from_tag) else {
        return mismatch_response(
            request_id,
            ErrorCategory::Operation,
            "unsupported_operation",
            "unsupported operation tag",
            EXIT_UNSUPPORTED_OPERATION,
            operation_tag,
            "handle, drive, or inspect",
        );
    };

    if command != Some(operation_kind.as_str()) {
        return mismatch_response(
            request_id,
            ErrorCategory::Operation,
            "operation_mismatch",
            "CLI command does not match request operation",
            EXIT_UNSUPPORTED_OPERATION,
            command,
            operation_kind.as_str(),
        );
    }

    if let Err(error) = parse_request(value) {
        return error_response(
            request_id,
            ErrorCategory::Transport,
            "invalid_request_schema",
            format!("request does not match the versioned schema: {error}"),
            EXIT_MALFORMED_INPUT,
            BTreeMap::new(),
        );
    }

    error_response(
        request_id,
        ErrorCategory::Internal,
        "standalone_scaffold_only",
        "standalone Phase 2 harness validates fixtures but does not execute Mission state",
        EXIT_SCAFFOLD_ONLY,
        BTreeMap::new(),
    )
}

pub fn process_temporary_fixture_request(
    command: Option<&str>,
    input: &str,
    temporary_root: &Path,
) -> CliResponse {
    let value: Value = match serde_json::from_str(input) {
        Ok(value) => value,
        Err(error) => {
            return error_response(
                None,
                ErrorCategory::Transport,
                "malformed_json",
                format!("stdin is not one valid JSON request: {error}"),
                EXIT_MALFORMED_INPUT,
                BTreeMap::new(),
            )
        }
    };
    let request = match parse_request(value) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                None,
                ErrorCategory::Transport,
                "invalid_request_schema",
                error.to_string(),
                EXIT_MALFORMED_INPUT,
                BTreeMap::new(),
            )
        }
    };
    if request.protocol != PROTOCOL_VERSION || request.binary_contract != BINARY_CONTRACT {
        return process_fixture_request(command, input);
    }
    let operation_kind = request.operation.kind();
    if command != Some(operation_kind.as_str()) {
        return mismatch_response(
            Some(request.request_id),
            ErrorCategory::Operation,
            "operation_mismatch",
            "CLI command does not match request operation",
            EXIT_UNSUPPORTED_OPERATION,
            command,
            operation_kind.as_str(),
        );
    }
    let database_path = Path::new(&request.database.path);
    let permit = match WritableDatabasePermit::for_temporary_fixture(temporary_root, database_path)
    {
        Ok(permit) => permit,
        Err(error) => return kernel_error_response(Some(request.request_id), error),
    };
    let mut kernel = match MissionKernel::open_temporary_sqlite_v3(
        &request.mission.mission_id,
        permit,
        Duration::from_millis(25),
    ) {
        Ok(kernel) => kernel,
        Err(error) => return kernel_error_response(Some(request.request_id), error),
    };
    let request_id = request.request_id;
    let result = match request.operation {
        Operation::Handle(handle) if request.database.access == DatabaseAccess::ReadWrite => kernel
            .handle(crate::KernelInput {
                decision_context: request.decision_context,
                input: handle.input,
            })
            .map(OperationResult::Handle),
        Operation::Inspect(inspect) if request.database.access == DatabaseAccess::ReadOnly => {
            kernel.inspect(inspect.query).map(OperationResult::Inspect)
        }
        Operation::Drive(mut drive) if request.database.access == DatabaseAccess::ReadWrite => {
            if drive
                .claim_owner
                .as_deref()
                .map(str::is_empty)
                .unwrap_or(true)
            {
                drive.claim_owner = Some(format!("fixture-driver:{request_id}"));
            }
            kernel
                .drive(drive, request.decision_context)
                .map(OperationResult::Drive)
        }
        _ => Err(KernelError {
            category: ErrorCategory::Contract,
            code: "database_access_mismatch".into(),
            message: "operation does not match the declared database access".into(),
            retryable: false,
            details: BTreeMap::new(),
        }),
    };
    match result {
        Ok(result) => CliResponse {
            outcome: KernelOutcome {
                protocol: PROTOCOL_VERSION.into(),
                binary_contract: BINARY_CONTRACT.into(),
                request_id: Some(request_id),
                outcome: OutcomeBody::Success {
                    operation: operation_kind,
                    result,
                },
            },
            diagnostic: String::new(),
            exit_code: 0,
        },
        Err(error) => kernel_error_response(Some(request_id), error),
    }
}

pub fn process_read_only_canary_request(
    command: Option<&str>,
    input: &str,
    permitted_database: &Path,
) -> CliResponse {
    let value: Value = match serde_json::from_str(input) {
        Ok(value) => value,
        Err(error) => {
            return error_response(
                None,
                ErrorCategory::Transport,
                "malformed_json",
                format!("stdin is not one valid JSON request: {error}"),
                EXIT_MALFORMED_INPUT,
                BTreeMap::new(),
            )
        }
    };
    let request = match parse_request(value) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                None,
                ErrorCategory::Transport,
                "invalid_request_schema",
                error.to_string(),
                EXIT_MALFORMED_INPUT,
                BTreeMap::new(),
            )
        }
    };
    if request.protocol != PROTOCOL_VERSION || request.binary_contract != BINARY_CONTRACT {
        return process_fixture_request(command, input);
    }
    let operation_kind = request.operation.kind();
    if command != Some(operation_kind.as_str()) {
        return mismatch_response(
            Some(request.request_id),
            ErrorCategory::Operation,
            "operation_mismatch",
            "CLI command does not match request operation",
            EXIT_UNSUPPORTED_OPERATION,
            command,
            operation_kind.as_str(),
        );
    }
    if !matches!(request.operation, Operation::Inspect(_))
        || request.database.access != DatabaseAccess::ReadOnly
    {
        return error_response(
            Some(request.request_id),
            ErrorCategory::Contract,
            "read_only_operation_required",
            "read-only canary permits only inspect with read_only database access",
            EXIT_SCAFFOLD_ONLY,
            BTreeMap::new(),
        );
    }
    let permit = match ReadOnlyDatabasePermit::for_exact_path(
        permitted_database,
        Path::new(&request.database.path),
    ) {
        Ok(permit) => permit,
        Err(error) => return kernel_error_response(Some(request.request_id), error),
    };
    let kernel = match MissionKernel::open_read_only_sqlite_v3(&request.mission.mission_id, permit)
    {
        Ok(kernel) => kernel,
        Err(error) => return kernel_error_response(Some(request.request_id), error),
    };
    let request_id = request.request_id;
    let Operation::Inspect(inspect) = request.operation else {
        unreachable!("read-only operation was validated before database open")
    };
    match kernel.inspect(inspect.query) {
        Ok(view) => CliResponse {
            outcome: KernelOutcome {
                protocol: PROTOCOL_VERSION.into(),
                binary_contract: BINARY_CONTRACT.into(),
                request_id: Some(request_id),
                outcome: OutcomeBody::Success {
                    operation: OperationKind::Inspect,
                    result: OperationResult::Inspect(view),
                },
            },
            diagnostic: String::new(),
            exit_code: 0,
        },
        Err(error) => kernel_error_response(Some(request_id), error),
    }
}

fn kernel_error_response(request_id: Option<String>, error: KernelError) -> CliResponse {
    let diagnostic = format!("{}: {}", error.code, error.message);
    CliResponse {
        outcome: KernelOutcome {
            protocol: PROTOCOL_VERSION.into(),
            binary_contract: BINARY_CONTRACT.into(),
            request_id,
            outcome: OutcomeBody::Error { error },
        },
        diagnostic,
        exit_code: EXIT_SCAFFOLD_ONLY,
    }
}

fn mismatch_response(
    request_id: Option<String>,
    category: ErrorCategory,
    code: &str,
    message: &str,
    exit_code: i32,
    received: Option<&str>,
    expected: &str,
) -> CliResponse {
    let mut details = BTreeMap::new();
    details.insert("expected".into(), json!(expected));
    details.insert("received".into(), json!(received));
    error_response(request_id, category, code, message, exit_code, details)
}

fn error_response(
    request_id: Option<String>,
    category: ErrorCategory,
    code: &str,
    message: impl Into<String>,
    exit_code: i32,
    details: BTreeMap<String, Value>,
) -> CliResponse {
    let error = KernelError {
        category,
        code: code.into(),
        message: message.into(),
        retryable: false,
        details,
    };
    let diagnostic = format!("{}: {}", error.code, error.message);
    CliResponse {
        outcome: KernelOutcome {
            protocol: PROTOCOL_VERSION.into(),
            binary_contract: BINARY_CONTRACT.into(),
            request_id,
            outcome: OutcomeBody::Error { error },
        },
        diagnostic,
        exit_code,
    }
}
