//! Ported from `internal/runtime/linerpc/linerpc_test.go` (Go reference).
//!
//! The "remote" side is a pipe the test writes lines into, and a sink that captures
//! what the peer sends.

// Test helpers outside `#[test]` functions may unwrap too (AGENTS.md §5).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::io::{self, Write as _};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openchef_runtime::BoxFuture;
use openchef_runtime::linerpc::{Error, Handler, LineSink, Notifications, Options, Peer, RpcError};
use openchef_runtime::rawlog::{Dir, RawLog};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Captures sent lines.
struct Capture(mpsc::UnboundedSender<Value>);

impl LineSink for Capture {
    fn send_line(&self, line: Vec<u8>) -> BoxFuture<'static, io::Result<()>> {
        let v: Value = serde_json::from_slice(&line).unwrap();
        drop(self.0.send(v));
        Box::pin(async { Ok(()) })
    }
}

struct Remote {
    to_peer: Option<io::PipeWriter>,
    from_peer: mpsc::UnboundedReceiver<Value>,
}

impl Remote {
    /// Writes one line in a single write: a pipe write below `PIPE_BUF` is atomic, so a
    /// peer that stops at an oversize line cannot break the pipe halfway through it.
    fn say(&mut self, line: &str) {
        let w = self.to_peer.as_mut().unwrap();
        w.write_all(format!("{line}\n").as_bytes()).unwrap();
    }

    async fn heard(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(5), self.from_peer.recv())
            .await
            .expect("nothing sent")
            .unwrap()
    }

    fn hang_up(&mut self) {
        drop(self.to_peer.take());
    }
}

fn start(options: Options) -> (Peer, Notifications, Remote) {
    let (reader, writer) = io::pipe().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let (peer, notes) = Peer::new(reader, Arc::new(Capture(tx)), options);
    (
        peer,
        notes,
        Remote {
            to_peer: Some(writer),
            from_peer: rx,
        },
    )
}

fn never() -> CancellationToken {
    CancellationToken::new()
}

#[tokio::test(flavor = "multi_thread")]
async fn call_and_result() {
    let (peer, _notes, mut remote) = start(Options::default());
    let call = tokio::spawn({
        let peer = peer.clone();
        async move {
            peer.call::<Value>("thread/start", json!({"cwd": "/w"}), &never())
                .await
        }
    });
    let req = remote.heard().await;
    assert_eq!(
        (req["jsonrpc"].as_str(), req["method"].as_str()),
        (Some("2.0"), Some("thread/start"))
    );
    assert_eq!(req["params"]["cwd"], "/w");
    remote.say(
        &json!({"jsonrpc": "2.0", "id": req["id"], "result": {"thread": {"id": "t1"}}}).to_string(),
    );
    assert_eq!(call.await.unwrap().unwrap()["thread"]["id"], "t1");
}

#[tokio::test(flavor = "multi_thread")]
async fn call_error_response() {
    let (peer, _notes, mut remote) = start(Options::default());
    let call = tokio::spawn({
        let peer = peer.clone();
        async move { peer.call::<Value>("x", Value::Null, &never()).await }
    });
    let req = remote.heard().await;
    assert!(req.get("params").is_none(), "null params are omitted");
    remote.say(
        &json!({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32000, "message": "nope"}})
            .to_string(),
    );
    match call.await.unwrap() {
        Err(Error::Rpc { error, .. }) => {
            assert_eq!((error.code, error.message.as_str()), (-32000, "nope"));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn id_formatting() {
    let (peer, _notes, mut remote) = start(Options::default());
    for answer_id in [json!(1.0), json!(2)] {
        let call = tokio::spawn({
            let peer = peer.clone();
            async move { peer.call::<Value>("m", Value::Null, &never()).await }
        });
        let _req = remote.heard().await;
        remote.say(&json!({"jsonrpc": "2.0", "id": answer_id, "result": "ok"}).to_string());
        assert_eq!(call.await.unwrap().unwrap(), "ok", "id {answer_id}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn server_requests() {
    let handler: Handler = Arc::new(|method, params, _cancel| {
        Box::pin(async move {
            match method.as_str() {
                "approve" => Ok(
                    json!({"decision": if params["cmd"] == "ls" { "accept" } else { "decline" }}),
                ),
                _ => Err(RpcError {
                    code: -32001,
                    message: "custom".into(),
                    data: None,
                }),
            }
        })
    });
    let (_peer, _notes, mut remote) = start(Options {
        handler: Some(handler),
        ..Options::default()
    });
    remote.say(r#"{"jsonrpc":"2.0","id":"a","method":"approve","params":{"cmd":"ls"}}"#);
    let reply = remote.heard().await;
    assert_eq!(
        (reply["id"].as_str(), reply["result"]["decision"].as_str()),
        (Some("a"), Some("accept"))
    );
    remote.say(r#"{"jsonrpc":"2.0","id":9,"method":"other"}"#);
    let reply = remote.heard().await;
    assert_eq!(
        (reply["id"].as_i64(), reply["error"]["code"].as_i64()),
        (Some(9), Some(-32001))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_handler() {
    let (_peer, _notes, mut remote) = start(Options::default());
    remote.say(r#"{"jsonrpc":"2.0","id":1,"method":"anything"}"#);
    assert_eq!(remote.heard().await["error"]["code"], -32601);
}

/// Notifications sent before the consumer reads any.
const N: usize = 2000;

#[tokio::test(flavor = "multi_thread")]
async fn notifications_are_ordered_and_never_dropped() {
    let (_peer, mut notes, mut remote) = start(Options::default());
    for i in 0..N {
        remote.say(&json!({"jsonrpc": "2.0", "method": "n", "params": {"i": i}}).to_string());
    }
    remote.hang_up();
    // The consumer was not reading while they arrived; nothing may be lost.
    tokio::time::sleep(Duration::from_millis(50)).await;
    for i in 0..N {
        let n = notes.recv().await.expect("notification lost");
        assert_eq!(n.params["i"], i);
    }
    assert!(notes.recv().await.is_none(), "stream ends after close");
}

#[tokio::test(flavor = "multi_thread")]
async fn close_fails_pending_and_future_calls() {
    let (peer, _notes, mut remote) = start(Options::default());
    let call = tokio::spawn({
        let peer = peer.clone();
        async move { peer.call::<Value>("pending", Value::Null, &never()).await }
    });
    let _req = remote.heard().await;
    remote.hang_up();
    assert!(matches!(call.await.unwrap(), Err(Error::Closed { .. })));
    peer.closed().cancelled().await;
    assert!(matches!(
        peer.call::<Value>("after", Value::Null, &never()).await,
        Err(Error::Closed { .. })
    ));
    assert_eq!(peer.stop_error(), None, "clean EOF");
}

#[tokio::test(flavor = "multi_thread")]
async fn call_cancel_and_late_response() {
    let reported = Arc::new(Mutex::new(Vec::<String>::new()));
    let on_error = {
        let reported = Arc::clone(&reported);
        Arc::new(move |_line: &[u8], reason: &str| reported.lock().unwrap().push(reason.to_owned()))
    };
    let (peer, _notes, mut remote) = start(Options {
        on_protocol_error: Some(on_error),
        ..Options::default()
    });
    let cancel = CancellationToken::new();
    let call = tokio::spawn({
        let (peer, cancel) = (peer.clone(), cancel.clone());
        async move { peer.call::<Value>("slow", Value::Null, &cancel).await }
    });
    let req = remote.heard().await;
    cancel.cancel();
    assert!(matches!(call.await.unwrap(), Err(Error::Cancelled { .. })));
    remote.say(&json!({"jsonrpc": "2.0", "id": req["id"], "result": 1}).to_string());
    // The peer keeps working after the late response.
    let next = tokio::spawn({
        let peer = peer.clone();
        async move { peer.call::<Value>("next", Value::Null, &never()).await }
    });
    let req = remote.heard().await;
    remote.say(&json!({"jsonrpc": "2.0", "id": req["id"], "result": 2}).to_string());
    assert_eq!(next.await.unwrap().unwrap(), 2);
    assert!(
        reported
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.contains("unknown id")),
        "{:?}",
        reported.lock().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_lines_and_duplicate_responses() {
    let reported = Arc::new(Mutex::new(Vec::<String>::new()));
    let on_error = {
        let reported = Arc::clone(&reported);
        Arc::new(move |_line: &[u8], reason: &str| reported.lock().unwrap().push(reason.to_owned()))
    };
    let (peer, mut notes, mut remote) = start(Options {
        on_protocol_error: Some(on_error),
        ..Options::default()
    });
    let call = tokio::spawn({
        let peer = peer.clone();
        async move { peer.call::<Value>("m", Value::Null, &never()).await }
    });
    let req = remote.heard().await;
    remote.say("not json");
    remote.say(r#"{"jsonrpc":"2.0"}"#);
    let response = json!({"jsonrpc": "2.0", "id": req["id"], "result": "first"}).to_string();
    remote.say(&response);
    remote.say(&response);
    remote.say(r#"{"jsonrpc":"2.0","method":"still-alive"}"#);
    assert_eq!(call.await.unwrap().unwrap(), "first");
    assert_eq!(notes.recv().await.unwrap().method, "still-alive");
    let got = reported.lock().unwrap().clone();
    assert_eq!(got.len(), 3, "{got:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn oversize_message_stops_peer() {
    let (peer, _notes, mut remote) = start(Options {
        max_line: Some(64),
        ..Options::default()
    });
    remote.say(&format!(
        r#"{{"jsonrpc":"2.0","method":"big","params":"{}"}}"#,
        "x".repeat(200)
    ));
    tokio::time::timeout(Duration::from_secs(5), peer.closed().cancelled())
        .await
        .unwrap();
    assert!(peer.stop_error().unwrap().contains("exceeds"));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_calls() {
    let (peer, _notes, mut remote) = start(Options::default());
    let mut calls = Vec::new();
    for i in 0..50 {
        let peer = peer.clone();
        calls.push(tokio::spawn(async move {
            (
                i,
                peer.call::<Value>("echo", json!({"i": i}), &never()).await,
            )
        }));
    }
    let mut requests = Vec::new();
    for _ in 0..50 {
        requests.push(remote.heard().await);
    }
    // Answer in reverse order: matching is by id, not by order.
    for req in requests.iter().rev() {
        remote.say(
            &json!({"jsonrpc": "2.0", "id": req["id"], "result": req["params"]["i"]}).to_string(),
        );
    }
    for c in calls {
        let (i, result) = c.await.unwrap();
        assert_eq!(result.unwrap(), i);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn traffic_is_logged() {
    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl io::Write for Shared {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let buf = Arc::new(Mutex::new(Vec::new()));
    let log = RawLog::to_writer(Box::new(Shared(Arc::clone(&buf))));
    let (peer, mut notes, mut remote) = start(Options {
        log: Some(log),
        ..Options::default()
    });
    peer.notify("hello", json!({"a": 1})).await.unwrap();
    let _sent = remote.heard().await;
    remote.say(r#"{"jsonrpc":"2.0","method":"hi"}"#);
    let _n = notes.recv().await.unwrap();
    let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    let records: Vec<openchef_runtime::rawlog::Record> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        records.iter().map(|r| r.dir).collect::<Vec<_>>(),
        [Dir::Send, Dir::Recv]
    );
    assert_eq!(records[0].msg.as_ref().unwrap()["method"], "hello");
}

#[tokio::test(flavor = "multi_thread")]
async fn barrier_orders_notifications_before_response() {
    let (peer, mut notes, mut remote) = start(Options::default());
    remote.say(r#"{"jsonrpc":"2.0","method":"update","params":1}"#);
    remote.say(r#"{"jsonrpc":"2.0","method":"update","params":2}"#);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let barrier = tokio::spawn({
        let peer = peer.clone();
        async move { peer.barrier(&CancellationToken::new()).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!barrier.is_finished(), "barrier must wait for the consumer");
    assert_eq!(notes.recv().await.unwrap().params, 1);
    assert_eq!(notes.recv().await.unwrap().params, 2);
    // The marker is released by the consumer's next receive.
    let consumer = tokio::spawn(async move { notes.recv().await });
    barrier.await.unwrap().unwrap();
    remote.hang_up();
    assert!(consumer.await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn barrier_after_close() {
    let (peer, mut notes, mut remote) = start(Options::default());
    remote.say(r#"{"jsonrpc":"2.0","method":"last"}"#);
    remote.hang_up();
    peer.closed().cancelled().await;
    let barrier = tokio::spawn({
        let peer = peer.clone();
        async move { peer.barrier(&CancellationToken::new()).await }
    });
    assert_eq!(notes.recv().await.unwrap().method, "last");
    assert!(notes.recv().await.is_none());
    tokio::time::timeout(Duration::from_secs(5), barrier)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
