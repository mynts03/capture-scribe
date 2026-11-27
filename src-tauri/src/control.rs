//! Explicit, user-private, capability-authenticated loopback control. No GUI startup here.
use crate::{
    capture::CaptureManager,
    capture_target::{SetTargetRequest, TargetControl, TargetTicket},
    control_permissions as permissions,
    selection_session::SelectionManager,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
const LIMIT: u64 = 8192;
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Endpoint {
    pub protocol_version: u8,
    pub address: SocketAddr,
    pub instance_id: String,
    pub app_pid: u32,
    pub app_executable: PathBuf,
    pub executable_sha256: String,
    // Never print this descriptor, log the token, or add it to review ZIPs.
    token: String,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    CaptureStatus {},
    SelectionStatus {},
    CancelSelection {
        #[serde(rename = "requestId")]
        request_id: String,
    },
    TargetDisplays {},
    TargetSet {
        request: SetTargetRequest,
    },
    TargetStatus {},
    TargetClear {
        ticket: TargetTicket,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    protocol_version: u8,
    instance_id: String,
    app_executable: PathBuf,
    executable_sha256: String,
    token: String,
    command: Operation,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Response {
    pub ok: bool,
    pub instance_id: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}
pub struct ControlServer {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    directory: PathBuf,
    endpoint: Endpoint,
    manager: SelectionManager,
    target: TargetControl,
}
fn nonce() -> io::Result<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|_| io::Error::other("OS random unavailable"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}
pub fn executable_sha256(path: &Path) -> io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut b = [0; 65536];
    loop {
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
fn read_json<T: serde::de::DeserializeOwned>(stream: &mut TcpStream) -> io::Result<T> {
    let mut b = Vec::new();
    BufReader::new(stream.take(LIMIT + 1)).read_until(b'\n', &mut b)?;
    if b.len() as u64 > LIMIT || b.last() != Some(&b'\n') {
        return Err(io::Error::other("invalid control frame"));
    }
    serde_json::from_slice(&b).map_err(|_| io::Error::other("invalid control JSON"))
}
fn send_json<T: Serialize>(stream: &mut TcpStream, value: &T) -> io::Result<()> {
    let b = serde_json::to_vec(value)?;
    if b.len() as u64 >= LIMIT {
        return Err(io::Error::other("control response too large"));
    }
    stream.write_all(&b)?;
    stream.write_all(b"\n")
}
fn token_matches(a: &str, b: &str) -> bool {
    if a.len() != 64 || b.len() != 64 {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |v, (a, b)| v | (a ^ b)) == 0
}
fn dispatch(
    e: &Endpoint,
    m: &SelectionManager,
    target: &TargetControl,
    capture: &CaptureManager,
    r: Request,
) -> Response {
    let result = if r.protocol_version != 1
        || r.instance_id != e.instance_id
        || r.app_executable != e.app_executable
        || r.executable_sha256 != e.executable_sha256
        || !token_matches(&r.token, &e.token)
    {
        Err("control_identity_or_authentication_failed".into())
    } else {
        match r.command {
            Operation::CaptureStatus {} => capture
                .control_status()
                .map_err(str::to_owned)
                .and_then(|status| {
                    serde_json::to_value(status).map_err(|_| "capture_status_unavailable".into())
                }),
            Operation::SelectionStatus {} => Ok(serde_json::to_value(m.snapshot()).unwrap()),
            Operation::CancelSelection { request_id } => m
                .cancel(&request_id)
                .map(|v| serde_json::to_value(v).unwrap()),
            Operation::TargetDisplays {} => {
                target.displays().map(|v| serde_json::to_value(v).unwrap())
            }
            Operation::TargetSet { request } => target
                .set(request)
                .map(|v| serde_json::to_value(v).unwrap()),
            Operation::TargetStatus {} => Ok(serde_json::to_value(target.snapshot()).unwrap()),
            Operation::TargetClear { ticket } => target
                .clear(ticket)
                .map(|v| serde_json::to_value(v).unwrap()),
        }
    };
    match result {
        Ok(v) => Response {
            ok: true,
            instance_id: e.instance_id.clone(),
            result: Some(v),
            error: None,
        },
        Err(e2) => Response {
            ok: false,
            instance_id: e.instance_id.clone(),
            result: None,
            error: Some(e2),
        },
    }
}
impl ControlServer {
    /// Directory must be a fresh, explicit absolute path. No reuse or auto-discovery.
    pub fn start(
        directory: &Path,
        executable: &Path,
        manager: SelectionManager,
        target: TargetControl,
        capture: CaptureManager,
    ) -> io::Result<Self> {
        if !directory.is_absolute() {
            return Err(io::Error::other("control directory must be absolute"));
        }
        let executable = executable.canonicalize()?;
        let hash = executable_sha256(&executable)?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let e = Endpoint {
            protocol_version: 1,
            address: listener.local_addr()?,
            instance_id: nonce()?,
            app_pid: std::process::id(),
            app_executable: executable,
            executable_sha256: hash,
            token: nonce()?,
        };
        permissions::create_private_dir(directory)?;
        let mut descriptor_created = false;
        let setup = (|| {
            permissions::check_private(directory, true)?;
            let mut f = permissions::private_file(&directory.join("endpoint.json"))?;
            descriptor_created = true;
            serde_json::to_writer(&mut f, &e)?;
            f.sync_all()?;
            Ok::<_, io::Error>(())
        })();
        if let Err(error) = setup {
            if descriptor_created {
                let _ = fs::remove_file(directory.join("endpoint.json"));
            }
            let _ = fs::remove_dir(directory);
            return Err(error);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let endpoint = e.clone();
        let state = manager.clone();
        let target_state = target.clone();
        let handle = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, peer)) => {
                        if !peer.ip().is_loopback() {
                            continue;
                        }
                        // macOS accepted streams can inherit the listener's
                        // nonblocking mode. A blocking stream needs the existing
                        // read/write deadlines before waiting for a request.
                        let timeout = Some(Duration::from_millis(1000));
                        if stream
                            .set_nonblocking(false)
                            .and_then(|()| stream.set_read_timeout(timeout))
                            .and_then(|()| stream.set_write_timeout(timeout))
                            .is_err()
                        {
                            continue;
                        }
                        if let Ok(request) = read_json(&mut stream) {
                            if !stopped.load(Ordering::Acquire) {
                                let _ = send_json(
                                    &mut stream,
                                    &dispatch(&endpoint, &state, &target_state, &capture, request),
                                );
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(25))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            stop,
            thread: Some(handle),
            directory: directory.into(),
            endpoint: e,
            manager,
            target,
        })
    }
    pub fn shutdown(&mut self) {
        self.manager.shutdown();
        self.target.shutdown();
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // Remove only this instance's own descriptor. Never recurse into user files.
        if let Ok(e) = read_endpoint(&self.directory.join("endpoint.json")) {
            if e.instance_id == self.endpoint.instance_id
                && token_matches(&e.token, &self.endpoint.token)
            {
                let _ = fs::remove_file(self.directory.join("endpoint.json"));
                let _ = fs::remove_dir(&self.directory);
            }
        }
    }
}
impl Drop for ControlServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}
pub fn read_endpoint(path: &Path) -> io::Result<Endpoint> {
    permissions::check_private(
        path.parent()
            .ok_or_else(|| io::Error::other("endpoint parent missing"))?,
        true,
    )?;
    permissions::check_private(path, false)?;
    let mut b = Vec::new();
    fs::File::open(path)?.take(LIMIT + 1).read_to_end(&mut b)?;
    if b.len() as u64 > LIMIT {
        return Err(io::Error::other("endpoint too large"));
    }
    let e: Endpoint =
        serde_json::from_slice(&b).map_err(|_| io::Error::other("invalid endpoint"))?;
    if e.protocol_version != 1
        || e.address.ip() != std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        || e.instance_id.len() != 64
        || e.token.len() != 64
    {
        return Err(io::Error::other("invalid local endpoint identity"));
    }
    Ok(e)
}
pub fn call(
    path: &Path,
    expected_executable: &Path,
    expected_hash: &str,
    expected_instance: Option<&str>,
    operation: Operation,
) -> io::Result<Response> {
    let capture_status = matches!(operation, Operation::CaptureStatus {});
    let context = |code: &str, error: io::Error| {
        if capture_status {
            io::Error::new(error.kind(), format!("{code}: {error}"))
        } else {
            error
        }
    };
    let e = read_endpoint(path).map_err(|error| {
        let code = if error.kind() == io::ErrorKind::NotFound {
            "control_endpoint_missing"
        } else {
            "control_endpoint_unavailable"
        };
        context(code, error)
    })?;
    let executable = expected_executable
        .canonicalize()
        .map_err(|error| context("control_identity_mismatch", error))?;
    if e.app_executable != executable
        || e.executable_sha256 != expected_hash
        || expected_instance.is_some_and(|id| id != e.instance_id)
    {
        return Err(context(
            "control_identity_mismatch",
            io::Error::other("endpoint does not match expected app/instance"),
        ));
    }
    if capture_status && expected_instance.is_none() {
        return Err(io::Error::other(
            "capture-status requires --instance-id from selection-status",
        ));
    }
    if matches!(operation, Operation::CancelSelection { .. }) && expected_instance.is_none() {
        return Err(io::Error::other(
            "cancel requires --instance-id from selection-status",
        ));
    }
    if matches!(
        operation,
        Operation::TargetDisplays {}
            | Operation::TargetSet { .. }
            | Operation::TargetStatus {}
            | Operation::TargetClear { .. }
    ) && expected_instance.is_none()
    {
        return Err(io::Error::other(
            "target operations require --instance-id from selection-status",
        ));
    }
    let mut stream = TcpStream::connect_timeout(&e.address, Duration::from_secs(2))
        .map_err(|error| context("control_connection_failed", error))?;
    let communication_error = |error| context("control_communication_failed", error);
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(communication_error)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(communication_error)?;
    let r = Request {
        protocol_version: 1,
        instance_id: e.instance_id.clone(),
        app_executable: e.app_executable.clone(),
        executable_sha256: e.executable_sha256.clone(),
        token: e.token,
        command: operation,
    };
    send_json(&mut stream, &r).map_err(communication_error)?;
    let response: Response = read_json(&mut stream).map_err(communication_error)?;
    if response.instance_id != e.instance_id {
        return Err(context(
            "control_identity_mismatch",
            io::Error::other("response instance mismatch"),
        ));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        server: ControlServer,
        manager: SelectionManager,
        target: TargetControl,
        capture: CaptureManager,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("cs-control-test-{}", nonce().unwrap()));
            fs::create_dir(&root).unwrap();
            let manager = SelectionManager::default();
            let target = TargetControl::default();
            let capture = CaptureManager::default();
            let server = ControlServer::start(
                &root.join("private"),
                &std::env::current_exe().unwrap(),
                manager.clone(),
                target.clone(),
                capture.clone(),
            )
            .unwrap();
            Self {
                root,
                server,
                manager,
                target,
                capture,
            }
        }
        fn endpoint(&self) -> PathBuf {
            self.root.join("private/endpoint.json")
        }
        fn call(&self, op: Operation) -> Response {
            let e = &self.server.endpoint;
            call(
                &self.endpoint(),
                &e.app_executable,
                &e.executable_sha256,
                Some(&e.instance_id),
                op,
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.shutdown();
            fs::remove_dir(&self.root).unwrap();
        }
    }
    #[test]
    fn authenticated_status_cancel_and_completion_use_same_session() {
        let f = Fixture::new();
        let mut session = f.manager.begin("gui-1").unwrap();
        session.started(99);
        assert_eq!(
            f.call(Operation::SelectionStatus {}).result.unwrap()["requestId"],
            "gui-1"
        );
        let response = f.call(Operation::CancelSelection {
            request_id: "gui-1".into(),
        });
        assert!(response.ok);
        assert_eq!(response.result.unwrap()["completed"], false);
        assert!(session.needs_cancel());
        session.cancel_sent();
        assert!(session.helper_finished());
        session.finish("cancelled", true);
        assert_eq!(
            f.call(Operation::SelectionStatus {}).result.unwrap()["phase"],
            "finished"
        );
        assert!(
            !f.call(Operation::CancelSelection {
                request_id: "old".into()
            })
            .ok
        );
    }
    #[test]
    fn old_instance_wrong_app_hash_token_and_protocol_are_rejected() {
        let f = Fixture::new();
        let e = &f.server.endpoint;
        for which in 0..5 {
            let mut r = Request {
                protocol_version: 1,
                instance_id: e.instance_id.clone(),
                app_executable: e.app_executable.clone(),
                executable_sha256: e.executable_sha256.clone(),
                token: e.token.clone(),
                command: Operation::SelectionStatus {},
            };
            match which {
                0 => r.instance_id = "old".into(),
                1 => r.app_executable = "/another/app".into(),
                2 => r.executable_sha256 = "different".into(),
                3 => r.token = "0".repeat(64),
                _ => r.protocol_version = 2,
            }
            let mut s = TcpStream::connect(e.address).unwrap();
            send_json(&mut s, &r).unwrap();
            let response: Response = read_json(&mut s).unwrap();
            assert!(!response.ok);
        }
        assert!(call(
            &f.endpoint(),
            &e.app_executable,
            "wrong",
            None,
            Operation::SelectionStatus {}
        )
        .is_err());
        assert!(call(
            &f.endpoint(),
            &e.app_executable,
            &e.executable_sha256,
            None,
            Operation::CancelSelection {
                request_id: "a".into()
            }
        )
        .is_err());
    }
    #[test]
    fn server_shutdown_removes_only_own_descriptor_and_stops_connections() {
        let mut f = Fixture::new();
        let e = f.server.endpoint.clone();
        let mut session = f.manager.begin("a").unwrap();
        f.server.shutdown();
        assert!(session.needs_cancel());
        assert!(!f.endpoint().exists());
        assert!(TcpStream::connect(e.address).is_err());
        assert!(f.manager.begin("b").is_err());
        assert!(session.helper_finished());
        session.finish("cancelled", true);
    }
    #[test]
    fn existing_directory_is_never_reused_or_modified() {
        let f = Fixture::new();
        let bytes = fs::read(f.endpoint()).unwrap();
        assert!(ControlServer::start(
            f.endpoint().parent().unwrap(),
            &std::env::current_exe().unwrap(),
            SelectionManager::default(),
            TargetControl::default(),
            CaptureManager::default(),
        )
        .is_err());
        assert_eq!(bytes, fs::read(f.endpoint()).unwrap());
    }
    #[test]
    fn another_instance_and_missing_connection_do_not_cancel() {
        let f = Fixture::new();
        let mut s = f.manager.begin("a").unwrap();
        let g = Fixture::new();
        let e = &f.server.endpoint;
        assert!(call(
            &g.endpoint(),
            &e.app_executable,
            &e.executable_sha256,
            Some(&e.instance_id),
            Operation::CancelSelection {
                request_id: "a".into()
            }
        )
        .is_err());
        assert!(!s.needs_cancel());
        s.helper_finished();
        s.finish("confirmed", true);
        assert!(call(
            &f.root.join("missing.json"),
            &e.app_executable,
            &e.executable_sha256,
            None,
            Operation::SelectionStatus {}
        )
        .is_err());
    }
    #[cfg(unix)]
    #[test]
    fn public_or_symlink_endpoint_is_rejected() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let f = Fixture::new();
        fs::set_permissions(f.endpoint(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_endpoint(&f.endpoint()).is_err());
        fs::set_permissions(f.endpoint(), fs::Permissions::from_mode(0o600)).unwrap();
        let link = f.root.join("link");
        symlink(f.endpoint().parent().unwrap(), &link).unwrap();
        assert!(read_endpoint(&link.join("endpoint.json")).is_err());
        fs::remove_file(link).unwrap();
    }
    #[test]
    fn capture_status_accepts_a_request_sent_after_connecting() {
        let f = Fixture::new();
        let endpoint = &f.server.endpoint;
        let mut stream = TcpStream::connect(endpoint.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        // Leave the request unsent across several accept-loop polls. The
        // server must keep the connection open instead of sending early EOF.
        let error = stream.read(&mut [0]).unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        send_json(
            &mut stream,
            &Request {
                protocol_version: endpoint.protocol_version,
                instance_id: endpoint.instance_id.clone(),
                app_executable: endpoint.app_executable.clone(),
                executable_sha256: endpoint.executable_sha256.clone(),
                token: endpoint.token.clone(),
                command: Operation::CaptureStatus {},
            },
        )
        .unwrap();
        let response: Response = read_json(&mut stream).unwrap();
        assert!(response.ok);
        assert_eq!(response.instance_id, endpoint.instance_id);
        assert_eq!(response.result.unwrap()["state"], "idle");
    }

    #[test]
    fn capture_status_served_after_stalled_peer_read_times_out() {
        let f = Fixture::new();
        let start = std::time::Instant::now();
        let mut stalled = TcpStream::connect(f.server.endpoint.address).unwrap();
        stalled
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stalled.write_all(b"{").unwrap();
        let mut response = Vec::new();
        stalled.read_to_end(&mut response).unwrap();
        assert!(response.is_empty());
        assert!(
            start.elapsed() >= Duration::from_millis(750),
            "server closed the stalled connection before the read timeout"
        );
        assert_eq!(
            f.call(Operation::CaptureStatus {}).result.unwrap()["state"],
            "idle"
        );
    }

    #[test]
    fn oversized_and_unknown_requests_do_not_change_state() {
        let f = Fixture::new();
        let e = &f.server.endpoint;
        let mut s = TcpStream::connect(e.address).unwrap();
        s.write_all(&vec![b'a'; LIMIT as usize + 1]).unwrap();
        let _ = s.shutdown(std::net::Shutdown::Write);
        let mut b = Vec::new();
        s.read_to_end(&mut b).unwrap();
        assert!(b.is_empty());
        assert_eq!(f.manager.snapshot().phase, "idle");
        assert!(serde_json::from_value::<Operation>(
            serde_json::json!({"operation":"start_recording"})
        )
        .is_err());
    }

    fn target_request() -> SetTargetRequest {
        SetTargetRequest {
            request_id: "ipc-target-1".into(),
            display_id: 1,
            display_ref: "test-display".into(),
            snapshot_token: "test-snapshot".into(),
            snapshot_revision: 1,
            region: crate::capture_target::Region {
                x: 20.0,
                y: 30.0,
                width: 320.0,
                height: 240.0,
                origin: "top-left".into(),
                unit: "display-logical".into(),
            },
        }
    }
    #[test]
    fn target_operations_require_explicit_instance_and_preserve_authentication() {
        let f = Fixture::new();
        let e = &f.server.endpoint;
        let operations = [
            Operation::TargetDisplays {},
            Operation::TargetStatus {},
            Operation::TargetSet {
                request: target_request(),
            },
            Operation::TargetClear {
                ticket: TargetTicket {
                    target_id: "old".into(),
                    revision: 1,
                },
            },
        ];
        for operation in operations {
            assert!(call(
                &f.endpoint(),
                &e.app_executable,
                &e.executable_sha256,
                None,
                operation
            )
            .unwrap_err()
            .to_string()
            .contains("--instance-id"));
        }
        for operation in [
            Operation::CaptureStatus {},
            Operation::TargetDisplays {},
            Operation::TargetStatus {},
            Operation::TargetSet {
                request: target_request(),
            },
            Operation::TargetClear {
                ticket: TargetTicket {
                    target_id: "old".into(),
                    revision: 1,
                },
            },
        ] {
            let response = dispatch(
                e,
                &f.manager,
                &f.target,
                &f.capture,
                Request {
                    protocol_version: 1,
                    instance_id: e.instance_id.clone(),
                    app_executable: e.app_executable.clone(),
                    executable_sha256: e.executable_sha256.clone(),
                    token: "0".repeat(64),
                    command: operation,
                },
            );
            assert!(!response.ok);
            assert_eq!(
                response.error.as_deref(),
                Some("control_identity_or_authentication_failed")
            );
        }
        assert_eq!(f.manager.snapshot().phase, "idle");
        assert!(f.call(Operation::TargetStatus {}).ok);
        assert!(
            !f.call(Operation::TargetDisplays {}).ok,
            "unobserved display cache must be an explicit failure"
        );
    }
    #[test]
    fn target_operations_reject_unknown_protocol_fields() {
        for command in [
            serde_json::json!({"operation":"selection_status","unexpected":true}),
            serde_json::json!({"operation":"target_status","startRecording":true}),
            serde_json::json!({"operation":"capture_status","refresh":true}),
            serde_json::json!({"operation":"target_displays","refresh":true}),
            serde_json::json!({"operation":"target_clear","ticket":{"targetId":"a","revision":1,"force":true}}),
            serde_json::json!({"operation":"target_set","request":{
                "requestId":"a","displayId":1,"displayRef":"d","snapshotToken":"s","snapshotRevision":1,
                "region":{"x":0,"y":0,"width":16,"height":16,"origin":"top-left","unit":"display-logical"},
                "persist":true
            }}),
            serde_json::json!({"operation":"target_set","request":{
                "requestId":"a","displayId":1,"displayRef":"d","snapshotToken":"s","snapshotRevision":1,
                "region":{"x":0,"y":0,"width":16,"height":16,"origin":"top-left","unit":"display-logical","roundPixels":true}
            }}),
        ] {
            assert!(serde_json::from_value::<Operation>(command).is_err());
        }
    }
    #[test]
    fn no_argument_operations_keep_the_original_wire_shape() {
        for (operation, name) in [
            (Operation::CaptureStatus {}, "capture_status"),
            (Operation::SelectionStatus {}, "selection_status"),
            (Operation::TargetDisplays {}, "target_displays"),
            (Operation::TargetStatus {}, "target_status"),
        ] {
            let json = serde_json::to_value(operation).unwrap();
            assert_eq!(json, serde_json::json!({"operation": name}));
            assert!(serde_json::from_value::<Operation>(json).is_ok());
        }
    }
    #[test]
    fn unsupported_operation_on_legacy_server_is_a_failure_without_fallback() {
        #[allow(dead_code)]
        #[derive(Deserialize)]
        #[serde(tag = "operation", rename_all = "snake_case")]
        enum LegacyOperation {
            SelectionStatus,
            CancelSelection {
                #[serde(rename = "requestId")]
                request_id: String,
            },
        }
        let f = Fixture::new();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut e = f.server.endpoint.clone();
        e.address = listener.local_addr().unwrap();
        let path = f.endpoint().parent().unwrap().join("legacy.json");
        let mut file = permissions::private_file(&path).unwrap();
        serde_json::to_writer(&mut file, &e).unwrap();
        drop(file);
        let legacy = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let request: serde_json::Value = read_json(&mut stream).unwrap();
            assert_eq!(request["command"]["operation"], "target_status");
            assert!(serde_json::from_value::<LegacyOperation>(request["command"].clone()).is_err());
            // Existing unsupported servers close invalid-JSON requests, with no operation executed.
        });
        assert!(call(
            &path,
            &e.app_executable,
            &e.executable_sha256,
            Some(&e.instance_id),
            Operation::TargetStatus {}
        )
        .is_err());
        legacy.join().unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(f.manager.snapshot().phase, "idle");
    }
}
