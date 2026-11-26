use gpui::Context;

use gpui_component::notification::Notification;

pub use tty7_core::host::{Host, HostId, SharedHost};

mod blocking {
    use std::collections::VecDeque;
    use std::sync::{Arc, Condvar, Mutex, OnceLock};
    use std::time::Duration;

    type Job = Box<dyn FnOnce() + Send + 'static>;

    const MAX_THREADS: usize = 64;

    const LINGER: Duration = Duration::from_secs(30);

    struct Inner {
        state: Mutex<State>,
        wake: Condvar,
    }

    struct State {
        jobs: VecDeque<Job>,
        threads: usize,
        idle: usize,
    }

    impl State {
        /// Whether a submission should start a thread rather than lean on the
        /// idle ones. Strictly greater: `jobs == idle` is already covered.
        fn wants_another_thread(&self) -> bool {
            self.jobs.len() > self.idle && self.threads < MAX_THREADS
        }
    }

    /// Whether a worker whose wait timed out should retire.
    ///
    /// Not on the timer alone. A job can be pushed between the timeout firing
    /// and this thread reacquiring the lock, and `submit` decides whether to
    /// spawn by counting idle threads — so it saw this one as available and
    /// did not spawn. Retiring on `timed_out` by itself would carry that job's
    /// only worker away with it, and the job would sit in the queue until some
    /// unrelated submission happened to start a thread.
    fn should_retire(timed_out: bool, pending_jobs: usize) -> bool {
        timed_out && pending_jobs == 0
    }

    fn pool() -> &'static Arc<Inner> {
        static POOL: OnceLock<Arc<Inner>> = OnceLock::new();
        POOL.get_or_init(|| {
            Arc::new(Inner {
                state: Mutex::new(State {
                    jobs: VecDeque::new(),
                    threads: 0,
                    idle: 0,
                }),
                wake: Condvar::new(),
            })
        })
    }

    pub(super) fn submit(job: impl FnOnce() + Send + 'static) {
        let inner = pool();
        let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
        st.jobs.push_back(Box::new(job));
        if st.wants_another_thread() {
            st.threads += 1;
            let spawned = Arc::clone(inner);
            match std::thread::Builder::new()
                .name("tty7-host-op".into())
                .spawn(move || worker(spawned))
            {
                Ok(_) => return,
                Err(e) => {
                    st.threads -= 1;
                    log::warn!("could not start a host-op thread: {e}");
                    if st.threads == 0
                        && let Some(job) = st.jobs.pop_back()
                    {
                        drop(st);
                        job();
                        return;
                    }
                }
            }
        }
        drop(st);
        inner.wake.notify_one();
    }

    fn worker(inner: Arc<Inner>) {
        loop {
            let job = {
                let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(job) = st.jobs.pop_front() {
                        break job;
                    }
                    st.idle += 1;
                    let (guard, timeout) = inner
                        .wake
                        .wait_timeout(st, LINGER)
                        .unwrap_or_else(|e| e.into_inner());
                    st = guard;
                    st.idle -= 1;
                    if should_retire(timeout.timed_out(), st.jobs.len()) {
                        st.threads -= 1;
                        return;
                    }
                }
            };
            job();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn state(jobs: usize, threads: usize, idle: usize) -> State {
            let mut q: VecDeque<Job> = VecDeque::new();
            for _ in 0..jobs {
                q.push_back(Box::new(|| {}));
            }
            State {
                jobs: q,
                threads,
                idle,
            }
        }

        #[test]
        fn an_idle_thread_is_preferred_over_a_new_one() {
            assert!(
                !state(1, 1, 1).wants_another_thread(),
                "one job and one idle thread needs nobody new"
            );
            assert!(
                !state(2, 2, 2).wants_another_thread(),
                "jobs == idle is already covered"
            );
            assert!(
                state(3, 2, 2).wants_another_thread(),
                "one job more than there are idle threads"
            );
        }

        #[test]
        fn the_first_job_starts_the_first_thread() {
            assert!(state(1, 0, 0).wants_another_thread());
        }

        #[test]
        fn the_pool_stops_growing_at_its_ceiling() {
            assert!(state(1000, MAX_THREADS - 1, 0).wants_another_thread());
            assert!(
                !state(1000, MAX_THREADS, 0).wants_another_thread(),
                "a backlog does not buy more than MAX_THREADS"
            );
        }

        #[test]
        fn a_worker_retires_only_on_a_timeout_with_nothing_queued() {
            assert!(should_retire(true, 0));
            assert!(!should_retire(false, 0), "a wake-up is not a timeout");
        }

        /// The whole reason the queue is consulted: `submit` counted this
        /// thread as idle and therefore did not spawn one, so retiring here
        /// would leave the job it just pushed with no worker.
        #[test]
        fn a_job_that_landed_during_the_timeout_keeps_the_worker_alive() {
            assert!(!should_retire(true, 1));
        }
    }
}

async fn off_thread<T, F>(f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = smol::channel::bounded(1);
    blocking::submit(move || {
        let _ = tx.send_blocking(f());
    });
    rx.recv().await.ok()
}

pub struct HostOps;

impl HostOps {
    pub fn run<T, E, F, L>(host: SharedHost, cx: &mut Context<E>, f: F, land: L)
    where
        E: 'static,
        T: Send + 'static,
        F: FnOnce(&dyn Host) -> T + Send + 'static,
        L: FnOnce(&mut E, T, &mut Context<E>) + 'static,
    {
        tty7_core::host::register_ui_thread();
        cx.spawn(async move |this, cx| {
            let Some(out) = off_thread(move || f(&*host)).await else {
                return;
            };
            let _ = this.update(cx, |view, cx| land(view, out, cx));
        })
        .detach();
    }
}

/// A failure worded as one sentence — "Could not open a terminal: <why>" —
/// as an error toast: what failed as the title, why under it.
///
/// The templates end in their `{error}`, after a colon or a dash, and some
/// of them are also shown whole elsewhere (the home screen keeps the start-up
/// one on screen), so the split is made here on the formatted text rather
/// than by giving every language a second, error-less copy of each. When the
/// reason is not at the end the sentence is left whole.
pub fn failure(sentence: String, error: &impl std::fmt::Display) -> Notification {
    let reason = error.to_string();
    match split_reason(&sentence, &reason) {
        Some(title) if !title.is_empty() => Notification::error(reason).title(title),
        _ => Notification::error(sentence),
    }
}

fn split_reason<'a>(sentence: &'a str, reason: &str) -> Option<&'a str> {
    if reason.is_empty() {
        return None;
    }
    let head = sentence.trim_end().strip_suffix(reason.trim_end())?;
    Some(head.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '：' | '—' | '-')))
}

#[cfg(test)]
mod failure_tests {
    use super::split_reason;

    #[test]
    fn the_reason_comes_off_the_end_with_its_separator() {
        assert_eq!(
            split_reason("Could not open a terminal: no pty", "no pty"),
            Some("Could not open a terminal")
        );
        assert_eq!(
            split_reason("Couldn't forward :3000 — refused", "refused"),
            Some("Couldn't forward :3000")
        );
        assert_eq!(
            split_reason("无法打开终端：no pty", "no pty"),
            Some("无法打开终端")
        );
        assert_eq!(
            split_reason("无法转发 :80——refused", "refused"),
            Some("无法转发 :80")
        );
    }

    #[test]
    fn a_reason_mid_sentence_leaves_it_whole() {
        assert_eq!(split_reason("x failed: boom, retrying", "boom"), None);
        assert_eq!(split_reason("anything", ""), None);
    }
}
