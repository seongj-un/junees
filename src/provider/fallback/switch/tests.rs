use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::*;
use crate::message::{ContentBlock, Message, StopReason, Usage};

/// Answers with what it was given, in order, and counts its calls.
struct Scripted {
    name: &'static str,
    results: Mutex<VecDeque<Result<CompletionResponse, ProviderError>>>,
    calls: Arc<Mutex<usize>>,
    /// Text reported before each failure, as a stream broken off midway.
    partial: bool,
}

impl Scripted {
    fn boxed(
        name: &'static str,
        results: Vec<Result<CompletionResponse, ProviderError>>,
    ) -> (Box<dyn Provider>, Arc<Mutex<usize>>) {
        let calls = Arc::new(Mutex::new(0));
        let provider = Self {
            name,
            results: Mutex::new(results.into()),
            calls: calls.clone(),
            partial: false,
        };
        (Box::new(provider), calls)
    }
}

#[async_trait]
impl Provider for Scripted {
    async fn complete(
        &self,
        _request: CompletionRequest<'_>,
    ) -> Result<CompletionResponse, ProviderError> {
        *self.calls.lock().unwrap() += 1;
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(text(self.name)))
    }

    async fn complete_streaming(
        &self,
        request: CompletionRequest<'_>,
        events: &StreamSender,
    ) -> Result<CompletionResponse, ProviderError> {
        if self.partial {
            let _ = events.send(StreamEvent::TextDelta("half".into()));
        }
        let response = self.complete(request).await?;
        crate::provider::report_blocks(&response.content, events);
        Ok(response)
    }

    fn model(&self) -> Option<&str> {
        Some(self.name)
    }
}

fn text(text: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::text(text)],
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

fn api(status: u16, retry_after: Option<u64>) -> Result<CompletionResponse, ProviderError> {
    Err(ProviderError::Api {
        status,
        kind: None,
        message: format!("status {status}"),
        retry_after,
    })
}

const AFTER: Duration = Duration::from_secs(120);

/// One streamed call; its reply text and the events other than text.
async fn ask(provider: &Switching) -> (Result<String, ProviderError>, Vec<StreamEvent>) {
    let messages = [Message::user_text("hi")];
    let (sender, mut received) = tokio::sync::mpsc::unbounded_channel();
    let result = provider
        .complete_streaming(
            CompletionRequest {
                system: "",
                messages: &messages,
                tools: &[],
                max_tokens: 10,
                cache: false,
                thinking: false,
                web_search: false,
            },
            &sender,
        )
        .await
        .map(|response| match &response.content[0] {
            ContentBlock::Text { text, .. } => text.clone(),
            other => panic!("{other:?}"),
        });
    let mut events = Vec::new();
    while let Ok(event) = received.try_recv() {
        if !matches!(event, StreamEvent::TextDelta(_) | StreamEvent::TextEnd) {
            events.push(event);
        }
    }
    (result, events)
}

#[tokio::test(start_paused = true)]
async fn a_blocked_main_model_hands_the_turn_to_the_fallback_until_its_time() {
    let (main, main_calls) = Scripted::boxed("big", vec![api(429, Some(900))]);
    let (fallback, fallback_calls) = Scripted::boxed("small", vec![]);
    let provider = Switching::new(main, Some(fallback), AFTER, None);

    provider.begin_turn();
    let (reply, events) = ask(&provider).await;
    assert_eq!(reply.unwrap(), "small");
    let [StreamEvent::ModelSwitch(switch)] = &events[..] else {
        panic!("{events:?}");
    };
    assert_eq!((switch.from.as_str(), switch.to.as_str()), ("big", "small"));
    assert_eq!(switch.reason, Reason::RateLimit);
    assert_eq!(switch.retry_in, Duration::from_secs(900));
    assert_eq!(
        switch.line(),
        "big 모델이 사용 한도로 막혀 small 모델로 이어 갑니다 (다시 시도: 15분 뒤)"
    );
    // Usage is stamped with the model that answered.
    assert_eq!(provider.model(), Some("small"));

    // The rest of the turn, and the next turns before the time, stay on
    // the fallback.
    assert_eq!(ask(&provider).await.0.unwrap(), "small");
    tokio::time::advance(Duration::from_secs(600)).await;
    provider.begin_turn();
    assert_eq!(ask(&provider).await.0.unwrap(), "small");
    assert_eq!(*main_calls.lock().unwrap(), 1);
    assert_eq!(*fallback_calls.lock().unwrap(), 3);

    // After it, the next turn tries the main model first again.
    tokio::time::advance(Duration::from_secs(301)).await;
    assert_eq!(ask(&provider).await.0.unwrap(), "small", "same turn");
    provider.begin_turn();
    let (reply, events) = ask(&provider).await;
    assert_eq!(reply.unwrap(), "big");
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(provider.model(), Some("big"));
}

#[tokio::test(start_paused = true)]
async fn errors_another_model_would_get_too_are_not_handed_over() {
    for status in [400, 401, 403, 404, 413] {
        let (main, _) = Scripted::boxed("big", vec![api(status, None)]);
        let (fallback, fallback_calls) = Scripted::boxed("small", vec![]);
        let provider = Switching::new(main, Some(fallback), AFTER, None);
        let (reply, events) = ask(&provider).await;
        assert!(matches!(reply, Err(ProviderError::Api { status: s, .. }) if s == status));
        assert!(events.is_empty());
        assert_eq!(*fallback_calls.lock().unwrap(), 0, "{status}");
    }
    // Overloaded, a server error or no answer at all is.
    for (error, reason) in [
        (api(529, None), Reason::Overloaded),
        (api(503, None), Reason::Unavailable),
        (
            Err(ProviderError::Transport("connection refused".into())),
            Reason::Unavailable,
        ),
    ] {
        let (main, _) = Scripted::boxed("big", vec![error]);
        let (fallback, _) = Scripted::boxed("small", vec![]);
        let provider = Switching::new(main, Some(fallback), AFTER, None);
        let (reply, events) = ask(&provider).await;
        assert_eq!(reply.unwrap(), "small");
        let [StreamEvent::ModelSwitch(switch)] = &events[..] else {
            panic!("{events:?}");
        };
        assert_eq!(switch.reason, reason);
        // No retry-after: the main model is skipped for fallback_after.
        assert_eq!(switch.retry_in, AFTER);
    }
}

#[tokio::test(start_paused = true)]
async fn a_reply_shown_in_part_is_void_when_the_fallback_takes_over() {
    let calls = Arc::new(Mutex::new(0));
    let main = Box::new(Scripted {
        name: "big",
        results: Mutex::new(vec![api(529, None)].into()),
        calls: calls.clone(),
        partial: true,
    });
    let (fallback, _) = Scripted::boxed("small", vec![]);
    let provider = Switching::new(main, Some(fallback), AFTER, None);
    let (reply, events) = ask(&provider).await;
    assert_eq!(reply.unwrap(), "small");
    assert!(
        matches!(
            &events[..],
            [StreamEvent::Retrying, StreamEvent::ModelSwitch(_)]
        ),
        "{events:?}"
    );
}

fn wait(limit: u64, deadline: Option<Duration>) -> Option<Wait> {
    Some(Wait {
        limit: Duration::from_secs(limit),
        deadline: deadline.map(|after| Instant::now() + after),
    })
}

#[tokio::test(start_paused = true)]
async fn unattended_runs_wait_for_a_limit_to_lift() {
    let (main, calls) = Scripted::boxed("big", vec![api(429, Some(2)), api(429, Some(3))]);
    let provider = Switching::new(main, None, AFTER, wait(1800, None));
    let start = Instant::now();
    let (reply, events) = ask(&provider).await;
    assert_eq!(reply.unwrap(), "big");
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    assert_eq!(*calls.lock().unwrap(), 3);
    let notices: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Notice(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        [
            "모델이 사용 한도로 막혀 2초 기다렸다가 다시 보냅니다",
            "모델이 사용 한도로 막혀 3초 기다렸다가 다시 보냅니다"
        ]
    );

    // The fallback blocked as well: the wait, then the main model first.
    let (main, main_calls) = Scripted::boxed("big", vec![api(429, Some(60))]);
    let (fallback, _) = Scripted::boxed("small", vec![api(429, Some(90))]);
    let provider = Switching::new(main, Some(fallback), AFTER, wait(1800, None));
    let (reply, _) = ask(&provider).await;
    assert_eq!(reply.unwrap(), "big");
    assert_eq!(*main_calls.lock().unwrap(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_wait_past_the_limit_or_the_run_time_is_not_started() {
    // Over wait_limit, counting the waits already made.
    let (main, calls) = Scripted::boxed("big", vec![api(429, Some(20)), api(429, Some(20))]);
    let provider = Switching::new(main, None, AFTER, wait(30, None));
    let start = Instant::now();
    let (reply, events) = ask(&provider).await;
    assert!(matches!(reply, Err(ProviderError::Api { status: 429, .. })));
    assert_eq!(start.elapsed(), Duration::from_secs(20));
    assert_eq!(*calls.lock().unwrap(), 2);
    assert!(
        matches!(events.last(), Some(StreamEvent::Notice(text)) if text.contains("wait_limit 30초")),
        "{events:?}"
    );

    // Past the run's time limit.
    let (main, _) = Scripted::boxed("big", vec![api(429, Some(600))]);
    let provider = Switching::new(
        main,
        None,
        AFTER,
        wait(1800, Some(Duration::from_secs(300))),
    );
    let start = Instant::now();
    let (reply, events) = ask(&provider).await;
    assert!(reply.is_err());
    assert_eq!(start.elapsed(), Duration::ZERO);
    assert!(
        matches!(events.last(), Some(StreamEvent::Notice(text)) if text.contains("실행 시간 상한")),
        "{events:?}"
    );

    // No wait at all for a person, or with wait_limit 0.
    for wait in [None, wait(0, None)] {
        let (main, calls) = Scripted::boxed("big", vec![api(429, Some(2))]);
        let provider = Switching::new(main, None, AFTER, wait);
        assert!(ask(&provider).await.0.is_err());
        assert_eq!(*calls.lock().unwrap(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_run_stops_waiting() {
    let (main, calls) = Scripted::boxed("big", vec![api(429, Some(600))]);
    let provider = Switching::new(main, None, AFTER, wait(1800, None));
    // Dropping the call (as a cancel does) ends the wait.
    let cancelled = tokio::time::timeout(Duration::from_secs(5), ask(&provider)).await;
    assert!(cancelled.is_err());
    tokio::time::advance(Duration::from_secs(900)).await;
    assert_eq!(*calls.lock().unwrap(), 1);
}
