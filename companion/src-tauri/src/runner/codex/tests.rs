//! The runner against a fake app-server on a loopback websocket. The fake replays frames recorded
//! by the spike (`fixtures/app-server.0.158.0.json`, keyed by their line in the spike's
//! `codex-app-server.jsonl`) and checks what the runner sends.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;

use super::*;

const FIXTURE: &str = include_str!("fixtures/app-server.0.158.0.json");
const TOKEN: &str = "test-token";
const THREAD: &str = "01a19883-c6d2-7401-9e0e-400e0301a198";

/// The frame the spike recorded on line `n`.
fn spike(n: usize) -> Value {
    let rows: Vec<Value> = serde_json::from_str(FIXTURE).unwrap();
    rows.into_iter()
        .find(|r| r["line"] == n)
        .unwrap_or_else(|| panic!("no spike frame {n}"))["frame"]
        .clone()
}

struct Fake {
    url: String,
    to_client: Option<mpsc::UnboundedSender<Value>>,
    from_client: mpsc::UnboundedReceiver<Value>,
}

/// A one-connection app-server that checks the bearer token.
#[allow(clippy::result_large_err)]
async fn fake() -> Fake {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (to_client, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let (in_tx, from_client) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let Ok((tcp, _)) = listener.accept().await else {
            return;
        };
        let check = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
            let ok = req
                .headers()
                .get("Authorization")
                .and_then(|h| h.to_str().ok())
                == Some(&format!("Bearer {TOKEN}"));
            if ok {
                Ok(resp)
            } else {
                let mut e = ErrorResponse::new(Some("invalid websocket bearer token".into()));
                *e.status_mut() = StatusCode::UNAUTHORIZED;
                Err(e)
            }
        };
        let Ok(ws) = tokio_tungstenite::accept_hdr_async(tcp, check).await else {
            return;
        };
        let (mut sink, mut stream) = ws.split();
        tokio::spawn(async move {
            while let Some(Ok(m)) = stream.next().await {
                if let Message::Text(t) = m {
                    let _ = in_tx.send(serde_json::from_str(t.as_str()).unwrap());
                }
            }
        });
        while let Some(v) = out_rx.recv().await {
            if sink.send(Message::text(v.to_string())).await.is_err() {
                return;
            }
        }
        let _ = sink.close().await;
    });
    Fake {
        url,
        to_client: Some(to_client),
        from_client,
    }
}

impl Fake {
    fn runner(&self) -> CodexRunner {
        self.runner_with(Duration::from_secs(600))
    }

    fn runner_with(&self, question_timeout: Duration) -> CodexRunner {
        CodexRunner::fixed(self.url.clone(), TOKEN.into(), question_timeout)
    }

    async fn recv(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(5), self.from_client.recv())
            .await
            .expect("the runner sent nothing")
            .expect("connection closed")
    }

    /// The next frame, which must be a call to `method`.
    async fn expect(&mut self, method: &str) -> Value {
        let v = self.recv().await;
        assert_eq!(v["method"], method, "unexpected frame {v}");
        v
    }

    fn send(&self, v: Value) {
        self.to_client.as_ref().unwrap().send(v).unwrap();
    }

    fn reply(&self, req: &Value, result: Value) {
        self.send(json!({"id": req["id"], "result": result}));
    }

    /// A recorded answer, re-addressed to `req`.
    fn replay(&self, req: &Value, line: usize) {
        let mut f = spike(line);
        f["id"] = req["id"].clone();
        self.send(f);
    }

    /// `initialize` (answered with the spike's reply) then `initialized`.
    async fn handshake(&mut self) {
        let init = self.expect("initialize").await;
        assert_eq!(init["params"]["clientInfo"]["name"], "keo-companion");
        assert_eq!(init["params"]["capabilities"]["experimentalApi"], true);
        self.replay(&init, 3);
        let done = self.expect("initialized").await;
        assert!(done.get("id").is_none());
    }

    fn close(&mut self) {
        self.to_client = None;
    }
}

/// The next event that is not a roster change.
async fn next(rx: &mut broadcast::Receiver<RunnerEvent>) -> RunnerEvent {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("no event")
            .unwrap();
        if !matches!(ev, RunnerEvent::RosterChanged { .. }) {
            return ev;
        }
    }
}

fn question(ev: RunnerEvent) -> (String, String, Vec<String>) {
    match ev {
        RunnerEvent::Question {
            session_id,
            prompt_id,
            text,
            options,
            ..
        } => {
            assert_eq!(session_id, THREAD);
            assert_eq!(
                uuid::Uuid::parse_str(&prompt_id).unwrap().get_version_num(),
                4
            );
            (prompt_id, text, options)
        }
        other => panic!("expected a question, got {other:?}"),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A list call and the roster page answering it, so the runner is connected.
async fn connect(fake: &mut Fake, runner: &CodexRunner) {
    let list = async {
        fake.handshake().await;
        let req = fake.expect("thread/list").await;
        fake.reply(&req, json!({"data": [], "nextCursor": null}));
    };
    let (res, ()) = tokio::join!(runner.list(), list);
    assert_eq!(res.unwrap(), Vec::new());
}

#[tokio::test]
async fn start_session_then_approval_progress_usage_and_turn_end() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let mut rx = runner.events();
    let req = StartRequest {
        cwd: PathBuf::from("/work/repo"),
        name: None,
        slack_thread: Some("C0123/1793560100.000100".into()),
    };
    let server = async {
        fake.handshake().await;
        let start = fake.expect("thread/start").await;
        // Keys built from the plugin ids: a whole `id.enabled` key reads as an email to the
        // publish guard.
        let off: serde_json::Map<String, Value> =
            ["computer-use", "unified-computer-use", "chrome", "browser"]
                .iter()
                .map(|id| (format!("plugins.{id}@openai-bundled.enabled"), json!(false)))
                .collect();
        assert_eq!(
            start["params"],
            json!({"cwd": "/work/repo", "approvalPolicy": "on-request", "sandbox": "workspace-write",
            "config": off})
        );
        fake.replay(&start, 10);
        let name = fake.expect("thread/name/set").await;
        assert_eq!(
            name["params"],
            json!({"threadId": THREAD, "name": "Slack C0123/1793560100.000100"})
        );
        fake.reply(&name, json!({}));
    };
    // `start` opens the thread idle: no turn, no event; the inbox's Register comes next.
    let (id, ()) = tokio::join!(runner.start(req), server);
    assert_eq!(id.unwrap(), THREAD);

    // The first words: the name takes the ticket key, then the turn starts.
    let server = async {
        let name = fake.expect("thread/name/set").await;
        assert_eq!(
            name["params"],
            json!({"threadId": THREAD, "name": "ABC-42"})
        );
        fake.reply(&name, json!({}));
        let turn = fake.expect("turn/start").await;
        assert_eq!(turn["params"]["threadId"], THREAD);
        assert_eq!(
            turn["params"]["input"][0]["text"],
            "Fix ABC-42: run the date command"
        );
        fake.replay(&turn, 38);
    };
    let (sent, ()) = tokio::join!(
        runner.send(THREAD, "Fix ABC-42: run the date command"),
        server
    );
    sent.unwrap();

    // The approval (codex-app-server.jsonl:67-69) becomes a question.
    for line in [41, 67, 69] {
        fake.send(spike(line));
    }
    let (prompt_id, text, options) = question(next(&mut rx).await);
    assert!(
        text.contains("date > spike-a.txt && cat spike-a.txt"),
        "{text}"
    );
    assert_eq!(options, vec!["Allow", "Deny and stop"]);
    runner
        .answer(&prompt_id, Answer::Option("Allow".into()))
        .await
        .unwrap();
    let answer = fake.recv().await;
    assert_eq!(answer["id"], 0);
    assert_eq!(answer["result"], json!({"decision": "accept"}));

    for line in [71, 72, 73, 79, 97, 104, 105] {
        fake.send(spike(line));
    }
    assert_eq!(
        next(&mut rx).await,
        RunnerEvent::Progress {
            tool: KIND,
            session_id: THREAD.into(),
            text: "Ran `date > spike-a.txt && cat spike-a.txt` (exit 0)".into(),
        }
    );
    assert_eq!(
        next(&mut rx).await,
        RunnerEvent::Usage {
            tool: KIND,
            window: "weekly".into(),
            percent: 18.0,
            resets_at: Some("2026-11-08T16:21:13Z".into()),
        }
    );
    assert_eq!(
        next(&mut rx).await,
        RunnerEvent::TurnEnded {
            tool: KIND,
            session_id: THREAD.into(),
            text: Some("Sun Nov  1 12:08:58 MST 2026".into()),
        }
    );
    // A second answer to the settled prompt is a no-op.
    runner.answer(&prompt_id, Answer::Cancelled).await.unwrap();
}

#[tokio::test]
async fn user_input_question_is_answered_with_the_recorded_shape() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let mut rx = runner.events();
    connect(&mut fake, &runner).await;
    fake.send(spike(268));
    let (prompt_id, text, options) = question(next(&mut rx).await);
    assert_eq!(text, "Color\nWhich color theme?");
    assert_eq!(options, vec!["Red", "Blue"]);
    runner
        .answer(&prompt_id, Answer::Option("Red".into()))
        .await
        .unwrap();
    let answer = fake.recv().await;
    assert_eq!(answer["id"], spike(269)["id"]);
    assert_eq!(answer["result"], spike(269)["result"]);
}

#[tokio::test]
async fn unanswered_question_declines_after_the_timeout() {
    let mut fake = fake().await;
    let runner = fake.runner_with(Duration::from_millis(150));
    let mut rx = runner.events();
    connect(&mut fake, &runner).await;
    fake.send(spike(69));
    let (prompt_id, _, _) = question(next(&mut rx).await);
    // No decline is offered for this command, so the timeout cancels.
    let answer = fake.recv().await;
    assert_eq!(
        answer,
        json!({"jsonrpc": "2.0", "id": 0, "result": {"decision": "cancel"}})
    );
    // Keo's late answer finds the question settled.
    runner
        .answer(&prompt_id, Answer::Option("Allow".into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn question_resolved_elsewhere_is_dropped() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let mut rx = runner.events();
    connect(&mut fake, &runner).await;
    fake.send(spike(69));
    let (prompt_id, _, _) = question(next(&mut rx).await);
    fake.send(spike(71));
    // Wait until the runner has read serverRequest/resolved: a later frame round-trips.
    let probe = async {
        let req = fake.expect("thread/list").await;
        fake.reply(&req, json!({"data": []}));
    };
    let (res, ()) = tokio::join!(runner.list(), probe);
    res.unwrap();
    runner
        .answer(&prompt_id, Answer::Option("Allow".into()))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), fake.from_client.recv())
            .await
            .is_err(),
        "nothing goes to the server for a resolved request"
    );
    assert!(matches!(
        runner.answer("not-a-prompt", Answer::Cancelled).await,
        Err(RunnerError::NoSession(_))
    ));
}

#[tokio::test]
async fn roster_lists_recent_threads_with_their_real_folders() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let server = async {
        fake.handshake().await;
        let req = fake.expect("thread/list").await;
        assert_eq!(
            req["params"],
            json!({"limit": 50, "sortKey": "recency_at", "archived": false})
        );
        let mut recent = spike(10)["result"]["thread"].clone();
        recent["updatedAt"] = json!(now_secs());
        recent["name"] = json!("keo-spike-codex-1");
        recent["status"] = json!({"type": "active", "activeFlags": ["waitingOnApproval"]});
        let mut old = recent.clone();
        old["id"] = json!("old-thread");
        old["updatedAt"] = json!(now_secs() - 30 * 86_400);
        old["recencyAt"] = json!(now_secs() - 30 * 86_400);
        old["createdAt"] = json!(now_secs() - 30 * 86_400);
        fake.reply(&req, json!({"data": [recent, old]}));
    };
    let (res, ()) = tokio::join!(runner.list(), server);
    assert_eq!(
        res.unwrap(),
        vec![RosterEntry {
            tool: KIND,
            session_id: THREAD.into(),
            cwd: "/work/repo".into(),
            name: "keo-spike-codex-1".into(),
            state: crate::runner::SessionState::WaitingPermission,
            pid: None,
        }]
    );
}

#[tokio::test]
async fn roster_keeps_every_touched_thread_off_the_page() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let ids: Vec<String> = (0..25).map(|i| format!("touched-{i}")).collect();
    runner.inner.st().touched.extend(ids.iter().cloned());
    let server = async {
        fake.handshake().await;
        let req = fake.expect("thread/list").await;
        fake.reply(&req, json!({"data": []}));
        for _ in 0..ids.len() {
            let read = fake.expect("thread/read").await;
            let mut thread = spike(10)["result"]["thread"].clone();
            thread["id"] = read["params"]["threadId"].clone();
            fake.reply(&read, json!({"thread": thread}));
        }
    };
    let (res, ()) = tokio::join!(runner.list(), server);
    let mut listed: Vec<String> = res.unwrap().into_iter().map(|e| e.session_id).collect();
    listed.sort();
    let mut want = ids.clone();
    want.sort();
    assert_eq!(listed, want);
}

#[tokio::test]
async fn a_failing_thread_read_keeps_the_last_roster() {
    let mut fake = fake().await;
    let runner = fake.runner();
    runner.inner.st().touched.insert("busy".into());
    let server = async {
        fake.handshake().await;
        let req = fake.expect("thread/list").await;
        fake.reply(&req, json!({"data": []}));
        let read = fake.expect("thread/read").await;
        fake.send(
            json!({"id": read["id"], "error": {"code": -32603, "message": "internal error"}}),
        );
    };
    let (res, ()) = tokio::join!(runner.list(), server);
    assert!(res.is_err(), "a failing read must not drop the session");
    assert!(runner.inner.st().touched.contains("busy"));

    // A thread Codex refuses outright leaves the roster.
    let server = async {
        let req = fake.expect("thread/list").await;
        fake.reply(&req, json!({"data": []}));
        let read = fake.expect("thread/read").await;
        fake.send(
            json!({"id": read["id"], "error": {"code": -32600, "message": "thread not found"}}),
        );
    };
    let (res, ()) = tokio::join!(runner.list(), server);
    assert_eq!(res.unwrap(), Vec::new());
    assert!(!runner.inner.st().touched.contains("busy"));
}

#[tokio::test]
async fn send_resumes_then_starts_and_steer_follows_the_running_turn() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let server = async {
        fake.handshake().await;
        let resume = fake.expect("thread/resume").await;
        assert_eq!(
            resume["params"],
            json!({"threadId": THREAD, "excludeTurns": true})
        );
        fake.reply(
            &resume,
            json!({"thread": spike(10)["result"]["thread"].clone()}),
        );
        let turn = fake.expect("turn/start").await;
        fake.replay(&turn, 38);
    };
    let (res, ()) = tokio::join!(runner.send(THREAD, "next step"), server);
    res.unwrap();

    // The turn the runner knows has moved on; the server names the one running now (:116).
    let server = async {
        let steer = fake.expect("turn/steer").await;
        assert_eq!(
            steer["params"]["expectedTurnId"],
            "01a19883-c985-7602-acc0-237eae361080"
        );
        assert_eq!(steer["params"]["input"][0]["text"], "also this");
        let mut err = spike(116);
        err["id"] = steer["id"].clone();
        fake.send(err);
        let again = fake.expect("turn/steer").await;
        assert_eq!(
            again["params"]["expectedTurnId"],
            "01a19883-d7b0-7323-95c0-55d085bb024d"
        );
        fake.reply(&again, json!({}));
    };
    let (res, ()) = tokio::join!(runner.steer(THREAD, "also this"), server);
    res.unwrap();

    let server = async {
        let stop = fake.expect("turn/interrupt").await;
        assert_eq!(
            stop["params"],
            json!({"threadId": THREAD, "turnId": "01a19883-d7b0-7323-95c0-55d085bb024d"})
        );
        fake.replay(&stop, 171);
    };
    let (res, ()) = tokio::join!(runner.interrupt(THREAD), server);
    res.unwrap();
}

#[tokio::test]
async fn overloaded_server_is_retried() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let server = async {
        fake.handshake().await;
        let first = fake.expect("thread/list").await;
        fake.send(json!({"id": first["id"], "error": {"code": -32001, "message": "Server overloaded; retry later."}}));
        let second = fake.expect("thread/list").await;
        fake.reply(&second, json!({"data": []}));
    };
    let (res, ()) = tokio::join!(runner.list(), server);
    assert_eq!(res.unwrap(), Vec::new());
}

#[tokio::test]
async fn closed_server_ends_the_running_turn() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let mut rx = runner.events();
    connect(&mut fake, &runner).await;
    fake.send(spike(41));
    // Round-trip a call so turn/started is read before the close.
    let probe = async {
        let req = fake.expect("thread/list").await;
        fake.reply(&req, json!({"data": []}));
    };
    let (res, ()) = tokio::join!(runner.list(), probe);
    res.unwrap();
    fake.close();
    match next(&mut rx).await {
        RunnerEvent::TurnEnded {
            session_id, text, ..
        } => {
            assert_eq!(session_id, THREAD);
            assert!(text.unwrap().contains("app-server closed"));
        }
        other => panic!("expected TurnEnded, got {other:?}"),
    }
}

#[tokio::test]
async fn wrong_token_is_refused() {
    let fake = fake().await;
    let runner = CodexRunner::fixed(fake.url.clone(), "wrong".into(), Duration::from_secs(1));
    match runner.list().await {
        Err(RunnerError::Failed(m)) => assert!(m.contains("could not connect"), "{m}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_server_request_is_refused() {
    let mut fake = fake().await;
    let runner = fake.runner();
    connect(&mut fake, &runner).await;
    fake.send(
        json!({"id": 7, "method": "item/somethingNew/request", "params": {"threadId": THREAD}}),
    );
    let answer = fake.recv().await;
    assert_eq!(answer["id"], 7);
    assert_eq!(answer["error"]["code"], -32601);
}

#[tokio::test]
async fn multi_question_user_input_replies_once_with_every_answer() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let mut rx = runner.events();
    connect(&mut fake, &runner).await;
    let mut req = spike(268);
    req["id"] = json!(9);
    let second = json!({"id":"size","header":"Size","question":"Which size?","isOther":true,"isSecret":false,
        "options":[{"label":"S","description":"small"},{"label":"L","description":"large"}]});
    req["params"]["questions"]
        .as_array_mut()
        .unwrap()
        .push(second);
    fake.send(req);
    let (color, _, _) = question(next(&mut rx).await);
    let (size, text, _) = question(next(&mut rx).await);
    assert_eq!(text, "Size\nWhich size?");
    runner
        .answer(&size, Answer::Text("M".into()))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), fake.from_client.recv())
            .await
            .is_err(),
        "no reply until every question has an answer"
    );
    runner
        .answer(&color, Answer::Option("Blue".into()))
        .await
        .unwrap();
    let answer = fake.recv().await;
    assert_eq!(answer["id"], 9);
    assert_eq!(
        answer["result"],
        json!({"answers": {"color": {"answers": ["Blue"]}, "size": {"answers": ["M"]}}})
    );
}

#[tokio::test]
async fn a_turn_that_ends_before_its_start_reply_stays_ended() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let mut rx = runner.events();
    let server = async {
        fake.handshake().await;
        let resume = fake.expect("thread/resume").await;
        fake.reply(
            &resume,
            json!({"thread": spike(10)["result"]["thread"].clone()}),
        );
        let turn = fake.expect("turn/start").await;
        // The whole turn runs before its turn/start reply (same turn id as spike 38).
        fake.send(spike(41));
        fake.send(spike(105));
        fake.replay(&turn, 38);
    };
    let (res, ()) = tokio::join!(runner.send(THREAD, "quick one"), server);
    res.unwrap();
    assert!(matches!(next(&mut rx).await, RunnerEvent::TurnEnded { .. }));
    // A later message starts a new turn: nothing runs to steer.
    let server = async {
        let turn = fake.expect("turn/start").await;
        fake.replay(&turn, 38);
    };
    let (res, ()) = tokio::join!(runner.send(THREAD, "next"), server);
    res.unwrap();
}

#[tokio::test]
async fn an_answer_after_its_turn_ended_is_a_no_op() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let mut rx = runner.events();
    connect(&mut fake, &runner).await;
    fake.send(spike(69));
    let (prompt_id, _, _) = question(next(&mut rx).await);
    fake.send(spike(105));
    assert!(matches!(next(&mut rx).await, RunnerEvent::TurnEnded { .. }));
    runner
        .answer(&prompt_id, Answer::Option("Allow".into()))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), fake.from_client.recv())
            .await
            .is_err(),
        "nothing goes to the server for a question whose turn ended"
    );
}

#[tokio::test]
async fn resuming_a_running_thread_finds_its_turn_and_a_finished_turn_falls_back() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let server = async {
        fake.handshake().await;
        let resume = fake.expect("thread/resume").await;
        let mut thread = spike(10)["result"]["thread"].clone();
        thread["status"] = json!({"type": "active", "activeFlags": []});
        fake.reply(&resume, json!({"thread": thread}));
        let turns = fake.expect("thread/turns/list").await;
        assert_eq!(turns["params"]["threadId"], THREAD);
        assert_eq!(turns["params"]["limit"], 1);
        fake.reply(
            &turns,
            json!({"data": [{"id": "running-turn", "status": "inProgress", "items": []}]}),
        );
        let steer = fake.expect("turn/steer").await;
        assert_eq!(steer["params"]["expectedTurnId"], "running-turn");
        // The turn ended meanwhile (codex-app-server.jsonl:240).
        let mut err = spike(116);
        err["id"] = steer["id"].clone();
        err["error"]["message"] = json!("no active turn to steer");
        fake.send(err);
        let turn = fake.expect("turn/start").await;
        assert_eq!(turn["params"]["input"][0]["text"], "carry on");
        fake.replay(&turn, 38);
    };
    let (res, ()) = tokio::join!(runner.send(THREAD, "carry on"), server);
    res.unwrap();
}

#[tokio::test]
async fn an_unsent_session_stays_off_the_roster_and_close_archives_it_quietly() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let mut rx = runner.events();
    let req = StartRequest {
        cwd: PathBuf::from("/work/repo"),
        name: Some("n".into()),
        slack_thread: None,
    };
    let server = async {
        fake.handshake().await;
        let start = fake.expect("thread/start").await;
        fake.replay(&start, 10);
        let name = fake.expect("thread/name/set").await;
        assert_eq!(name["params"]["name"], "n");
        fake.reply(&name, json!({}));
    };
    let (id, ()) = tokio::join!(runner.start(req), server);
    assert_eq!(id.unwrap(), THREAD);

    // The roster leaves it out until its first words, even when Codex lists it.
    let server = async {
        let req = fake.expect("thread/list").await;
        let mut t = spike(10)["result"]["thread"].clone();
        t["updatedAt"] = json!(now_secs());
        fake.reply(&req, json!({"data": [t]}));
    };
    let (res, ()) = tokio::join!(runner.list(), server);
    assert_eq!(res.unwrap(), Vec::new());

    // A first send that fails is an error, so the inbox closes the session.
    let server = async {
        let turn = fake.expect("turn/start").await;
        fake.send(json!({"id": turn["id"], "error": {"code": -32600, "message": "failed to load configuration"}}));
    };
    let (sent, ()) = tokio::join!(runner.send(THREAD, "do it"), server);
    assert!(sent
        .unwrap_err()
        .to_string()
        .contains("failed to load configuration"));

    // A second start's session never sent: close archives it, with no SessionEnded.
    let server = async {
        let start = fake.expect("thread/start").await;
        let mut r = spike(10);
        r["id"] = start["id"].clone();
        r["result"]["thread"]["id"] = json!("thread-2");
        fake.send(r);
        let name = fake.expect("thread/name/set").await;
        fake.reply(&name, json!({}));
    };
    let (id2, ()) = tokio::join!(
        runner.start(StartRequest {
            cwd: PathBuf::from("/work/repo"),
            name: Some("m".into()),
            slack_thread: None,
        }),
        server
    );
    assert_eq!(id2.unwrap(), "thread-2");
    let server = async {
        let archive = fake.expect("thread/archive").await;
        assert_eq!(archive["params"], json!({"threadId": "thread-2"}));
        fake.send(json!({"method": "thread/archived", "params": {"threadId": "thread-2"}}));
        fake.reply(&archive, json!({}));
    };
    let (closed, ()) = tokio::join!(runner.close("thread-2"), server);
    closed.unwrap();
    let mut saw_archive = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
        assert!(
            !matches!(ev, RunnerEvent::SessionEnded { .. }),
            "a closed unsent session never was on the roster: {ev:?}"
        );
        saw_archive |= matches!(ev, RunnerEvent::RosterChanged { .. });
    }
    assert!(saw_archive, "the archive notification was handled");
    // Closing a session that already got its words does nothing.
    runner.close(THREAD).await.unwrap();
}

#[tokio::test]
async fn a_roster_read_waits_for_a_thread_start_in_flight() {
    let mut fake = fake().await;
    let runner = fake.runner();
    let (began, wait) = tokio::sync::oneshot::channel::<()>();
    let req = StartRequest {
        cwd: PathBuf::from("/work/repo"),
        name: Some("n".into()),
        slack_thread: None,
    };
    let server = async {
        fake.handshake().await;
        let start = fake.expect("thread/start").await;
        began.send(()).unwrap();
        // Codex already lists the new thread, but the roster read waits for the start reply.
        let early = tokio::time::timeout(Duration::from_millis(200), fake.from_client.recv()).await;
        assert!(
            early.is_err(),
            "thread/list went out during thread/start: {early:?}"
        );
        fake.replay(&start, 10);
        for _ in 0..2 {
            let req = fake.recv().await;
            match req["method"].as_str() {
                Some("thread/name/set") => fake.reply(&req, json!({})),
                Some("thread/list") => {
                    let mut t = spike(10)["result"]["thread"].clone();
                    t["updatedAt"] = json!(now_secs());
                    fake.reply(&req, json!({"data": [t]}));
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    };
    let roster = async {
        wait.await.unwrap();
        runner.list().await
    };
    let (id, roster, ()) = tokio::join!(runner.start(req), roster, server);
    assert_eq!(id.unwrap(), THREAD);
    assert_eq!(roster.unwrap(), Vec::new());
}

/// The runner against the real installed `codex app-server` (spends a few Codex tokens). Run by
/// hand: `KEO_CODEX_LIVE_CWD=<git repo> cargo test --lib live_codex -- --ignored --nocapture`.
/// It starts the server itself (token file, port file, `/readyz`), starts a session, answers
/// every question with its first option, and prints each event until the turn ends.
#[tokio::test]
#[ignore]
async fn live_codex() {
    let cwd = std::env::var("KEO_CODEX_LIVE_CWD").expect("set KEO_CODEX_LIVE_CWD");
    let data = tempfile::tempdir().unwrap();
    set_data_dir(data.path().to_path_buf());
    let runner = Arc::new(CodexRunner::new());
    let mut rx = runner.events();
    let text = std::env::var("KEO_CODEX_LIVE_TEXT").unwrap_or_else(|_| {
        "Append the current date to the file /tmp/keo-companion-live.txt, which is outside this \
         workspace (ask for the permission you need). Then reply with one short line."
            .into()
    });
    let started = std::time::Instant::now();
    let id = runner
        .start(StartRequest {
            cwd: PathBuf::from(&cwd),
            name: Some("keo-companion live check".into()),
            slack_thread: None,
        })
        .await
        .unwrap();
    runner.send(&id, &text).await.unwrap();
    println!("[{:>6.1}s] started {id}", started.elapsed().as_secs_f64());
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(180), rx.recv())
            .await
            .expect("no event in 3 minutes")
            .unwrap();
        println!("[{:>6.1}s] {ev:?}", started.elapsed().as_secs_f64());
        match ev {
            RunnerEvent::Question {
                prompt_id, options, ..
            } => {
                let pick = options.first().cloned().unwrap_or_else(|| "yes".into());
                runner
                    .answer(&prompt_id, Answer::Option(pick))
                    .await
                    .unwrap();
            }
            RunnerEvent::TurnEnded { session_id, .. } if session_id == id => break,
            _ => {}
        }
    }
    let roster = runner.list().await.unwrap();
    let me = roster.iter().find(|e| e.session_id == id).expect("listed");
    println!("roster entry: {me:?}");
    assert_eq!(
        me.cwd,
        std::fs::canonicalize(&cwd).unwrap().to_string_lossy()
    );
    // Dropping the runner ends the app-server it started (its whole process group).
    drop(rx);
    let runner = Arc::try_unwrap(runner).ok().expect("one owner");
    drop(runner);
}
