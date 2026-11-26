//! Unit and mock-socket tests for `orcherd-hc`.

use crate::{
    AgentKind, Core, End, Endpoint, Error, Method, Violation,
    agent::{self, Until},
    agent_in_front, agent_kind, agent_name,
    endpoint::Inputs,
    event::{Lines, MAX_LINE, Notice, TerminalChange, parse},
    frame::{self, Decoder, Frame, HEADER, Kind, MAX_PAYLOAD, encode},
    hello::{Hello, Mismatch},
    identify_argv,
    reply::{Replied, reply, when_free},
    wire::{Attach, BLOCKED, Run, STATUS_MISMATCH, SendText, StatusState, Terminal},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(5);

pub(crate) fn hello_reply(version: &str, methods: &[&str]) -> Value {
    json!({"jsonrpc": "2.0", "id": 0, "result": {
        "generation": 1, "version": version, "framed": true, "features": [],
        "core": {"id": "c0ffee", "name": "box"}, "methods": methods}})
}

pub(crate) fn every_method() -> Vec<&'static str> {
    Method::ALL.iter().map(|method| method.as_str()).collect()
}

pub(crate) fn terminal(address: &str) -> Value {
    json!({"address": address, "labels": {}, "argv": ["sh"], "cwd": "/tmp", "state": "live",
        "created_ms": 1, "screen": "", "cols": 80, "rows": 24})
}

pub(crate) fn pair() -> (UnixStream, UnixStream) {
    let (client, server) = UnixStream::pair().unwrap();
    for end in [&client, &server] {
        end.set_read_timeout(Some(crate::connection::POLL)).unwrap();
    }
    (client, server)
}

/// The core's side: answers the handshake, then reads and writes frames.
pub(crate) struct MockCore {
    decoder: Decoder,
    socket: UnixStream,
}

impl MockCore {
    /// Reads the client's hello line and answers with `reply`.
    pub(crate) fn accept(mut socket: UnixStream, reply: &Value) -> (Self, Value) {
        let mut line = Vec::new();
        let mut byte = [0];
        let deadline = Instant::now() + Duration::from_secs(5);
        while !line.ends_with(b"\n") {
            assert!(Instant::now() < deadline, "no hello");
            if let Ok(1) = socket.read(&mut byte) {
                line.push(byte[0]);
            }
        }
        let hello = serde_json::from_slice(&line).unwrap();
        let mut answer = serde_json::to_vec(reply).unwrap();
        answer.push(b'\n');
        socket.write_all(&answer).unwrap();
        (
            Self {
                decoder: Decoder::default(),
                socket,
            },
            hello,
        )
    }

    pub(crate) fn frame(&mut self) -> Frame {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buffer = [0; 4096];
        loop {
            if let Some(frame) = self.decoder.next_frame().unwrap() {
                return frame;
            }
            assert!(Instant::now() < deadline, "no frame");
            if let Ok(read) = self.socket.read(&mut buffer) {
                assert!(read > 0, "client closed");
                self.decoder.push(&buffer[..read]);
            }
        }
    }

    /// The next call on channel 0, skipping credit and other stream frames.
    pub(crate) fn request(&mut self) -> Value {
        loop {
            let frame = self.frame();
            if frame.channel == 0 && frame.kind == Kind::Data {
                return serde_json::from_slice(&frame.payload).unwrap();
            }
        }
    }

    pub(crate) fn send(&mut self, channel: u32, kind: Kind, payload: &[u8]) {
        self.socket
            .write_all(&encode(channel, kind, payload).unwrap())
            .unwrap();
    }

    pub(crate) fn answer(&mut self, request: &Value, result: Value) {
        let message = json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
        self.send(0, Kind::Data, &serde_json::to_vec(&message).unwrap());
    }

    pub(crate) fn refuse(&mut self, request: &Value, kind: &str, message: &str) {
        let payload = json!({"jsonrpc": "2.0", "id": request["id"],
            "error": {"code": -32000, "message": message, "data": {"kind": kind}}});
        self.send(0, Kind::Data, &serde_json::to_vec(&payload).unwrap());
    }
}

/// A client connected to a scripted core that speaks every method.
pub(crate) fn connected() -> (Core, MockCore) {
    let (client, server) = pair();
    let core =
        thread::spawn(move || MockCore::accept(server, &hello_reply("0.1.13", &every_method())));
    let client = Core::over(client).unwrap();
    let (core, _) = core.join().unwrap();
    (client, core)
}

#[test]
fn a_frame_round_trips_through_any_split() {
    let bytes = [
        encode(7, Kind::Data, b"hello").unwrap(),
        encode(0, Kind::Credit, &5u32.to_le_bytes()).unwrap(),
    ]
    .concat();
    for split in 0..bytes.len() {
        let mut decoder = Decoder::default();
        decoder.push(&bytes[..split]);
        let mut frames = Vec::new();
        while let Some(frame) = decoder.next_frame().unwrap() {
            frames.push(frame);
        }
        decoder.push(&bytes[split..]);
        while let Some(frame) = decoder.next_frame().unwrap() {
            frames.push(frame);
        }
        assert_eq!(
            frames,
            [
                Frame {
                    channel: 7,
                    kind: Kind::Data,
                    payload: b"hello".to_vec()
                },
                Frame {
                    channel: 0,
                    kind: Kind::Credit,
                    payload: 5u32.to_le_bytes().to_vec()
                },
            ],
            "split at {split}"
        );
        assert_eq!(decoder.pending(), 0);
    }
}

#[test]
fn an_oversized_header_fails_before_its_payload_arrives() {
    let mut header = vec![0; HEADER];
    header[5..9].copy_from_slice(&(MAX_PAYLOAD as u32 + 1).to_le_bytes());
    let mut decoder = Decoder::default();
    decoder.push(&header);
    let over = frame::Error::TooLarge(MAX_PAYLOAD + 1);
    assert_eq!(decoder.next_frame(), Err(over));
    assert_eq!(encode(1, Kind::Data, &vec![0; MAX_PAYLOAD + 1]), Err(over));
}

#[test]
fn an_unknown_kind_is_refused() {
    let mut frame = encode(1, Kind::Data, b"x").unwrap();
    frame[4] = 9;
    let mut decoder = Decoder::default();
    decoder.push(&frame);
    assert_eq!(decoder.next_frame(), Err(frame::Error::UnknownKind(9)));
}

#[test]
fn kinds_keep_their_wire_numbers() {
    let kinds = [
        Kind::Data,
        Kind::Credit,
        Kind::Close,
        Kind::Resize,
        Kind::Focus,
        Kind::Facts,
    ];
    for (number, kind) in kinds.into_iter().enumerate() {
        assert_eq!(u8::from(kind), number as u8);
        assert_eq!(Kind::try_from(number as u8), Ok(kind));
    }
}

fn make_hello(generation: u32, version: &str, methods: &[&str]) -> Hello {
    let mut reply = hello_reply(version, methods)["result"].clone();
    reply["generation"] = generation.into();
    serde_json::from_value(reply).unwrap()
}

#[test]
fn a_current_core_with_every_method_is_accepted() {
    assert_eq!(make_hello(1, "0.1.13", &every_method()).check(), Ok(()));
    assert_eq!(
        make_hello(1, "hc 0.2.0-beta", &every_method()).check(),
        Ok(())
    );
}

#[test]
fn an_old_or_newer_generation_is_refused() {
    let methods = every_method();
    assert_eq!(
        make_hello(2, "0.1.13", &methods).check(),
        Err(Mismatch::Newer(2))
    );
    for (generation, ver) in [(0, "0.1.13"), (1, "0.1.12")] {
        assert_eq!(
            make_hello(generation, ver, &methods).check(),
            Err(Mismatch::TooOld { have: ver.into() })
        );
    }
}

#[test]
fn missing_methods_are_named() {
    let methods: Vec<&str> = every_method()
        .into_iter()
        .filter(|name| *name != "doc.set")
        .collect();
    assert_eq!(
        make_hello(1, "0.1.13", &methods).check(),
        Err(Mismatch::Missing(vec![Method::DocSet]))
    );
    let methods: Vec<&str> = every_method()
        .into_iter()
        .filter(|name| *name != "hello")
        .collect();
    assert_eq!(make_hello(1, "0.1.13", &methods).check(), Ok(()));
}

// --- Endpoint resolution tests ---

fn resolve_endpoint(
    vars: &[(&str, &str)],
    run_user: Option<&str>,
    config: Option<&str>,
) -> crate::Result<Endpoint> {
    let vars: Vec<(String, OsString)> = vars
        .iter()
        .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
        .collect();
    let var = |name: &str| {
        vars.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let config = config.map(str::to_owned);
    let read = move |_: &Path| config.clone();
    let inputs = Inputs {
        #[cfg(unix)]
        run_user: run_user.map(PathBuf::from),
        config: &read,
    };
    Endpoint::resolve_with(&var, &inputs)
}

#[cfg(unix)]
#[test]
fn the_runtime_folder_holds_the_default_socket() {
    let at = |vars: &[(&str, &str)], run_user| resolve_endpoint(vars, run_user, None).unwrap().socket;
    assert_eq!(
        at(&[("HOME", "/h"), ("XDG_RUNTIME_DIR", "/run/me")], None),
        PathBuf::from("/run/me/herdr-core/default.sock")
    );
    assert_eq!(
        at(&[("HOME", "/h")], Some("/run/user/501")),
        PathBuf::from("/run/user/501/herdr-core/default.sock")
    );
    assert_eq!(
        at(&[("HOME", "/h")], None),
        PathBuf::from("/h/.local/state/herdr-core/default/default.sock")
    );
    assert_eq!(
        at(
            &[
                ("HOME", "/h"),
                ("XDG_STATE_HOME", "/s"),
                ("HC_SESSION", " smoke ")
            ],
            None
        ),
        PathBuf::from("/s/herdr-core/smoke/smoke.sock")
    );
}

#[test]
fn an_explicit_socket_wins_over_config_and_defaults() {
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let configured =
        resolve_endpoint(&[(home, "/h")], None, Some("socket = \"/cfg.sock\"\n")).unwrap();
    assert_eq!(configured.socket, PathBuf::from("/cfg.sock"));
    let explicit = resolve_endpoint(
        &[(home, "/h"), ("HC_SOCKET", "/env.sock")],
        None,
        Some("socket = \"/cfg.sock\"\n"),
    )
    .unwrap();
    assert_eq!(explicit.socket, PathBuf::from("/env.sock"));
    assert_ne!(
        resolve_endpoint(&[(home, "/h")], None, Some("socket = ["))
            .unwrap()
            .socket,
        PathBuf::from("/cfg.sock")
    );
}

#[test]
fn a_session_must_be_a_folder_name() {
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    for bad in ["a/b", "..", "a:b"] {
        assert!(
            matches!(resolve_endpoint(&[(home, "/h"), ("HC_SESSION", bad)], None, None), Err(Error::Session(name)) if name == bad)
        );
    }
    assert!(matches!(
        resolve_endpoint(&[], None, None),
        Err(Error::NoHome)
    ));
}

// --- Event parsing tests ---

fn notice(value: Value) -> Option<Notice> {
    parse(&serde_json::to_vec(&value).unwrap()).unwrap()
}

fn event_change(kind: &str, data: Value) -> TerminalChange {
    match notice(json!({"jsonrpc": "2.0", "method": "event",
        "params": {"seq": 4, "type": kind, "terminal": "t7", "data": data}}))
    {
        Some(Notice::Event(event)) => {
            assert_eq!((event.seq, event.terminal.as_str()), (4, "t7"));
            event.change
        }
        other => panic!("not an event: {other:?}"),
    }
}

#[test]
fn every_event_type_has_typed_data() {
    assert!(
        matches!(event_change("created", terminal("t7")), TerminalChange::Created(t) if t.address == "t7")
    );
    let status = event_change(
        "status",
        json!({"status": {"state": "blocked", "kind": "permission"}, "records": {}, "reported": true}),
    );
    assert!(
        matches!(status, TerminalChange::Status { reported: true, status: Some(s) } if s.state == StatusState::Blocked)
    );
    assert_eq!(
        event_change("title", json!({"title": "vim"})),
        TerminalChange::Title(Some("vim".into()))
    );
    assert_eq!(
        event_change("size", json!({"cols": 100, "rows": 30})),
        TerminalChange::Size {
            cols: 100,
            rows: 30
        }
    );
    assert_eq!(
        event_change("keep", json!({"keep": true})),
        TerminalChange::Keep(true)
    );
    assert_eq!(event_change("removed", json!({})), TerminalChange::Removed);
    assert_eq!(
        event_change("foreground", json!(null)),
        TerminalChange::Foreground(None)
    );
    assert!(
        matches!(event_change("finished", json!({"reason": "exited", "exit_code": 0, "signal": null, "finished_ms": 9})),
        TerminalChange::Finished(ended) if ended.exit_code == Some(0))
    );
    assert_eq!(
        event_change("turn", json!({"turn": 3, "state": "done"})),
        TerminalChange::Turn {
            state: Some("done".into()),
            turn: 3
        }
    );
}

#[test]
fn newer_words_do_not_fail() {
    assert_eq!(
        event_change("teleported", json!({})),
        TerminalChange::Unknown("teleported".into())
    );
    let status = event_change(
        "status",
        json!({"status": {"state": "dreaming"}, "reported": true}),
    );
    assert!(
        matches!(status, TerminalChange::Status { status: Some(s), .. } if s.state == StatusState::Other)
    );
    assert_eq!(
        notice(json!({"jsonrpc": "2.0", "method": "news", "params": {}})),
        None
    );
}

#[test]
fn heartbeats_and_missed_snapshots_parse() {
    assert_eq!(
        notice(json!({"method": "heartbeat", "params": {"seq": 42}})),
        Some(Notice::Heartbeat { seq: 42 })
    );
    let missed = notice(
        json!({"method": "missed", "params": {"seq": 57, "terminals": [terminal("t1")]}}),
    );
    assert!(matches!(missed, Some(Notice::Missed { seq: 57, terminals }) if terminals.len() == 1));
}

#[test]
fn lines_join_across_frames_and_stay_bounded() {
    let mut lines = Lines::default();
    assert!(lines.push(b"{\"a\":").unwrap().is_empty());
    assert_eq!(
        lines.push(b"1}\n\n{\"b\":2}\n{").unwrap(),
        [b"{\"a\":1}".to_vec(), b"{\"b\":2}".to_vec()]
    );
    let mut long = Lines::default();
    assert!(matches!(
        long.push(&vec![b'x'; MAX_LINE + 2]),
        Err(Error::Protocol(Violation::LongLine(_)))
    ));
}

#[test]
fn doc_events_name_no_terminal() {
    let local = br#"{"jsonrpc":"2.0","method":"event","params":{"seq":43,"type":"doc","data":{"name":"orcherd/team/t1","version":2}}}"#;
    assert_eq!(
        parse(local).unwrap(),
        Some(Notice::Doc {
            machine: None,
            name: "orcherd/team/t1".into(),
            seq: 43,
            version: 2,
        })
    );
    let remote = br#"{"jsonrpc":"2.0","method":"event","params":{"seq":44,"type":"doc","machine":"m1","data":{"name":"orcherd/team/t1","version":3}}}"#;
    assert!(matches!(
        parse(remote).unwrap(),
        Some(Notice::Doc { machine: Some(machine), version: 3, .. }) if machine == "m1"
    ));
    let missing = br#"{"jsonrpc":"2.0","method":"event","params":{"seq":43,"type":"doc"}}"#;
    assert!(parse(missing).is_err());
}

// --- Connection, RPC, Stream, Doc, Label, Send, Preview tests ---

#[test]
fn the_handshake_asks_for_framed_mode_and_checks_the_core() {
    let (client, server) = pair();
    let core =
        thread::spawn(move || MockCore::accept(server, &hello_reply("0.1.13", &every_method())));
    let connected = Core::over(client).unwrap();
    let (_, hello) = core.join().unwrap();
    assert_eq!(hello["method"], "hello");
    assert_eq!(hello["params"], json!({"framed": true, "generation": 1}));
    assert_eq!(connected.hello().core.name, "box");

    let (client, server) = pair();
    let core =
        thread::spawn(move || MockCore::accept(server, &hello_reply("0.1.0", &every_method())));
    assert!(matches!(
        Core::over(client),
        Err(Error::Compat(Mismatch::TooOld { .. }))
    ));
    drop(core.join().unwrap());
}

#[test]
fn calls_are_answered_by_id_even_out_of_order_and_doc_set_works() {
    let (client, mut core) = connected();
    let client = Arc::new(client);
    let first = thread::spawn({
        let client = Arc::clone(&client);
        move || client.doc_get("one").unwrap()
    });
    let one = core.request();
    let second = thread::spawn({
        let client = Arc::clone(&client);
        move || client.doc_set("two", &json!({"team": "k2"}), 1).unwrap()
    });
    let two = core.request();
    assert_eq!(
        (
            one["params"]["name"].as_str(),
            two["params"]["name"].as_str()
        ),
        (Some("one"), Some("two"))
    );
    assert_eq!(two["method"], "doc.set");
    core.answer(&two, json!({"version": 2}));
    core.answer(&one, json!({"value": 1, "version": 1}));
    assert_eq!(first.join().unwrap().value, json!(1));
    assert_eq!(second.join().unwrap(), 2);
}

#[test]
fn a_refusal_keeps_the_cores_kind() {
    let (client, mut core) = connected();
    let call = thread::spawn(move || client.kill("t9"));
    let request = core.request();
    core.refuse(&request, "not_found", "no terminal t9");
    match call.join().unwrap() {
        Err(Error::Refused {
            method: Method::TerminalKill,
            kind,
            message,
        }) => {
            assert_eq!(
                (kind.as_deref(), message.as_str()),
                (Some("not_found"), "no terminal t9")
            );
        }
        other => panic!("{other:?}"),
    }
}

fn attached(
    client: Core,
    core: &mut MockCore,
    then: impl FnOnce(&mut MockCore),
) -> (Core, crate::Attachment) {
    let attach = thread::spawn(move || {
        let attachment = client.attach(&Attach {
            cols: 80,
            rows: 24,
            terminal: "t7",
            ..Attach::default()
        });
        (client, attachment)
    });
    let request = core.request();
    assert_eq!(
        request["params"],
        json!({"cols": 80, "rows": 24, "terminal": "t7"})
    );
    core.answer(&request, json!({"channel": 5, "terminal": terminal("t7")}));
    then(core);
    let (client, attachment) = attach.join().unwrap();
    (client, attachment.unwrap())
}

#[test]
fn stream_bytes_that_follow_the_answer_are_kept_and_credited() {
    let (client, mut core) = connected();
    let (_client, attachment) = attached(client, &mut core, |core| {
        core.send(5, Kind::Data, b"\x1b[2Jhello")
    });
    let chunk = attachment.read(WAIT).unwrap();
    assert_eq!(
        (chunk.bytes.as_slice(), chunk.end),
        (&b"\x1b[2Jhello"[..], None)
    );
    let credit = core.frame();
    assert_eq!(
        (credit.channel, credit.kind, credit.payload),
        (5, Kind::Credit, 9u32.to_le_bytes().to_vec())
    );
}

#[test]
fn read_preview_attaches_in_observe_mode_and_detaches() {
    let (client, mut core) = connected();
    let reader = thread::spawn(move || client.read_preview("t7", 50, WAIT));
    let request = core.request();
    assert_eq!(request["method"], "terminal.attach");
    assert_eq!(
        request["params"],
        json!({"cols": 120, "rows": 40, "history": 50, "observe": true, "terminal": "t7"})
    );
    core.answer(&request, json!({"channel": 8, "terminal": terminal("t7")}));
    core.send(8, Kind::Data, b"worker ready\n");
    let preview = reader.join().unwrap().unwrap();
    assert_eq!(preview, "worker ready\n");
}

#[test]
fn a_closed_stream_reports_why_once() {
    let (client, mut core) = connected();
    let (_client, attachment) = attached(client, &mut core, |core| {
        core.send(
            5,
            Kind::Close,
            br#"{"reason":"exited","exit_code":3,"signal":null}"#,
        )
    });
    let chunk = attachment.read(WAIT).unwrap();
    assert!(matches!(chunk.end, Some(End::Closed(closed)) if closed.exit_code == Some(3)));
    assert_eq!(
        attachment.read(Duration::from_millis(20)).unwrap().end,
        None
    );
}

#[test]
fn losing_the_core_fails_waiting_calls_and_ends_streams() {
    let (client, mut core) = connected();
    let client = Arc::new(client);
    let call = thread::spawn({
        let client = Arc::clone(&client);
        move || client.list(false)
    });
    let _ = core.request();
    drop(core);
    assert!(matches!(call.join().unwrap(), Err(Error::Lost)));
    assert!(client.is_lost());
    assert!(matches!(client.doc_get("x"), Err(Error::Lost)));
}

#[test]
fn a_subscription_turns_lines_into_notices() {
    let (client, mut core) = connected();
    let subscribe = thread::spawn(move || {
        let subscription = client.subscribe();
        (client, subscription)
    });
    let request = core.request();
    core.answer(
        &request,
        json!({"channel": 3, "seq": 10, "terminals": [terminal("t1")]}),
    );
    let (_client, subscription) = subscribe.join().unwrap();
    let mut subscription = subscription.unwrap();
    assert_eq!((subscription.seq, subscription.terminals.len()), (10, 1));
    let line = |value: Value| [serde_json::to_vec(&value).unwrap(), b"\n".to_vec()].concat();
    let bytes = [
        line(json!({"method": "heartbeat", "params": {"seq": 11}})),
        line(json!({"method": "event", "params": {"seq": 12, "type": "title", "terminal": "t1", "data": {"title": "x"}}})),
    ]
    .concat();
    let (head, tail) = bytes.split_at(7);
    core.send(3, Kind::Data, head);
    core.send(3, Kind::Data, tail);
    let mut notices = Vec::new();
    while notices.len() < 2 {
        notices.extend(subscription.next(WAIT).unwrap());
    }
    assert_eq!(notices[0], Notice::Heartbeat { seq: 11 });
    core.send(3, Kind::Close, b"");
    let ended = (0..100).find_map(|_| subscription.next(Duration::from_millis(50)).err());
    assert!(matches!(ended, Some(Error::Lost)));
}

#[test]
fn a_label_carries_the_terminal_and_its_labels() {
    let (client, mut core) = connected();
    let call = thread::spawn(move || {
        client.label(
            "t3",
            &BTreeMap::from([("orcherd.seat", "super"), ("gone", "")]),
        )
    });
    let request = core.request();
    assert_eq!(request["method"], "terminal.label");
    assert_eq!(
        request["params"],
        json!({"terminal": "t3", "labels": {"orcherd.seat": "super", "gone": ""}})
    );
    core.answer(&request, terminal("t3"));
    assert_eq!(call.join().unwrap().unwrap().address, "t3");
}

#[test]
fn a_send_carries_the_terminal_and_text_and_handles_refusals() {
    let (client, mut core) = connected();
    let call = thread::spawn(move || {
        let sent = client
            .send(&SendText {
                terminal: "t3",
                text: "go on",
                only_if: Some("idle"),
                ..SendText::default()
            })
            .unwrap();
        let blocked = client.send(&SendText {
            terminal: "t3",
            text: "yes",
            ..SendText::default()
        });
        let mismatch = client.send(&SendText {
            terminal: "t3",
            text: "doorbell",
            only_if: Some("idle"),
            ..SendText::default()
        });
        (sent, blocked, mismatch)
    });
    let req1 = core.request();
    assert_eq!(req1["method"], "terminal.send");
    assert_eq!(
        req1["params"],
        json!({"terminal": "t3", "text": "go on", "if": "idle"})
    );
    core.answer(
        &req1,
        json!({"turn": 4, "started": true, "status": "idle"}),
    );

    let req2 = core.request();
    core.refuse(&req2, BLOCKED, "t3 is blocked");

    let req3 = core.request();
    core.refuse(&req3, STATUS_MISMATCH, "t3 is working");

    let (sent, blocked, mismatch) = call.join().unwrap();
    assert_eq!((sent.turn, sent.started), (4, Some(true)));
    assert!(
        matches!(blocked, Err(Error::Refused { method: Method::TerminalSend, kind: Some(k), .. }) if k == BLOCKED)
    );
    assert!(
        matches!(mismatch, Err(Error::Refused { method: Method::TerminalSend, kind: Some(k), .. }) if k == STATUS_MISMATCH)
    );
}

// --- Identity & Reply tests ---

fn record(value: Value) -> Terminal {
    serde_json::from_value(value).unwrap()
}

fn claude_record(program: &str, front: i32, state: &str) -> Value {
    let mut value = terminal("t1");
    value["argv"] = json!([program]);
    value["pid"] = json!(100);
    value["foreground"] = json!({"pid": front, "name": "claude", "argv": ["claude"], "cwd": ""});
    value["status"] = json!({"state": state, "app": "claude-code"});
    value
}

#[test]
fn the_agent_is_identified_by_its_report_or_its_process() {
    assert_eq!(
        identify_argv("zcode", &["zcode".into()]),
        Some(AgentKind::Zcode)
    );
    assert_eq!(
        identify_argv("agy", &["agy".into(), "--conversation=1".into()]),
        Some(AgentKind::Antigravity)
    );
    assert_eq!(
        identify_argv("node", &["node".into(), "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js".into()]),
        Some(AgentKind::Claude)
    );
    let mut value = terminal("t1");
    value["foreground"] =
        json!({"pid": 7, "name": "node", "argv": ["node", "/x/codex"], "cwd": ""});
    assert_eq!(agent_kind(&record(value.clone())), Some(AgentKind::Codex));
    value["status"] = json!({"state": "idle", "app": "claude-code"});
    assert_eq!(agent_kind(&record(value)), Some(AgentKind::Claude));
    assert_eq!(agent_kind(&record(terminal("t2"))), None);
}

#[test]
fn an_unknown_program_that_reports_a_status_is_named_by_its_report() {
    let mut value = terminal("t1");
    value["status"] = json!({"state": "idle", "app": "my\u{202e}bot"});
    assert_eq!(
        agent_name(&record(value.clone())).as_deref(),
        Some("mybot")
    );
    value["status"] = json!({"state": "idle", "app": "claude-code"});
    assert_eq!(agent_name(&record(value)).as_deref(), Some("claude"));
    assert_eq!(agent_name(&record(terminal("t2"))), None);
}

#[test]
fn the_agent_is_in_front_until_its_shell_is_back() {
    assert!(agent_in_front(
        &record(claude_record("zsh", 200, "idle")),
        "claude"
    ));
    let mut quit = claude_record("zsh", 100, "idle");
    quit["foreground"] = json!({"pid": 100, "name": "zsh", "argv": ["zsh"], "cwd": ""});
    assert!(!agent_in_front(&record(quit), "claude"));
    assert!(agent_in_front(
        &record(claude_record("claude", 100, "idle")),
        "claude"
    ));
    let mut exec = claude_record("zsh", 100, "idle");
    exec["foreground"] = json!({"pid": 100, "name": "claude", "argv": ["claude"], "cwd": ""});
    assert!(agent_in_front(&record(exec.clone()), "claude"));
    let mut codex = exec;
    codex["foreground"] = json!({"pid": 100, "name": "codex", "argv": ["codex"], "cwd": ""});
    assert!(!agent_in_front(&record(codex.clone()), "claude"));
    assert!(agent_in_front(&record(codex), "codex"));
    assert!(!agent_in_front(
        &record(claude_record("zsh", 200, "idle")),
        "codex"
    ));
}

#[test]
fn an_idle_agent_is_typed_to_only_while_the_core_says_it_is_idle() {
    let (client, mut core) = connected();
    let call = thread::spawn(move || reply(&client, "t1", "claude", "go on"));
    let list = core.request();
    assert_eq!(list["method"], "terminal.list");
    core.answer(
        &list,
        json!({"terminals": [claude_record("zsh", 200, "idle")]}),
    );
    let send = core.request();
    assert_eq!(send["method"], "terminal.send");
    assert_eq!(
        send["params"],
        json!({"terminal": "t1", "text": "go on", "if": "idle"})
    );
    core.answer(&send, json!({"turn": 2, "started": true}));
    assert_eq!(call.join().unwrap().unwrap(), Replied::Sent);
}

#[test]
fn a_reply_to_an_agent_that_left_is_not_typed() {
    let (client, mut core) = connected();
    let call = thread::spawn(move || reply(&client, "t1", "claude", "rm -rf build"));
    let list = core.request();
    let mut quit = claude_record("zsh", 100, "idle");
    quit["foreground"] = json!({"pid": 100, "name": "zsh", "argv": ["zsh"], "cwd": ""});
    core.answer(&list, json!({"terminals": [quit]}));
    assert_eq!(call.join().unwrap().unwrap(), Replied::Gone);
}

#[test]
fn when_free_waits_for_the_turn_to_end_then_goes_to_the_idle_agent() {
    let (client, mut core) = connected();
    let call = thread::spawn(move || {
        let stop = AtomicBool::new(false);
        when_free(
            &client,
            "t1",
            "claude",
            "see note",
            Until {
                deadline: Instant::now() + Duration::from_secs(60),
                stop: &stop,
            },
        )
    });
    let list = core.request();
    core.answer(
        &list,
        json!({"terminals": [claude_record("zsh", 200, "working")]}),
    );
    let wait = core.request();
    assert_eq!(wait["method"], "terminal.wait");
    core.answer(
        &wait,
        json!({"ended": true, "reason": "status", "status": "idle", "turn": 3}),
    );
    let list = core.request();
    core.answer(
        &list,
        json!({"terminals": [claude_record("zsh", 200, "idle")]}),
    );
    let send = core.request();
    assert_eq!(
        send["params"],
        json!({"terminal": "t1", "text": "see note", "if": "idle"})
    );
    core.answer(&send, json!({"turn": 4, "started": true}));
    assert_eq!(call.join().unwrap().unwrap(), Replied::Sent);
}

// --- Agent launch & readiness tests ---

fn waited(ended: bool, reason: &str, turn: u64) -> Value {
    json!({"terminal": "t1", "ended": ended, "reason": reason, "turn": turn,
        "children": [], "status": if ended { "idle" } else { "working" }})
}

fn until(stop: &AtomicBool) -> Until<'_> {
    Until {
        deadline: Instant::now() + Duration::from_secs(30),
        stop,
    }
}

fn expect_call(core: &mut MockCore, method: &str, result: Value) -> Value {
    let request = core.request();
    assert_eq!(request["method"], method, "{request}");
    core.answer(&request, result);
    request
}

#[test]
fn the_prompt_waits_for_the_agent_to_be_ready() {
    let (client, mut core) = connected();
    let launched = thread::spawn(move || {
        let stop = AtomicBool::new(false);
        let argv = ["claude".to_owned()];
        let run = Run {
            argv: &argv,
            cwd: Some("/work"),
            ..Run::default()
        };
        agent::launch(&client, &run, "fix it", until(&stop))
    });
    let run = expect_call(&mut core, "terminal.run", terminal("t1"));
    assert_eq!(run["params"]["argv"], json!(["claude"]));
    let wait = expect_call(&mut core, "terminal.wait", waited(false, "timeout", 0));
    assert_eq!(wait["params"]["terminal"], "t1");
    assert!(wait["params"]["timeout"].as_f64().unwrap() <= 5.);
    expect_call(&mut core, "terminal.wait", waited(true, "status", 0));
    let send = expect_call(
        &mut core,
        "terminal.send",
        json!({"started": true, "status": "idle", "turn": 0}),
    );
    assert_eq!(send["params"], json!({"terminal": "t1", "text": "fix it"}));
    let started = launched.join().unwrap().unwrap();
    assert_eq!(started.terminal.address, "t1");
    assert_eq!(started.sent.turn, 0);
}

#[test]
fn an_agent_that_exits_or_blocks_before_it_is_ready_gets_no_prompt() {
    let (client, mut core) = connected();
    let launched = thread::spawn(move || {
        let stop = AtomicBool::new(false);
        agent::launch(&client, &Run::default(), "fix it", until(&stop))
    });
    expect_call(&mut core, "terminal.run", terminal("t1"));
    let mut exit = waited(true, "exit", 0);
    exit["exit_code"] = json!(127);
    expect_call(&mut core, "terminal.wait", exit);
    let error = launched.join().unwrap().unwrap_err();
    assert!(
        matches!(error, Error::AgentExited { code: Some(127) }),
        "{error}"
    );

    let (client, mut core) = connected();
    let prompted = thread::spawn(move || {
        let stop = AtomicBool::new(false);
        agent::prompt(&client, "t1", "fix it", until(&stop))
    });
    let mut blocked = waited(true, "status", 0);
    blocked["status"] = json!("blocked");
    expect_call(&mut core, "terminal.wait", blocked);
    let error = prompted.join().unwrap().unwrap_err();
    assert!(
        matches!(error, Error::AgentNotReady { status: Some(ref status) } if status == "blocked"),
        "{error}"
    );
}

#[test]
fn finishing_waits_for_the_turn_the_prompt_started() {
    let (client, mut core) = connected();
    let finished = thread::spawn(move || {
        let stop = AtomicBool::new(false);
        agent::finish(&client, "t1", 0, until(&stop))
    });
    expect_call(&mut core, "terminal.wait", waited(true, "status", 0));
    expect_call(&mut core, "terminal.wait", waited(false, "timeout", 1));
    expect_call(&mut core, "terminal.wait", waited(true, "status", 1));
    assert_eq!(finished.join().unwrap().unwrap().turn, 1);
}
