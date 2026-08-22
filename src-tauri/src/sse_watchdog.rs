use bytes::Bytes;
use futures_util::{stream::BoxStream, StreamExt};

type ByteStream = BoxStream<'static, Result<Bytes, reqwest::Error>>;

const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
// Healthy Responses streams normally emit response.created/in_progress promptly. Cut off a
// dead connection before Codex's generic 300-second idle timeout, while allowing long reasoning
// gaps after the first semantic event.
const FIRST_EVENT_IDLE_CUTOFF: std::time::Duration = std::time::Duration::from_secs(60);
const BETWEEN_EVENT_IDLE_CUTOFF: std::time::Duration = std::time::Duration::from_secs(270);

#[derive(Clone, Default)]
pub(crate) struct SseStreamDiagnostic {
    pub session_key: String,
    pub initial_account_id: String,
}

#[derive(Default)]
struct SseSemanticActivityDetector {
    event_has_data: bool,
    line_len: usize,
    line_prefix: [u8; 5],
    line_prefix_len: usize,
    skip_lf_after_cr: bool,
}

impl SseSemanticActivityDetector {
    /// Returns true when the bytes dispatch at least one SSE event containing a `data` field.
    /// Comment-only keep-alives do not count, and fields may be split across chunks.
    fn observe(&mut self, bytes: &[u8]) -> bool {
        let mut dispatched = false;
        for &byte in bytes {
            if self.skip_lf_after_cr {
                self.skip_lf_after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => {
                    dispatched |= self.finish_line();
                    self.skip_lf_after_cr = true;
                }
                b'\n' => dispatched |= self.finish_line(),
                _ => {
                    if self.line_prefix_len < self.line_prefix.len() {
                        self.line_prefix[self.line_prefix_len] = byte;
                        self.line_prefix_len += 1;
                    }
                    self.line_len += 1;
                }
            }
        }
        dispatched
    }

    fn finish_line(&mut self) -> bool {
        let dispatched = if self.line_len == 0 {
            let dispatched = self.event_has_data;
            self.event_has_data = false;
            dispatched
        } else {
            let is_data_field = (self.line_len == 4
                && self.line_prefix_len >= 4
                && &self.line_prefix[..4] == b"data")
                || (self.line_len >= 5
                    && self.line_prefix_len == 5
                    && &self.line_prefix == b"data:");
            self.event_has_data |= is_data_field;
            false
        };
        self.line_len = 0;
        self.line_prefix_len = 0;
        dispatched
    }
}

struct SseWatchdogState {
    inner: ByteStream,
    detector: SseSemanticActivityDetector,
    last_semantic_activity: std::time::Instant,
    last_raw_activity: std::time::Instant,
    saw_semantic_event: bool,
    diagnostic: SseStreamDiagnostic,
}

/// Keep the HTTP/TCP path alive with SSE comments. When no complete `data` event arrives before
/// the semantic cutoff, close the stream so Codex's native request retry takes over.
pub(crate) fn wrap_with_sse_watchdog(
    inner: ByteStream,
    diagnostic: SseStreamDiagnostic,
) -> ByteStream {
    wrap_with_config(
        inner,
        HEARTBEAT_INTERVAL,
        FIRST_EVENT_IDLE_CUTOFF,
        BETWEEN_EVENT_IDLE_CUTOFF,
        diagnostic,
    )
}

fn wrap_with_config(
    inner: ByteStream,
    heartbeat_interval: std::time::Duration,
    first_event_idle_cutoff: std::time::Duration,
    between_event_idle_cutoff: std::time::Duration,
    diagnostic: SseStreamDiagnostic,
) -> ByteStream {
    let now = std::time::Instant::now();
    let state = SseWatchdogState {
        inner,
        detector: SseSemanticActivityDetector::default(),
        last_semantic_activity: now,
        last_raw_activity: now,
        saw_semantic_event: false,
        diagnostic,
    };
    futures_util::stream::unfold(state, move |mut state| async move {
        let semantic_idle_cutoff = if state.saw_semantic_event {
            between_event_idle_cutoff
        } else {
            first_event_idle_cutoff
        };
        let semantic_idle = state.last_semantic_activity.elapsed();
        if semantic_idle >= semantic_idle_cutoff {
            log_semantic_idle_cutoff(&state, semantic_idle_cutoff);
            return None;
        }

        let wait = heartbeat_interval.min(semantic_idle_cutoff - semantic_idle);
        match tokio::time::timeout(wait, state.inner.next()).await {
            Ok(Some(item)) => {
                if let Ok(bytes) = &item {
                    state.last_raw_activity = std::time::Instant::now();
                    if state.detector.observe(bytes) {
                        state.saw_semantic_event = true;
                        state.last_semantic_activity = std::time::Instant::now();
                    } else if state.last_semantic_activity.elapsed() >= semantic_idle_cutoff {
                        log_semantic_idle_cutoff(&state, semantic_idle_cutoff);
                        return None;
                    }
                }
                Some((item, state))
            }
            Ok(None) => None,
            Err(_) if state.last_semantic_activity.elapsed() >= semantic_idle_cutoff => {
                log_semantic_idle_cutoff(&state, semantic_idle_cutoff);
                None
            }
            Err(_) => Some((Ok(Bytes::from_static(b": keep-alive\n\n")), state)),
        }
    })
    .boxed()
}

fn log_semantic_idle_cutoff(state: &SseWatchdogState, semantic_idle_cutoff: std::time::Duration) {
    eprintln!(
        "[Proxy] SSE semantic idle cutoff phase={} semantic_idle_ms={} raw_idle_ms={} cutoff_ms={} session_key={} initial_account_id={}",
        if state.saw_semantic_event { "between_events" } else { "first_event" },
        state.last_semantic_activity.elapsed().as_millis(),
        state.last_raw_activity.elapsed().as_millis(),
        semantic_idle_cutoff.as_millis(),
        if state.diagnostic.session_key.is_empty() { "?" } else { &state.diagnostic.session_key },
        if state.diagnostic.initial_account_id.is_empty() { "?" } else { &state.diagnostic.initial_account_id },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_detector_ignores_comment_only_events() {
        let mut detector = SseSemanticActivityDetector::default();
        assert!(!detector.observe(b": keep-alive\n\n"));
        assert!(!detector.observe(b"event: ping\n\n"));
    }

    #[test]
    fn activity_detector_accepts_data_events_split_across_chunks() {
        let mut detector = SseSemanticActivityDetector::default();
        assert!(!detector.observe(b"event: response.in_progress\nda"));
        assert!(!detector.observe(b"ta: {\"type\":\"response.in_progress\"}\n"));
        assert!(detector.observe(b"\n"));
    }

    #[test]
    fn activity_detector_supports_crlf_and_empty_data_fields() {
        let mut detector = SseSemanticActivityDetector::default();
        assert!(detector.observe(b"data\r\n\r\n"));
        assert!(detector.observe(b"data:\r\n\r\n"));
    }

    #[tokio::test]
    async fn watchdog_emits_comments_then_closes_on_semantic_idle() {
        let inner: ByteStream = futures_util::stream::pending().boxed();
        let mut stream = wrap_with_config(
            inner,
            std::time::Duration::from_millis(5),
            std::time::Duration::from_millis(18),
            std::time::Duration::from_millis(30),
            SseStreamDiagnostic::default(),
        );
        let mut comments = 0;
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
                .await
                .expect("watchdog should either heartbeat or close")
            {
                Some(Ok(bytes)) => {
                    assert_eq!(bytes.as_ref(), b": keep-alive\n\n");
                    comments += 1;
                }
                Some(Err(err)) => panic!("unexpected stream error: {err}"),
                None => break,
            }
        }
        assert!(comments >= 2, "expected heartbeat comments before cutoff");
    }

    #[tokio::test]
    async fn watchdog_resets_only_after_complete_data_event() {
        let upstream = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.in_progress\"}\n",
            )),
            Ok(Bytes::from_static(b"\n")),
        ])
        .chain(futures_util::stream::pending())
        .boxed();
        let mut stream = wrap_with_config(
            upstream,
            std::time::Duration::from_millis(5),
            std::time::Duration::from_millis(8),
            std::time::Duration::from_millis(20),
            SseStreamDiagnostic::default(),
        );
        assert!(stream.next().await.is_some());
        assert!(stream.next().await.is_some());
        let started = std::time::Instant::now();
        while stream.next().await.is_some() {}
        assert!(started.elapsed() >= std::time::Duration::from_millis(15));
    }

    #[tokio::test]
    async fn watchdog_does_not_treat_upstream_comments_as_semantic_activity() {
        let upstream = futures_util::stream::unfold((), |_| async {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            Some((Ok(Bytes::from_static(b": upstream keep-alive\n\n")), ()))
        })
        .boxed();
        let mut stream = wrap_with_config(
            upstream,
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(15),
            std::time::Duration::from_millis(30),
            SseStreamDiagnostic::default(),
        );
        let started = std::time::Instant::now();
        let mut comments = 0;
        while let Some(item) = stream.next().await {
            assert_eq!(item.unwrap().as_ref(), b": upstream keep-alive\n\n");
            comments += 1;
        }
        assert!(comments >= 2);
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
    }
}
