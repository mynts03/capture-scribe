//! Thin client only; never starts the GUI, helper, recording, or a second manager.
use capture_scribe_studio_lib::{
    capture_target::{Region, SetTargetRequest, TargetTicket},
    control::{call, Operation},
};
use std::{collections::HashMap, path::Path};

struct OperationSpec {
    name: &'static str,
    description: &'static str,
    arguments: &'static [(&'static str, &'static str)],
    instance_id_required: bool,
}

const OPERATIONS: &[OperationSpec] = &[
    OperationSpec {
        name: "selection-status",
        description: "Read the range-selection status and the authenticated instance ID.",
        arguments: &[],
        instance_id_required: false,
    },
    OperationSpec {
        name: "cancel-selection",
        description: "Request cancellation of the matching range-selection request.",
        arguments: &[("--request-id", "ID")],
        instance_id_required: true,
    },
    OperationSpec {
        name: "target-displays",
        description: "Read the display information last acquired by the GUI. This does not query the displays again.",
        arguments: &[],
        instance_id_required: true,
    },
    OperationSpec {
        name: "target-set",
        description: "Set a temporary recording region for the next recording attempt using the display snapshot.",
        arguments: &[
            ("--request-id", "ID"),
            ("--display-id", "ID"),
            ("--display-ref", "REF"),
            ("--snapshot-token", "TOKEN"),
            ("--snapshot-revision", "NUMBER"),
            ("--x", "POINTS"),
            ("--y", "POINTS"),
            ("--width", "POINTS"),
            ("--height", "POINTS"),
        ],
        instance_id_required: true,
    },
    OperationSpec {
        name: "target-status",
        description: "Read the temporary recording target and its status.",
        arguments: &[],
        instance_id_required: true,
    },
    OperationSpec {
        name: "target-clear",
        description: "Clear only the matching temporary target ID and revision.",
        arguments: &[("--target-id", "ID"), ("--revision", "NUMBER")],
        instance_id_required: true,
    },
    OperationSpec {
        name: "capture-status",
        description: "Read the recording state. An idle state does not mean a recording was saved successfully.",
        arguments: &[],
        instance_id_required: true,
    },
];

fn print_help() {
    println!(
        r#"CaptureScribe Studio local control client

Usage
  capture-scribe-control --help
  capture-scribe-control OPERATION --endpoint PATH --expect-executable PATH --expect-sha256 HEX [operation arguments]

Required for every operation
  --endpoint PATH            The private endpoint.json from the approved app instance.
  --expect-executable PATH   The expected main executable of the approved app.
  --expect-sha256 HEX        The expected SHA-256 of that app executable.

Operations and their arguments"#
    );
    for spec in OPERATIONS {
        let instance_arg = if spec.instance_id_required {
            "--instance-id ID"
        } else {
            "[--instance-id ID]"
        };
        print!("  {} {instance_arg}", spec.name);
        for (flag, value) in spec.arguments {
            print!(" {flag} {value}");
        }
        println!("\n    {}", spec.description);
    }
    println!(
        r#"
--instance-id ID is the instanceId returned by an authenticated selection-status.
For cancel-selection, use the request ID from selection-status. Check status
again to confirm cancellation completed after the request was accepted.
For target-set, use a new request ID and the display ID, reference, token, and
revision returned by target-displays. Coordinates are logical points relative
to the display's top-left. X and Y must be finite and nonnegative. Width and
height must be finite and at least 16 logical points. Keep the rectangle within
the display. This target does not change saved settings. For target-clear,
use the ID and revision returned by target-set or target-status.

Control is disabled during normal startup. Get explicit user permission before
enabling or using it. It is authenticated and local to the approved app instance.
To opt in, set CAPTURE_SCRIBE_CONTROL_DIR at app startup to a private absolute
directory that does not yet exist. The app creates it and writes endpoint.json.
Verify the intended executable, its SHA-256, and the instance. Keep the endpoint
descriptor private because it contains the authentication token. This boundary
does not isolate the app from an administrator or other code under the same OS
user.

macOS example using the public README's direct launch method
  APP="/absolute/path/to/CaptureScribe.app"
  APP_MAIN="$APP/Contents/MacOS/capture-scribe-studio"
  CLI="$APP/Contents/MacOS/capture-scribe-control"
  CONTROL_DIR="/absolute/path/to/private-parent/new-control-dir"
  # After the user's approval and verification of the intended app
  CAPTURE_SCRIBE_CONTROL_DIR="$CONTROL_DIR" "$APP_MAIN"

Set the same variables in a second terminal. Compare the SHA-256 of APP_MAIN
with the trusted value for the approved app before making a request.
  APP_SHA="$(shasum -a 256 "$APP_MAIN" | cut -d ' ' -f 1)"
  "$CLI" selection-status --endpoint "$CONTROL_DIR/endpoint.json" \
    --expect-executable "$APP_MAIN" --expect-sha256 "$APP_SHA"

Starting or stopping recording, splitting, and transcription are not available
through this client. Use the app UI for those operations. Do not try another
control path for unsupported operations without explicit user authorization.

Exit codes
  0  Help or a successful operation response
  1  Local error
  2  Server rejection

Help is static and requires no connection. Operation responses are JSON on
stdout. Local errors go to stderr."#
    );
}

fn usage_error(message: &str) -> ! {
    eprintln!("capture-scribe-control: {message}");
    eprintln!("Run 'capture-scribe-control --help' for usage.");
    std::process::exit(1)
}

fn unsigned(value: &str, option: &str) -> Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit()) {
        return Err(format!("{option} must be an unsigned decimal integer"));
    }
    value
        .parse()
        .map_err(|_| format!("{option} is out of range"))
}
fn coordinate(value: &str, option: &str, positive: bool) -> Result<f64, String> {
    let number: f64 = value.parse().map_err(|_| format!("invalid {option}"))?;
    if !number.is_finite() || number < 0.0 || (positive && number == 0.0) {
        return Err(format!(
            "{option} must be finite and {}",
            if positive { "positive" } else { "nonnegative" }
        ));
    }
    Ok(number)
}
fn run(spec: &OperationSpec, mut args: impl Iterator<Item = String>) -> Result<bool, String> {
    let command = spec.name;
    let common_options = [
        "--endpoint",
        "--expect-executable",
        "--expect-sha256",
        "--instance-id",
    ];
    let mut options = HashMap::new();
    while let Some(key) = args.next() {
        if (!common_options.contains(&key.as_str())
            && !spec.arguments.iter().any(|(flag, _)| *flag == key))
            || options.contains_key(&key)
        {
            return Err("unknown, extra or duplicate option".into());
        }
        let value = args.next().ok_or("missing option value")?;
        if value.is_empty() || value.starts_with("--") {
            return Err("missing option value".into());
        }
        options.insert(key, value);
    }
    let value = |key: &str| {
        options
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| format!("missing {key}"))
    };
    if command == "capture-status" || command.starts_with("target-") {
        value("--instance-id")?;
    }
    let operation = match command {
        "capture-status" => Operation::CaptureStatus {},
        "selection-status" => Operation::SelectionStatus {},
        "cancel-selection" => Operation::CancelSelection {
            request_id: value("--request-id")?.into(),
        },
        "target-displays" => Operation::TargetDisplays {},
        "target-status" => Operation::TargetStatus {},
        "target-set" => Operation::TargetSet {
            request: SetTargetRequest {
                request_id: value("--request-id")?.into(),
                display_id: u32::try_from(unsigned(value("--display-id")?, "--display-id")?)
                    .map_err(|_| "--display-id is out of range")?,
                display_ref: value("--display-ref")?.into(),
                snapshot_token: value("--snapshot-token")?.into(),
                snapshot_revision: unsigned(value("--snapshot-revision")?, "--snapshot-revision")?,
                region: Region {
                    x: coordinate(value("--x")?, "--x", false)?,
                    y: coordinate(value("--y")?, "--y", false)?,
                    width: coordinate(value("--width")?, "--width", true)?,
                    height: coordinate(value("--height")?, "--height", true)?,
                    origin: "top-left".into(),
                    unit: "display-logical".into(),
                },
            },
        },
        "target-clear" => Operation::TargetClear {
            ticket: TargetTicket {
                target_id: value("--target-id")?.into(),
                revision: unsigned(value("--revision")?, "--revision")?,
            },
        },
        _ => unreachable!(),
    };
    let response = call(
        Path::new(value("--endpoint")?),
        Path::new(value("--expect-executable")?),
        value("--expect-sha256")?,
        options.get("--instance-id").map(String::as_str),
        operation,
    )
    .map_err(|e| e.to_string())?;
    println!(
        "{}",
        serde_json::to_string(&response).map_err(|e| e.to_string())?
    );
    Ok(response.ok)
}
fn main() {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        usage_error("expected a capture-status, selection, or target operation");
    };
    if command == "--help" && args.len() == 0 {
        print_help();
        return;
    }
    let Some(spec) = OPERATIONS.iter().find(|spec| spec.name == command) else {
        usage_error("unknown operation");
    };

    match run(spec, args) {
        Ok(true) => {}
        Ok(false) => std::process::exit(2),
        Err(e) => {
            eprintln!("capture-scribe-control: {e}");
            std::process::exit(1)
        }
    }
}
