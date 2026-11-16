//! Console and network journals: bounded ring buffers with cursors.
//!
//! The model never receives a whole log. It gets the newest entries plus a
//! cursor (`next`), and a count of what the ring dropped while it was not
//! looking. This is the difference between a useful debugging channel and a
//! context blow-up: a single noisy SPA can emit thousands of console lines.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::{ConsoleEntry, ConsoleLevel, JournalPage, JournalQuery, NetEntry};

/// Per-entry text cap. A page can log a 5 MB object dump; the model cannot.
pub const MAX_TEXT_CHARS: usize = 4000;

/// Cap on in-flight requests tracked until their response arrives.
const MAX_PENDING_REQUESTS: usize = 512;

/// URL length cap for both journals.
const MAX_URL_CHARS: usize = 2000;

pub struct Journal {
    inner: Mutex<JournalState>,
}

struct JournalState {
    console: Vec<ConsoleEntry>,
    network: Vec<NetEntry>,
    pending: HashMap<String, PendingRequest>,
    /// Insertion order of `pending`, so overflow drops the oldest.
    pending_order: Vec<String>,
    console_capacity: usize,
    network_capacity: usize,
    next_console_seq: u64,
    next_net_seq: u64,
    dropped_console: u64,
    dropped_network: u64,
}

struct PendingRequest {
    method: String,
    url: String,
    resource_type: String,
    seq: u64,
    started_ms: i64,
}

impl Journal {
    pub fn new(console_capacity: usize, network_capacity: usize) -> Self {
        Self {
            inner: Mutex::new(JournalState {
                console: Vec::new(),
                network: Vec::new(),
                pending: HashMap::new(),
                pending_order: Vec::new(),
                console_capacity: console_capacity.max(1),
                network_capacity: network_capacity.max(1),
                next_console_seq: 1,
                next_net_seq: 1,
                dropped_console: 0,
                dropped_network: 0,
            }),
        }
    }

    /// Read console entries newer than the query's cursor.
    ///
    /// Cursor semantics: `next` in the returned page is the last written
    /// sequence number, so the caller passes `since = page.next` next time and
    /// only sees new entries. `dropped` is the cumulative number of entries the
    /// ring evicted; a non-monotonic jump there means the caller missed lines.
    pub fn console(&self, query: &JournalQuery) -> JournalPage<ConsoleEntry> {
        let state = self.lock();
        let minimum_level = match (query.level, query.failed_only) {
            (Some(level), _) => Some(level),
            (None, true) => Some(ConsoleLevel::Error),
            (None, false) => None,
        };
        let url_filter = query.url_contains.as_deref().map(str::to_ascii_lowercase);

        let mut filtered: Vec<ConsoleEntry> = state
            .console
            .iter()
            .filter(|entry| match query.since {
                Some(since) => entry.seq > since,
                None => true,
            })
            .filter(|entry| {
                minimum_level
                    .map(|level| entry.level.rank() >= level.rank())
                    .unwrap_or(true)
            })
            .filter(|entry| match &url_filter {
                Some(filter) => entry
                    .url
                    .as_deref()
                    .map(|url| url.to_ascii_lowercase().contains(filter))
                    .unwrap_or(false),
                None => true,
            })
            .cloned()
            .collect();

        let limit = query.limit_or(40);
        if filtered.len() > limit {
            filtered.drain(..filtered.len() - limit);
        }

        JournalPage {
            entries: filtered,
            next: state.next_console_seq.saturating_sub(1),
            dropped: state.dropped_console,
            total: state.next_console_seq.saturating_sub(1),
        }
    }

    /// Read network entries newer than the query's cursor. Only completed
    /// requests appear, because status and duration are what the model needs.
    pub fn network(&self, query: &JournalQuery) -> JournalPage<NetEntry> {
        let state = self.lock();
        let url_filter = query.url_contains.as_deref().map(str::to_ascii_lowercase);

        let mut filtered: Vec<NetEntry> = state
            .network
            .iter()
            .filter(|entry| match query.since {
                Some(since) => entry.seq > since,
                None => true,
            })
            .filter(|entry| !query.failed_only || entry.is_failed())
            .filter(|entry| match &url_filter {
                Some(filter) => entry.url.to_ascii_lowercase().contains(filter),
                None => true,
            })
            .cloned()
            .collect();

        let limit = query.limit_or(40);
        if filtered.len() > limit {
            filtered.drain(..filtered.len() - limit);
        }

        JournalPage {
            entries: filtered,
            next: state.next_net_seq.saturating_sub(1),
            dropped: state.dropped_network,
            total: state.next_net_seq.saturating_sub(1),
        }
    }

    pub fn push_console(
        &self,
        level: ConsoleLevel,
        text: String,
        url: Option<String>,
        line: Option<u32>,
    ) {
        let mut state = self.lock();
        let seq = state.next_console_seq;
        state.next_console_seq += 1;
        let entry = ConsoleEntry {
            seq,
            level,
            text: truncate(&text, MAX_TEXT_CHARS),
            url: url.map(|url| truncate(&url, MAX_URL_CHARS)),
            line,
            at_ms: now_ms(),
        };
        state.console.push(entry);
        while state.console.len() > state.console_capacity {
            state.console.remove(0);
            state.dropped_console += 1;
        }
    }

    /// Register an in-flight request. It only reaches the ring once finished,
    /// so `pending` can never grow past [`MAX_PENDING_REQUESTS`].
    pub fn push_network_start(
        &self,
        request_id: &str,
        method: &str,
        url: &str,
        resource_type: &str,
    ) {
        if is_noise_url(url) {
            return;
        }
        let mut state = self.lock();
        if state.pending.len() >= MAX_PENDING_REQUESTS
            && let Some(oldest) = state.pending_order.first().cloned()
        {
            state.pending.remove(&oldest);
            state.pending_order.remove(0);
        }
        let seq = state.next_net_seq;
        state.next_net_seq += 1;
        state.pending.insert(
            request_id.to_string(),
            PendingRequest {
                method: method.to_string(),
                url: truncate(url, MAX_URL_CHARS),
                resource_type: resource_type.to_string(),
                seq,
                started_ms: now_ms(),
            },
        );
        state.pending_order.push(request_id.to_string());
    }

    pub fn push_network_finish(
        &self,
        request_id: &str,
        status: Option<u16>,
        failure: Option<String>,
        duration_ms: Option<u64>,
    ) {
        let mut state = self.lock();
        let Some(pending) = state.pending.remove(request_id) else {
            return;
        };
        if let Some(position) = state.pending_order.iter().position(|id| id == request_id) {
            state.pending_order.remove(position);
        }
        let duration = duration_ms.or_else(|| {
            let elapsed = now_ms().saturating_sub(pending.started_ms);
            u64::try_from(elapsed).ok()
        });
        let entry = NetEntry {
            seq: pending.seq,
            method: pending.method,
            url: pending.url,
            status,
            resource_type: pending.resource_type,
            failure,
            duration_ms: duration,
            at_ms: pending.started_ms,
        };
        state.network.push(entry);
        // Keep the ring in sequence order even though requests finish out of
        // order; the model reads oldest-first.
        state.network.sort_by_key(|entry| entry.seq);
        while state.network.len() > state.network_capacity {
            state.network.remove(0);
            state.dropped_network += 1;
        }
    }

    /// Count console errors newer than the cursor. Used to decide whether the
    /// tool result should nudge the model at `browser_console`.
    pub fn console_errors_since(&self, since: Option<u64>) -> usize {
        let state = self.lock();
        state
            .console
            .iter()
            .filter(|entry| match since {
                Some(since) => entry.seq > since,
                None => true,
            })
            .filter(|entry| entry.level.is_error())
            .count()
    }

    /// Clear both rings. Sequence counters are deliberately **not** reset, so a
    /// caller holding a stale cursor cannot re-consume old lines as new ones.
    pub fn clear(&self) {
        let mut state = self.lock();
        state.console.clear();
        state.network.clear();
        state.pending.clear();
        state.pending_order.clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, JournalState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// `data:`/`blob:` payloads and favicons are pure noise for a model.
fn is_noise_url(url: &str) -> bool {
    let lowered = url.trim_start().to_ascii_lowercase();
    lowered.starts_with("data:")
        || lowered.starts_with("blob:")
        || lowered.ends_with("/favicon.ico")
}

fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let mut out: String = value.chars().take(limit).collect();
    out.push('…');
    out
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal() -> Journal {
        // Small rings keep the eviction tests short.
        Journal::new(4, 4)
    }

    fn query() -> JournalQuery {
        JournalQuery::default()
    }

    #[test]
    fn console_sequence_starts_at_one_and_increments_monotonically() {
        let journal = journal();
        journal.push_console(ConsoleLevel::Log, "a".into(), None, None);
        journal.push_console(ConsoleLevel::Error, "b".into(), None, None);
        let page = journal.console(&query());
        assert_eq!(page.next, 2);
        assert_eq!(page.total, 2);
        assert_eq!(page.entries.len(), 2);
        // Oldest first.
        assert_eq!(page.entries[0].seq, 1);
        assert_eq!(page.entries[1].seq, 2);
    }

    #[test]
    fn since_cursor_returns_only_newer_entries() {
        let journal = journal();
        journal.push_console(ConsoleLevel::Log, "a".into(), None, None);
        let first = journal.console(&query());
        journal.push_console(ConsoleLevel::Log, "b".into(), None, None);
        let page = journal.console(&JournalQuery {
            since: Some(first.next),
            ..JournalQuery::default()
        });
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].text, "b");
        assert_eq!(page.next, 2);
    }

    #[test]
    fn eviction_drops_old_entries_but_never_rewinds_the_cursor() {
        let journal = journal();
        for index in 0..6 {
            journal.push_console(ConsoleLevel::Log, format!("line{index}"), None, None);
        }
        let page = journal.console(&query());
        assert_eq!(page.entries.len(), 4, "capacity is four");
        assert_eq!(page.dropped, 2);
        assert_eq!(page.next, 6);
        assert_eq!(page.entries.last().unwrap().seq, 6);
    }

    #[test]
    fn level_filter_is_at_least_semantics() {
        let journal = journal();
        journal.push_console(ConsoleLevel::Debug, "d".into(), None, None);
        journal.push_console(ConsoleLevel::Info, "i".into(), None, None);
        journal.push_console(ConsoleLevel::Warning, "w".into(), None, None);
        journal.push_console(ConsoleLevel::Error, "e".into(), None, None);
        let page = journal.console(&JournalQuery {
            level: Some(ConsoleLevel::Warning),
            ..JournalQuery::default()
        });
        let levels: Vec<_> = page.entries.iter().map(|entry| entry.level).collect();
        assert_eq!(levels, vec![ConsoleLevel::Warning, ConsoleLevel::Error]);
    }

    #[test]
    fn failed_only_on_console_means_errors() {
        let journal = journal();
        journal.push_console(ConsoleLevel::Warning, "w".into(), None, None);
        journal.push_console(ConsoleLevel::Error, "e".into(), None, None);
        let page = journal.console(&JournalQuery {
            failed_only: true,
            ..JournalQuery::default()
        });
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].text, "e");
    }

    #[test]
    fn limit_keeps_the_newest_entries_in_ascending_order() {
        let journal = Journal::new(20, 20);
        for index in 0..10 {
            journal.push_console(ConsoleLevel::Log, format!("line{index}"), None, None);
        }
        let page = journal.console(&JournalQuery {
            limit: Some(3),
            ..JournalQuery::default()
        });
        let texts: Vec<_> = page
            .entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(texts, vec!["line7", "line8", "line9"]);
        assert_eq!(page.next, 10);
    }

    #[test]
    fn network_entries_appear_only_after_finish() {
        let journal = journal();
        journal.push_network_start("r1", "GET", "https://a.test/one", "xhr");
        assert!(journal.network(&query()).entries.is_empty());
        journal.push_network_finish("r1", Some(200), None, Some(12));
        let page = journal.network(&query());
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].status, Some(200));
        assert_eq!(page.entries[0].duration_ms, Some(12));
    }

    #[test]
    fn unfinished_requests_are_capped_and_do_not_leak() {
        let journal = journal();
        for index in 0..(MAX_PENDING_REQUESTS + 50) {
            journal.push_network_start(
                &format!("r{index}"),
                "GET",
                &format!("https://a.test/{index}"),
                "xhr",
            );
        }
        let state = journal.lock();
        assert!(state.pending.len() <= MAX_PENDING_REQUESTS);
    }

    #[test]
    fn network_failed_only_selects_error_statuses_and_failures() {
        let journal = Journal::new(20, 20);
        journal.push_network_start("ok", "GET", "https://a.test/ok", "document");
        journal.push_network_finish("ok", Some(200), None, Some(5));
        journal.push_network_start("bad", "POST", "https://a.test/bad", "fetch");
        journal.push_network_finish("bad", Some(500), None, Some(7));
        journal.push_network_start("dead", "GET", "https://a.test/dead", "image");
        journal.push_network_finish("dead", None, Some("net::ERR_FAILED".into()), None);

        let page = journal.network(&JournalQuery {
            failed_only: true,
            ..JournalQuery::default()
        });
        let urls: Vec<_> = page
            .entries
            .iter()
            .map(|entry| entry.url.as_str())
            .collect();
        assert_eq!(urls, vec!["https://a.test/bad", "https://a.test/dead"]);
    }

    #[test]
    fn url_filter_is_case_insensitive_on_both_journals() {
        let journal = Journal::new(20, 20);
        journal.push_console(
            ConsoleLevel::Error,
            "boom".into(),
            Some("https://API.test/x".into()),
            None,
        );
        journal.push_network_start("r", "GET", "https://api.test/thing", "xhr");
        journal.push_network_finish("r", Some(200), None, Some(1));

        let console_page = journal.console(&JournalQuery {
            url_contains: Some("api.test".into()),
            ..JournalQuery::default()
        });
        assert_eq!(console_page.entries.len(), 1);
        let net_page = journal.network(&JournalQuery {
            url_contains: Some("API.TEST".into()),
            ..JournalQuery::default()
        });
        assert_eq!(net_page.entries.len(), 1);
    }

    #[test]
    fn noisy_urls_and_long_text_are_trimmed() {
        let journal = journal();
        journal.push_network_start("d", "GET", "data:image/png;base64,AAA", "image");
        journal.push_network_start("b", "GET", "blob:https://a.test/xyz", "xhr");
        journal.push_network_start("f", "GET", "https://a.test/favicon.ico", "other");
        journal.push_network_finish("d", Some(200), None, None);
        journal.push_network_finish("b", Some(200), None, None);
        journal.push_network_finish("f", Some(200), None, None);
        assert!(journal.network(&query()).entries.is_empty());

        let long = "x".repeat(MAX_TEXT_CHARS + 100);
        journal.push_console(ConsoleLevel::Log, long, None, None);
        let page = journal.console(&query());
        let text = &page.entries[0].text;
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn console_errors_since_respects_the_cursor() {
        let journal = journal();
        journal.push_console(ConsoleLevel::Error, "e1".into(), None, None);
        let cursor = journal.console(&query()).next;
        journal.push_console(ConsoleLevel::Log, "l".into(), None, None);
        journal.push_console(ConsoleLevel::Error, "e2".into(), None, None);
        assert_eq!(journal.console_errors_since(Some(cursor)), 1);
        assert_eq!(journal.console_errors_since(None), 2);
    }

    #[test]
    fn clear_does_not_rewind_sequence_numbers() {
        let journal = journal();
        journal.push_console(ConsoleLevel::Error, "old".into(), None, None);
        let cursor = journal.console(&query()).next;
        journal.clear();
        assert!(journal.console(&query()).entries.is_empty());
        journal.push_console(ConsoleLevel::Error, "new".into(), None, None);
        let page = journal.console(&JournalQuery {
            since: Some(cursor),
            ..JournalQuery::default()
        });
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].text, "new");
        assert!(page.entries[0].seq > cursor);
    }

    #[test]
    fn network_ring_is_sequence_ordered_even_when_responses_invert() {
        let journal = Journal::new(20, 20);
        journal.push_network_start("a", "GET", "https://a.test/a", "xhr");
        journal.push_network_start("b", "GET", "https://a.test/b", "xhr");
        // Second request finishes first.
        journal.push_network_finish("b", Some(200), None, Some(1));
        journal.push_network_finish("a", Some(200), None, Some(2));
        let page = journal.network(&query());
        let urls: Vec<_> = page
            .entries
            .iter()
            .map(|entry| entry.url.as_str())
            .collect();
        assert_eq!(urls, vec!["https://a.test/a", "https://a.test/b"]);
    }
}
