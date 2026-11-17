//! The provider that switches to the fallback model while the main one is
//! blocked, and (in unattended runs) waits for a usage limit to lift.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
// Tokio's clock, so tests can let time pass at once.
use tokio::time::Instant;

use crate::provider::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, StreamEvent, StreamSender,
};

/// Why a model is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// 429: a rate or usage limit.
    RateLimit,
    /// 529: the API is overloaded.
    Overloaded,
    /// A 5xx or no answer, after the retries.
    Unavailable,
}

impl Reason {
    /// Whether `error` means the model is blocked for now, and why. Not
    /// for errors another model would get as well, or that are about the
    /// credentials (400, 401, 403, 404, 413): those stay errors.
    pub fn of(error: &ProviderError) -> Option<Self> {
        match error {
            ProviderError::Api { status: 429, .. } => Some(Self::RateLimit),
            ProviderError::Api { status: 529, .. } => Some(Self::Overloaded),
            ProviderError::Api { status, .. } if *status >= 500 => Some(Self::Unavailable),
            ProviderError::Transport(_) => Some(Self::Unavailable),
            _ => None,
        }
    }

    /// The name in the dashboard's `model_switch` event.
    pub fn code(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::Overloaded => "overloaded",
            Self::Unavailable => "unavailable",
        }
    }

    /// For people: "사용 한도로 막혀". [`Self::Unavailable`] covers both a
    /// 5xx answer and no answer at all, so its words say both.
    pub fn korean(self) -> &'static str {
        match self {
            Self::RateLimit => "사용 한도로",
            Self::Overloaded => "과부하로",
            Self::Unavailable => "서버 오류나 연결 실패로",
        }
    }
}

/// The calls moved from the main model to the fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct Switch {
    pub from: String,
    pub to: String,
    pub reason: Reason,
    /// When the main model is tried again (at the first call of a later
    /// turn after this time).
    pub retry_at: SystemTime,
    /// The same, from now.
    pub retry_in: Duration,
}

impl Switch {
    /// The line for the terminal and the gateways.
    pub fn line(&self) -> String {
        format!(
            "{} 모델이 {} 막혀 {} 모델로 이어 갑니다 (다시 시도: {} 뒤)",
            self.from,
            self.reason.korean(),
            self.to,
            crate::korean::duration(round_up(self.retry_in)),
        )
    }
}

/// `duration` rounded up to whole seconds, so "0초 뒤" is never shown.
fn round_up(duration: Duration) -> Duration {
    Duration::from_secs(duration.as_secs() + u64::from(duration.subsec_nanos() > 0))
        .max(Duration::from_secs(1))
}

/// How long an unattended run may wait for a usage limit to lift.
#[derive(Debug, Clone, Copy)]
pub struct Wait {
    /// All the waiting one call may do (`wait_limit`).
    pub limit: Duration,
    /// When the run must be over (its time limit), if it has one: a wait
    /// that would end after it is not started.
    pub deadline: Option<Instant>,
}

#[derive(Default)]
struct State {
    /// This turn's calls go to the fallback.
    on_fallback: bool,
    /// Before this, a new turn starts on the fallback too.
    main_blocked_until: Option<Instant>,
}

/// The main model, the fallback (if set) and the wait (unattended runs).
pub struct Switching {
    main: Box<dyn Provider>,
    fallback: Option<Box<dyn Provider>>,
    /// `fallback_after`: how long the main model stays skipped after a
    /// block without `retry-after`, and the wait guess for it.
    after: Duration,
    wait: Option<Wait>,
    state: Mutex<State>,
    /// The last call went to the fallback ([`Provider::model`]).
    used_fallback: AtomicBool,
}

impl Switching {
    pub fn new(
        main: Box<dyn Provider>,
        fallback: Option<Box<dyn Provider>>,
        after: Duration,
        wait: Option<Wait>,
    ) -> Self {
        Self {
            main,
            fallback,
            after,
            wait,
            state: Mutex::default(),
            used_fallback: AtomicBool::new(false),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn on_fallback(&self) -> bool {
        self.fallback.is_some() && self.state().on_fallback
    }

    fn current(&self) -> &dyn Provider {
        match &self.fallback {
            Some(fallback) if self.on_fallback() => fallback.as_ref(),
            _ => self.main.as_ref(),
        }
    }

    async fn call(
        &self,
        request: CompletionRequest<'_>,
        events: Option<&StreamSender>,
    ) -> Result<CompletionResponse, ProviderError> {
        let mut waited = Duration::ZERO;
        loop {
            let on_fallback = self.on_fallback();
            let provider = self.current();
            let (result, reported) = one_call(provider, request, events).await;
            self.used_fallback.store(on_fallback, Ordering::Relaxed);
            let error = match result {
                Ok(response) => {
                    if let (Some(events), Some(_)) = (events, &self.fallback) {
                        let model = provider.model().unwrap_or_default().to_string();
                        let _ = events.send(StreamEvent::AnsweredBy(model));
                    }
                    return Ok(response);
                }
                Err(error) => error,
            };
            let Some(reason) = Reason::of(&error) else {
                return Err(error);
            };
            let pause = error.retry_after().unwrap_or(self.after);
            if !on_fallback && let Some(fallback) = &self.fallback {
                let now = Instant::now();
                {
                    let mut state = self.state();
                    state.on_fallback = true;
                    state.main_blocked_until = Some(now + pause);
                }
                if let Some(events) = events {
                    if reported {
                        let _ = events.send(StreamEvent::Retrying);
                    }
                    let _ = events.send(StreamEvent::ModelSwitch(Switch {
                        from: self.main.model().unwrap_or("main").to_string(),
                        to: fallback.model().unwrap_or("fallback").to_string(),
                        reason,
                        retry_at: SystemTime::now() + pause,
                        retry_in: pause,
                    }));
                }
                continue;
            }
            // Nothing else to switch to: wait, when this run may.
            let Some(wait) = self.wait.filter(|wait| !wait.limit.is_zero()) else {
                return Err(error);
            };
            let pause = pause.max(Duration::from_secs(1));
            let notice = |text: String| {
                if let Some(events) = events {
                    let _ = events.send(StreamEvent::Notice(text));
                }
            };
            if waited + pause > wait.limit {
                notice(format!(
                    "모델이 {} 막혔습니다. 다시 시도까지 {} 더 기다려야 해서, 기다릴 수 있는 \
                     시간(wait_limit {})을 넘으므로 멈춥니다",
                    reason.korean(),
                    crate::korean::duration(round_up(pause)),
                    crate::korean::duration(wait.limit),
                ));
                return Err(error);
            }
            if wait
                .deadline
                .is_some_and(|deadline| Instant::now() + pause >= deadline)
            {
                notice(format!(
                    "모델이 {} 막혔습니다. {} 기다리면 실행 시간 상한을 넘으므로 멈춥니다",
                    reason.korean(),
                    crate::korean::duration(round_up(pause)),
                ));
                return Err(error);
            }
            notice(format!(
                "모델이 {} 막혀 {} 기다렸다가 다시 보냅니다",
                reason.korean(),
                crate::korean::duration(round_up(pause)),
            ));
            if reported && let Some(events) = events {
                let _ = events.send(StreamEvent::Retrying);
            }
            tokio::time::sleep(pause).await;
            waited += pause;
            // After the wait the main model goes first again, unless it is
            // still blocked.
            let mut state = self.state();
            state.on_fallback = state
                .main_blocked_until
                .is_some_and(|until| Instant::now() < until);
        }
    }
}

/// One call to `provider`, passing its stream events on; also says whether
/// any of the response was reported (and is void if another call follows).
async fn one_call(
    provider: &dyn Provider,
    request: CompletionRequest<'_>,
    events: Option<&StreamSender>,
) -> (Result<CompletionResponse, ProviderError>, bool) {
    let Some(events) = events else {
        return (provider.complete(request).await, false);
    };
    let (sender, mut received) = tokio::sync::mpsc::unbounded_channel();
    let mut reported = false;
    let mut pass = |event: StreamEvent| {
        match event {
            StreamEvent::Retrying => reported = false,
            StreamEvent::ModelSwitch(_) | StreamEvent::Notice(_) | StreamEvent::AnsweredBy(_) => {}
            _ => reported = true,
        }
        let _ = events.send(event);
    };
    let call = provider.complete_streaming(request, &sender);
    tokio::pin!(call);
    let result = loop {
        tokio::select! {
            biased;
            Some(event) = received.recv() => pass(event),
            result = &mut call => break result,
        }
    };
    while let Ok(event) = received.try_recv() {
        pass(event);
    }
    (result, reported)
}

#[async_trait]
impl Provider for Switching {
    async fn complete(
        &self,
        request: CompletionRequest<'_>,
    ) -> Result<CompletionResponse, ProviderError> {
        self.call(request, None).await
    }

    async fn complete_streaming(
        &self,
        request: CompletionRequest<'_>,
        events: &StreamSender,
    ) -> Result<CompletionResponse, ProviderError> {
        self.call(request, Some(events)).await
    }

    fn manual_thinking(&self) -> bool {
        self.current().manual_thinking()
    }

    fn model(&self) -> Option<&str> {
        match &self.fallback {
            Some(fallback) if self.used_fallback.load(Ordering::Relaxed) => fallback.model(),
            _ => self.main.model(),
        }
    }

    /// The main model again, unless it was blocked until later than now.
    fn begin_turn(&self) {
        let mut state = self.state();
        state.on_fallback = state
            .main_blocked_until
            .is_some_and(|until| Instant::now() < until);
    }
}

#[cfg(test)]
mod tests;
