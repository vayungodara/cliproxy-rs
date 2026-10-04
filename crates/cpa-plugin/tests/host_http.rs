//! `host.http.*` beyond the Go golden: upstream capture (Go's `recordHTTPRequest`,
//! `RecordAPIResponseMetadata`, `AppendAPIResponseChunk` in http_bridge.go) into the
//! calling request's capture sink, operations ending with their callback context or
//! request, and the timing of Go's body reads (cancellation, hand-over, 32 KiB reads).

use std::sync::{Arc, Mutex};

use cpa_core::exec::{CaptureEvent, CaptureObserver, CaptureSink};
use cpa_plugin::Host;
use cpa_plugin::callbacks::RequestScope;
use cpa_plugin::client::CallbackInstance;
use cpa_plugin::testing::{call_from_plugin, callback_context, raw_upstream};
use serde_json::Value;

#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl CaptureObserver for Recorder {
    fn record(&self, event: CaptureEvent<'_>) {
        let line = match event {
            CaptureEvent::Request(r) => format!(
                "request {} {} headers={:?} body={:?} provider={:?} auth={:?}",
                r.method,
                r.url,
                r.headers,
                String::from_utf8_lossy(r.body),
                r.provider,
                r.auth_id
            ),
            CaptureEvent::ResponseMetadata(status, headers) => {
                let mut names: Vec<_> = headers.iter().map(|(n, v)| format!("{n}={v}")).collect();
                names.sort();
                format!("metadata {status} {names:?}")
            }
            CaptureEvent::ResponseChunk(chunk) => format!("chunk {:?}", String::from_utf8_lossy(chunk)),
            CaptureEvent::ResponseError(e) => format!("error {e}"),
            _ => "other".into(),
        };
        self.0.lock().unwrap().push(line);
    }
}

fn result(raw: &[u8]) -> Value {
    let envelope: Value = serde_json::from_slice(raw).unwrap();
    assert_eq!(envelope["ok"], true, "{envelope}");
    envelope["result"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exchanges_are_captured_in_the_calling_request() {
    let upstream = raw_upstream().await;
    let host = Host::new();
    let instance = Arc::new(CallbackInstance::default());
    let recorder = Arc::new(Recorder::default());
    let scope = RequestScope {
        capture: CaptureSink::new(recorder.clone()),
        ..Default::default()
    };
    let guard = callback_context(&host, "p", instance.clone(), scope);
    let callback_id = guard.id().to_owned();
    let (h, i, u) = (host.clone(), instance.clone(), upstream.clone());
    tokio::task::spawn_blocking(move || {
        let request = format!(
            r#"{{"host_callback_id":"{callback_id}","method":"POST","url":"http://{u}/echo","headers":{{"X-A":["1","2"]}},"body":"aGk="}}"#
        );
        let resp = result(&call_from_plugin(&h, "p", &i, "host.http.do", request.as_bytes()).unwrap());
        assert_eq!(resp["StatusCode"], 200);
        let request = format!(r#"{{"host_callback_id":"{callback_id}","url":"http://{u}/stream"}}"#);
        let resp = result(&call_from_plugin(&h, "p", &i, "host.http.do_stream", request.as_bytes()).unwrap());
        let read = format!(r#"{{"stream_id":"{}"}}"#, resp["stream_id"].as_str().unwrap());
        loop {
            let chunk = result(&call_from_plugin(&h, "p", &i, "host.http.stream_read", read.as_bytes()).unwrap());
            if chunk["done"] == true {
                break;
            }
        }
        // Without a callback context there is no request to capture into.
        let request = format!(r#"{{"url":"http://{u}/status"}}"#);
        let resp = result(&call_from_plugin(&h, "p", &i, "host.http.do", request.as_bytes()).unwrap());
        assert_eq!(resp["StatusCode"], 404);
    })
    .await
    .unwrap();
    let events = recorder.0.lock().unwrap().clone();
    let echo = "POST /echo HTTP/1.1\r\nHost: UPSTREAM\r\nUser-Agent: Go-http-client/1.1\r\nContent-Length: 2\r\nX-A: 1\r\nX-A: 2\r\nAccept-Encoding: gzip\r\n\r\nhi";
    let want = [
        format!(
            r#"request POST http://{upstream}/echo headers=[("X-A", "1"), ("X-A", "2")] body="hi" provider="" auth="""#
        ),
        format!(
            r#"metadata 200 ["Content-Length={}", "Content-Type=text/plain", "X-Multi=a", "X-Multi=b"]"#,
            echo.len()
        ),
        format!("chunk {echo:?}"),
        format!(r#"request GET http://{upstream}/stream headers=[] body="" provider="" auth="""#),
        r#"metadata 200 ["Content-Type=text/event-stream"]"#.to_owned(),
        r#"chunk "one""#.to_owned(),
        r#"chunk "two""#.to_owned(),
        r#"chunk "three""#.to_owned(),
    ];
    assert_eq!(events, want);
    drop(guard);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operations_end_with_their_callback_context() {
    let upstream = raw_upstream().await;
    let host = Host::new();
    let instance = Arc::new(CallbackInstance::default());
    let guard = callback_context(&host, "p", instance.clone(), RequestScope::default());
    let callback_id = guard.id().to_owned();
    let (h, i) = (host.clone(), instance.clone());
    let open = format!(r#"{{"host_callback_id":"{callback_id}"}}"#);
    let operation = tokio::task::spawn_blocking(move || {
        result(&call_from_plugin(&h, "p", &i, "host.http.operation_open", open.as_bytes()).unwrap())["operation_id"]
            .as_str()
            .unwrap()
            .to_owned()
    })
    .await
    .unwrap();
    drop(guard);
    let (h, i) = (host.clone(), instance.clone());
    let err = tokio::task::spawn_blocking(move || {
        let request = format!(
            r#"{{"host_callback_id":"{callback_id}","operation_id":"{operation}","url":"http://{upstream}/echo"}}"#
        );
        call_from_plugin(&h, "p", &i, "host.http.do", request.as_bytes()).unwrap_err()
    })
    .await
    .unwrap();
    assert!(err.message.contains("is not open"), "{}", err.message);
    // Another instance of the plugin cannot use or cancel this instance's operations.
    let other = Arc::new(CallbackInstance::default());
    let (h, i) = (host.clone(), instance.clone());
    tokio::task::spawn_blocking(move || {
        let open = result(&call_from_plugin(&h, "p", &i, "host.http.operation_open", b"{}").unwrap());
        let id = open["operation_id"].as_str().unwrap().to_owned();
        let cancel = format!(r#"{{"operation_id":"{id}"}}"#);
        call_from_plugin(&h, "p", &other, "host.http.cancel", cancel.as_bytes()).unwrap();
        let request = format!(r#"{{"operation_id":"{id}","url":"http://127.0.0.1:1/never"}}"#);
        let err = call_from_plugin(&h, "p", &other, "host.http.do", request.as_bytes()).unwrap_err();
        assert!(err.message.contains("is not open"), "{}", err.message);
    })
    .await
    .unwrap();
}

fn recorded_scope() -> (Arc<Recorder>, RequestScope) {
    let recorder = Arc::new(Recorder::default());
    let scope = RequestScope {
        capture: CaptureSink::new(recorder.clone()),
        ..Default::default()
    };
    (recorder, scope)
}

/// Waits up to two seconds for the recorder to hold `want` events.
async fn events(recorder: &Recorder, want: usize) -> Vec<String> {
    for _ in 0..200 {
        let events = recorder.0.lock().unwrap().clone();
        if events.len() >= want {
            return events;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    recorder.0.lock().unwrap().clone()
}

/// Go `Do`: cancelling the operation while the body is read keeps the partial body
/// in the capture, records the context error after it, and fails as a read error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_whole_body_read_captures_what_arrived() {
    let upstream = raw_upstream().await;
    let host = Host::new();
    let instance = Arc::new(CallbackInstance::default());
    let (recorder, scope) = recorded_scope();
    let guard = callback_context(&host, "p", instance.clone(), scope);
    let callback_id = guard.id().to_owned();
    let (h, i) = (host.clone(), instance.clone());
    let open = format!(r#"{{"host_callback_id":"{callback_id}"}}"#);
    let operation = tokio::task::spawn_blocking(move || {
        result(&call_from_plugin(&h, "p", &i, "host.http.operation_open", open.as_bytes()).unwrap())["operation_id"]
            .as_str()
            .unwrap()
            .to_owned()
    })
    .await
    .unwrap();
    let (h, i, op) = (host.clone(), instance.clone(), operation.clone());
    let read = tokio::task::spawn_blocking(move || {
        let request =
            format!(r#"{{"host_callback_id":"{callback_id}","operation_id":"{op}","url":"http://{upstream}/slow"}}"#);
        call_from_plugin(&h, "p", &i, "host.http.do", request.as_bytes()).unwrap_err()
    });
    // The first chunk has arrived when the read is under way.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let (h, i) = (host.clone(), instance.clone());
    tokio::task::spawn_blocking(move || {
        let cancel = format!(r#"{{"operation_id":"{operation}"}}"#);
        call_from_plugin(&h, "p", &i, "host.http.cancel", cancel.as_bytes()).unwrap();
    })
    .await
    .unwrap();
    let err = read.await.unwrap();
    assert_eq!(err.message, "read host http response: context canceled");
    let events = events(&recorder, 4).await;
    assert_eq!(events[2..], [r#"chunk "one""#, "error context canceled"]);
    drop(guard);
}

/// Go `DoStream`'s unbuffered hand-over: the reader takes one chunk ahead and no
/// more until the plugin reads; a read cancelled by closing the stream is captured as
/// the context error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_reads_wait_for_the_plugin_and_close_cancels_the_read() {
    let upstream = raw_upstream().await;
    let host = Host::new();
    let instance = Arc::new(CallbackInstance::default());
    let (recorder, scope) = recorded_scope();
    let guard = callback_context(&host, "p", instance.clone(), scope);
    let callback_id = guard.id().to_owned();
    let (h, i, u, id) = (host.clone(), instance.clone(), upstream.clone(), callback_id.clone());
    let stream = tokio::task::spawn_blocking(move || {
        let request = format!(r#"{{"host_callback_id":"{id}","url":"http://{u}/stream"}}"#);
        result(&call_from_plugin(&h, "p", &i, "host.http.do_stream", request.as_bytes()).unwrap())["stream_id"]
            .as_str()
            .unwrap()
            .to_owned()
    })
    .await
    .unwrap();
    // All three chunks are sent within 150 ms; unread, only the first is taken.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let chunks = |events: &[String]| events.iter().filter(|e| e.starts_with("chunk")).count();
    assert_eq!(chunks(&recorder.0.lock().unwrap()), 1);
    let (h, i) = (host.clone(), instance.clone());
    let read = format!(r#"{{"stream_id":"{stream}"}}"#);
    tokio::task::spawn_blocking(move || {
        let chunk = result(&call_from_plugin(&h, "p", &i, "host.http.stream_read", read.as_bytes()).unwrap());
        assert_eq!(chunk["payload"], "b25l");
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(chunks(&recorder.0.lock().unwrap()), 2);
    drop(guard);

    // A stream whose next chunk is seconds away: closing it cancels the pending read.
    let (recorder, scope) = recorded_scope();
    let guard = callback_context(&host, "p", instance.clone(), scope);
    let id = guard.id().to_owned();
    let (h, i) = (host.clone(), instance.clone());
    tokio::task::spawn_blocking(move || {
        let request = format!(r#"{{"host_callback_id":"{id}","url":"http://{upstream}/slow"}}"#);
        let stream = result(&call_from_plugin(&h, "p", &i, "host.http.do_stream", request.as_bytes()).unwrap());
        let stream = format!(r#"{{"stream_id":"{}"}}"#, stream["stream_id"].as_str().unwrap());
        let chunk = result(&call_from_plugin(&h, "p", &i, "host.http.stream_read", stream.as_bytes()).unwrap());
        assert_eq!(chunk["payload"], "b25l");
        std::thread::sleep(std::time::Duration::from_millis(200));
        call_from_plugin(&h, "p", &i, "host.http.stream_close", stream.as_bytes()).unwrap();
    })
    .await
    .unwrap();
    let events = events(&recorder, 4).await;
    assert_eq!(events[2..], [r#"chunk "one""#, "error context canceled"]);
    drop(guard);
}

/// Go opens operations under the request's context: when the request is cancelled,
/// its open operations leave the registry even while the callback context stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_cancellation_ends_open_operations() {
    let upstream = raw_upstream().await;
    let host = Host::new();
    let instance = Arc::new(CallbackInstance::default());
    let scope = RequestScope::default();
    let cancel = scope.cancel.clone();
    let guard = callback_context(&host, "p", instance.clone(), scope);
    let callback_id = guard.id().to_owned();
    let (h, i, id) = (host.clone(), instance.clone(), callback_id.clone());
    let operation = tokio::task::spawn_blocking(move || {
        let open = format!(r#"{{"host_callback_id":"{id}"}}"#);
        result(&call_from_plugin(&h, "p", &i, "host.http.operation_open", open.as_bytes()).unwrap())["operation_id"]
            .as_str()
            .unwrap()
            .to_owned()
    })
    .await
    .unwrap();
    cancel.cancel();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (h, i, op) = (host.clone(), instance.clone(), operation.clone());
    let err = tokio::task::spawn_blocking(move || {
        let request =
            format!(r#"{{"host_callback_id":"{callback_id}","operation_id":"{op}","url":"http://{upstream}/echo"}}"#);
        call_from_plugin(&h, "p", &i, "host.http.do", request.as_bytes()).unwrap_err()
    })
    .await
    .unwrap();
    assert_eq!(err.message, format!("host http operation {operation:?} is not open"));
    drop(guard);
}

/// Operations still open when a host is dropped without shutdown do not leave their
/// watcher tasks behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_host_ends_operation_watchers() {
    let metrics = tokio::runtime::Handle::current().metrics();
    let before = metrics.num_alive_tasks();
    let host = Host::new();
    let instance = Arc::new(CallbackInstance::default());
    let guard = callback_context(&host, "p", instance.clone(), RequestScope::default());
    let (h, i) = (host.clone(), instance.clone());
    tokio::task::spawn_blocking(move || {
        for _ in 0..3 {
            result(&call_from_plugin(&h, "p", &i, "host.http.operation_open", b"{}").unwrap());
        }
    })
    .await
    .unwrap();
    assert_eq!(metrics.num_alive_tasks(), before + 3);
    drop(guard);
    drop(host);
    for _ in 0..100 {
        if metrics.num_alive_tasks() == before {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{} watcher tasks left", metrics.num_alive_tasks() - before);
}

/// `host.http.do` buffers at most `MAX_WHOLE_RESPONSE`; an endless answer fails as a
/// read error instead of growing without bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whole_body_reads_are_bounded() {
    use tokio::io::AsyncWriteExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut head = [0u8; 4096];
        let _ = tokio::io::AsyncReadExt::read(&mut conn, &mut head).await;
        let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n").await;
        let chunk = vec![b'x'; 1 << 20];
        while conn.write_all(&chunk).await.is_ok() {}
    });
    let host = Host::new();
    let instance = Arc::new(CallbackInstance::default());
    let guard = callback_context(&host, "p", instance.clone(), RequestScope::default());
    let callback_id = guard.id().to_owned();
    let (h, i) = (host.clone(), instance.clone());
    let err = tokio::task::spawn_blocking(move || {
        let request = format!(r#"{{"host_callback_id":"{callback_id}","url":"http://{addr}/big"}}"#);
        call_from_plugin(&h, "p", &i, "host.http.do", request.as_bytes()).unwrap_err()
    })
    .await
    .unwrap();
    assert_eq!(
        err.message,
        format!("read host http response: response body exceeds {} bytes", 64 << 20)
    );
    drop(guard);
}
