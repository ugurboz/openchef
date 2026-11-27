//! Ported from `internal/daemon/daemon_test.go`. Unix only until the Windows socket
//! (R3.3); the lock test runs everywhere.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use openchef_daemon::{Error, Lock};

#[test]
fn lock_is_exclusive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.lock");
    let mut l = Lock::acquire(&path).unwrap();
    assert!(
        matches!(Lock::acquire(&path), Err(Error::Running)),
        "second acquire"
    );
    l.release().unwrap();
    l.release().unwrap(); // idempotent
    let mut again = Lock::acquire(&path).unwrap();
    again.release().unwrap();
}

#[cfg(unix)]
mod unix {
    use openchef_api::{
        CODE_NOT_INITIALIZED, CODE_VERSION_MISMATCH, Client, InitializeParams, METHOD_INITIALIZE,
        METHOD_STATUS,
    };
    use openchef_runtime::linerpc::{self, CODE_METHOD_NOT_FOUND};
    use serde_json::{Value, json};
    use std::os::unix::fs::PermissionsExt as _;

    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use openchef_config::Paths;
    use openchef_daemon::{Method, Options, run};
    use tokio_util::sync::CancellationToken;

    use super::{Error, Lock};

    /// A home whose socket path stays under the Unix limit (temp dirs on macOS are long).
    fn short_home() -> (Paths, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix("oc")
            .tempdir_in("/tmp")
            .unwrap();
        (Paths::in_home(&dir.path().join("h")).unwrap(), dir)
    }

    struct Running {
        paths: Paths,
        shutdown: CancellationToken,
        done: tokio::task::JoinHandle<Result<(), Error>>,
        _dir: Option<tempfile::TempDir>,
    }

    async fn start(
        methods: HashMap<String, Method>,
        on_stop: Option<openchef_daemon::OnStop>,
        home: Option<Paths>,
    ) -> Running {
        let (paths, dir) = if let Some(p) = home {
            (p, None)
        } else {
            let (p, d) = short_home();
            (p, Some(d))
        };
        let shutdown = CancellationToken::new();
        let options = Options {
            paths: paths.clone(),
            version: "test".into(),
            methods,
            on_stop,
            shutdown: shutdown.clone(),
        };
        let done = tokio::spawn(run(options));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match Client::dial(&paths.socket, "test").await {
                Ok(c) => {
                    c.close();
                    break;
                }
                Err(e) => {
                    assert!(!done.is_finished(), "daemon exited");
                    assert!(Instant::now() < deadline, "daemon not up: {e}");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
        Running {
            paths,
            shutdown,
            done,
            _dir: dir,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_and_single_instance() {
        let r = start(HashMap::new(), None, None).await;
        let c = Client::dial(&r.paths.socket, "test").await.unwrap();
        assert_eq!(
            (
                c.daemon.version.as_str(),
                c.daemon.daemon_version.as_str(),
                c.daemon.pid
            ),
            ("openchef.v1", "test", std::process::id())
        );
        let st = c.status().await.unwrap();
        assert_eq!(
            (st.socket.clone(), st.connections),
            (r.paths.socket.clone(), 1)
        );
        let second = Options {
            paths: r.paths.clone(),
            version: "x".into(),
            methods: HashMap::new(),
            on_stop: None,
            shutdown: CancellationToken::new(),
        };
        assert!(
            matches!(run(second).await, Err(Error::Running)),
            "second daemon"
        );
        // The refused second daemon must not have removed the first one's socket.
        c.status().await.unwrap();
        let mode = std::fs::metadata(&r.paths.socket)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "socket must be owner-only: {mode:o}");
        c.close();
        r.shutdown.cancel();
        r.done.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stop_removes_socket_and_lock() {
        let stopped = Arc::new(AtomicBool::new(false));
        let on_stop: openchef_daemon::OnStop = {
            let stopped = Arc::clone(&stopped);
            Box::new(move || {
                Box::pin(async move {
                    stopped.store(true, Ordering::SeqCst);
                    Ok(())
                })
            })
        };
        let r = start(HashMap::new(), Some(on_stop), None).await;
        let c = Client::dial(&r.paths.socket, "test").await.unwrap();
        c.stop().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), r.done)
            .await
            .expect("daemon did not stop")
            .unwrap()
            .unwrap();
        assert!(stopped.load(Ordering::SeqCst), "on_stop did not run");
        assert!(!r.paths.socket.exists(), "socket left behind");
        let mut l = Lock::acquire(&r.paths.lock).expect("lock still held");
        l.release().unwrap();
        assert!(matches!(
            Client::dial(&r.paths.socket, "test").await,
            Err(openchef_api::Error::NotRunning)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stale_socket_is_replaced() {
        let (paths, _dir) = short_home();
        std::fs::create_dir_all(&paths.home).unwrap();
        std::fs::write(&paths.socket, "left by a crashed daemon").unwrap();
        let r = start(HashMap::new(), None, Some(paths)).await;
        let c = Client::dial(&r.paths.socket, "test").await.unwrap();
        c.status().await.unwrap();
        c.close();
        r.shutdown.cancel();
        r.done.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn protocol_guards() {
        let echo: Method = Arc::new(|conn, params, _cancel| {
            Box::pin(async move {
                conn.notify("test/notified", params.clone())
                    .await
                    .map_err(|e| linerpc::RpcError::server(e.to_string()))?;
                Ok(params)
            })
        });
        let r = start(HashMap::from([("test/echo".to_owned(), echo)]), None, None).await;
        let mut c = Client::dial(&r.paths.socket, "test").await.unwrap();
        let out: Value = c.call("test/echo", json!({"n": 7})).await.unwrap();
        assert_eq!(out["n"], 7);
        let n = tokio::time::timeout(Duration::from_secs(5), c.notification())
            .await
            .expect("no notification")
            .unwrap();
        assert_eq!(n.method, "test/notified");
        match c.call::<Value>("nope", Value::Null).await {
            Err(openchef_api::Error::Rpc(linerpc::Error::Rpc { error, .. })) => {
                assert_eq!(error.code, CODE_METHOD_NOT_FOUND);
            }
            other => panic!("unknown method = {other:?}"),
        }
        // A raw peer that skips initialize, and one with the wrong version.
        let raw = openchef_api::transport::dial(&r.paths.socket, linerpc::Options::default())
            .await
            .unwrap();
        let none = CancellationToken::new();
        match raw.peer.call_value(METHOD_STATUS, Value::Null, &none).await {
            Err(linerpc::Error::Rpc { error, .. }) => assert_eq!(error.code, CODE_NOT_INITIALIZED),
            other => panic!("status before initialize = {other:?}"),
        }
        let old = InitializeParams {
            version: "openchef.v0".into(),
            client: "old".into(),
        };
        match raw.peer.call_value(METHOD_INITIALIZE, old, &none).await {
            Err(linerpc::Error::Rpc { error, .. }) => assert_eq!(error.code, CODE_VERSION_MISMATCH),
            other => panic!("old version = {other:?}"),
        }
        raw.shutdown();
        c.close();
        r.shutdown.cancel();
        r.done.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dial_without_daemon() {
        let (paths, _dir) = short_home();
        assert!(matches!(
            Client::dial(&paths.socket, "test").await,
            Err(openchef_api::Error::NotRunning)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn socket_path_too_long() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(
            "a-directory-name-long-enough-to-push-the-socket-path-past-the-unix-limit-for-sure",
        );
        let options = Options {
            paths: Paths::in_home(&home).unwrap(),
            version: "x".into(),
            methods: HashMap::new(),
            on_stop: None,
            shutdown: CancellationToken::new(),
        };
        assert!(matches!(
            run(options).await,
            Err(Error::SocketPathTooLong { .. })
        ));
    }

    /// A detached daemon outlives its starter and stops on request.
    #[tokio::test(flavor = "multi_thread")]
    async fn start_detached() {
        let (paths, _dir) = short_home();
        let d = openchef_daemon::Detached {
            exe: env!("CARGO_BIN_EXE_daemon-test-helper").into(),
            args: vec![paths.home.clone().into_os_string()],
            env: None,
            log_path: paths.logs.join("daemon.log"),
            socket: paths.socket.clone(),
            timeout: Duration::from_secs(10),
        };
        let info = openchef_daemon::start_detached(&d).await.unwrap();
        assert!(
            info.pid != std::process::id() && info.pid > 0,
            "pid {}",
            info.pid
        );
        let c = Client::dial(&paths.socket, "test").await.unwrap();
        c.stop().await.unwrap();
        let closed = c.closed();
        tokio::time::timeout(Duration::from_secs(15), closed.cancelled())
            .await
            .expect("detached daemon did not stop");
        c.close();
    }
}
