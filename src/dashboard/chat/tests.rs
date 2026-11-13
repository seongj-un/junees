use super::runner::{ChatAsk, Shared, TextOut};
use super::*;
use crate::approval::{Ask, Request};

fn settings() -> Settings {
    Settings::new(
        |_: &str| None,
        crate::config_file::ConfigFile::default(),
        None,
        false,
    )
}

fn shared() -> Shared {
    Shared {
        dir: PathBuf::from("/w"),
        home: None,
        model: "m".into(),
        settings: Arc::new(settings()),
        approval_time: Duration::from_millis(200),
        running: Arc::default(),
    }
}

fn types(room: &Room) -> Vec<Value> {
    room.after(0)
        .into_iter()
        .map(|(_, data)| serde_json::from_str(&data).unwrap())
        .collect()
}

#[test]
fn routes_are_read_strictly() {
    assert_eq!(route("/api/chat"), Some(Route::Open));
    assert_eq!(route("/api/chat/a1/messages"), Some(Route::Messages("a1")));
    assert_eq!(route("/api/chat/a1/events"), Some(Route::Events("a1")));
    assert_eq!(route("/api/chat/a1/cancel"), Some(Route::Cancel("a1")));
    assert_eq!(
        route("/api/chat/a1/approvals/q9"),
        Some(Route::Approval("a1", "q9"))
    );
    for path in [
        "/api/chat/",
        "/api/chat/a1",
        "/api/chat/a1/messages/x",
        "/api/chat/a1/approvals",
        "/api/chats",
        "/api/chat/a1/approvals/q/x",
    ] {
        assert_eq!(route(path), None, "{path}");
    }
    assert!(Route::Open.writes() && !Route::Events("a").writes());
    assert!(is_chat_path("/api/chat/x/y/z") && !is_chat_path("/api/chats"));
    assert_eq!(body_limit("/api/chat/a/messages"), Some(MAX_MESSAGE_BODY));
    assert_eq!(body_limit("/api/chat"), None);
}

#[test]
fn events_are_numbered_and_only_the_last_are_kept() {
    let room = Room::new("r".into());
    for n in 0..MAX_EVENTS + 5 {
        assert_eq!(
            room.push(json!({ "type": "text", "delta": n })),
            n as u64 + 1
        );
    }
    let kept = room.after(0);
    assert_eq!(kept.len(), MAX_EVENTS);
    assert_eq!(kept[0].0, 6);
    let tail = room.after(MAX_EVENTS as u64 + 3);
    assert_eq!(
        tail.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        [1004, 1005]
    );
    assert!(room.after(1005).is_empty());
    assert_eq!(*room.seq.borrow(), 1005);
}

#[tokio::test]
async fn a_question_closes_once() {
    let room = Room::new("r".into());
    let answer = room.ask("q1", true);
    assert_eq!(room.offers_always("q1"), Some(true));
    let reply = room.close("q1", "y", "browser").unwrap().unwrap();
    reply.send(Approval::Yes).unwrap();
    assert_eq!(answer.await.unwrap(), Approval::Yes);
    assert_eq!(
        room.close("q1", "n", "timeout").unwrap_err(),
        Closing::Closed
    );
    assert_eq!(
        room.close("q2", "n", "browser").unwrap_err(),
        Closing::Missing
    );
    assert_eq!(room.offers_always("q2"), None);
    let events = types(&room);
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        json!({"type": "approval_closed", "question_id": "q1", "answer": "y", "by": "browser"})
    );
}

#[test]
fn old_closed_questions_are_forgotten_open_ones_kept() {
    let room = Room::new("r".into());
    let _open = room.ask("open", false);
    for n in 0..MAX_QUESTIONS + 10 {
        let id = format!("q{n}");
        let _answer = room.ask(&id, false);
        let _ = room.close(&id, "n", "browser");
    }
    let questions = room.questions.lock().unwrap();
    assert!(questions.open.len() <= MAX_QUESTIONS + 1);
    assert!(questions.open.contains_key("open"));
}

#[test]
fn text_goes_out_in_whole_words_with_secrets_hidden() {
    let shared = shared();
    let room = Room::new("r".into());
    let mut out = TextOut::default();
    let key = format!("sk-ant-api03-{}", "a".repeat(40));
    // The key arrives in pieces; no event may hold a part of it.
    let text = format!("Here is the key {key} and some more words to pass the chunk size.\nDone");
    for piece in text.as_bytes().chunks(5) {
        out.push(&shared, &room, std::str::from_utf8(piece).unwrap());
    }
    out.flush(&shared, &room);
    let deltas: Vec<String> = types(&room)
        .iter()
        .map(|event| event["delta"].as_str().unwrap().to_string())
        .collect();
    assert!(deltas.len() >= 2, "{deltas:?}");
    let joined = deltas.concat();
    assert!(!joined.contains("sk-ant-api03"), "{joined}");
    assert!(
        joined.starts_with("Here is the key ") && joined.ends_with("Done"),
        "{joined}"
    );
    for delta in &deltas {
        assert!(!delta.contains("aaaaaaaaaa"), "{delta}");
    }
}

#[test]
fn hangul_is_cut_on_character_boundaries() {
    let shared = shared();
    let room = Room::new("r".into());
    let mut out = TextOut::default();
    let text = "안녕하세요 반갑습니다 ".repeat(20);
    for piece in text.chars().collect::<Vec<_>>().chunks(3) {
        out.push(&shared, &room, &piece.iter().collect::<String>());
    }
    out.flush(&shared, &room);
    let joined: String = types(&room)
        .iter()
        .map(|event| event["delta"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(joined, text);
}

fn request(command: &str, kinds: &[&str]) -> Request {
    Request {
        tool: "bash".into(),
        target: command.into(),
        reasons: vec!["deletes files".into()],
        preview: None,
        kinds: kinds.iter().map(|kind| kind.to_string()).collect(),
    }
}

#[tokio::test]
async fn an_unanswered_question_is_declined_by_timeout() {
    let room = Arc::new(Room::new("r".into()));
    let mut ask = ChatAsk::new(&room, &shared());
    let answer = ask.ask(&request("rm notes.txt", &["bash: rm"])).await;
    assert!(
        matches!(answer, Approval::NoBecause(ref why) if why.contains("no answer")),
        "{answer:?}"
    );
    let events = types(&room);
    assert_eq!(events[0]["type"], "approval");
    assert_eq!(events[0]["kind"], "bash");
    assert_eq!(events[0]["options"], json!(["y", "a", "n"]));
    assert!(
        events[0]["summary"]
            .as_str()
            .unwrap()
            .contains("rm notes.txt")
    );
    let id = events[0]["question_id"].as_str().unwrap();
    assert_eq!(
        events[1],
        json!({"type": "approval_closed", "question_id": id, "answer": "n", "by": "timeout"})
    );
    assert_eq!(room.close(id, "y", "browser").unwrap_err(), Closing::Closed);
}

#[tokio::test]
async fn a_question_asked_every_time_offers_no_always_and_a_cancel_closes_it() {
    let room = Arc::new(Room::new("r".into()));
    let mut ask = ChatAsk::new(&room, &shared());
    let kinds = [format!(
        "github: merge {}",
        crate::approval::ASKED_EVERY_TIME
    )];
    let kinds: Vec<&str> = kinds.iter().map(String::as_str).collect();
    let canceller = {
        let room = Arc::clone(&room);
        tokio::spawn(async move {
            while room.after(0).is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            room.cancel.cancel();
        })
    };
    let answer = ask.ask(&request("merge 7", &kinds)).await;
    canceller.await.unwrap();
    assert!(
        matches!(answer, Approval::NoBecause(ref why) if why.contains("cancelled")),
        "{answer:?}"
    );
    let events = types(&room);
    assert_eq!(events[0]["options"], json!(["y", "n"]));
    assert_eq!(events[1]["by"], "cancel");
}

#[tokio::test]
async fn the_page_answers_a_question() {
    let room = Arc::new(Room::new("r".into()));
    let mut ask = ChatAsk::new(&room, &shared());
    let answerer = {
        let room = Arc::clone(&room);
        tokio::spawn(async move {
            loop {
                if let Some((_, data)) = room.after(0).first() {
                    let event: Value = serde_json::from_str(data).unwrap();
                    let id = event["question_id"].as_str().unwrap();
                    let reply = room.close(id, "a", "browser").unwrap().unwrap();
                    reply.send(Approval::Always).unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };
    let answer = ask.ask(&request("rm notes.txt", &["bash: rm"])).await;
    answerer.await.unwrap();
    assert_eq!(answer, Approval::Always);
    assert_eq!(types(&room)[1]["by"], "browser");
}
