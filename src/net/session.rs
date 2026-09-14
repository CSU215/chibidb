//! Sessions that outlive the connection that started them.
//!
//! The frontend's default is one `Session` per TCP connection, which a browser
//! cannot rely on: it pools and reuses sockets freely, so a `BEGIN` on one
//! request and the matching `INSERT` on the next may land on different ones. A
//! client that carries `X-Chibi-Session` gets a session kept here instead, in a
//! map keyed by an id the server minted.
//!
//! The two modes coexist on purpose. Requests without the header keep the
//! per-connection session and its rollback-on-close, so every existing client
//! behaves exactly as before, and no registry entry is created for them.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use crate::instance::Instance;
use crate::net::server::SharedInstance;
use crate::txn::trx::Session;

/// How long a session may sit untouched before it is rolled back and dropped.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How often the background sweeper runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Upper bound on live sessions, enforced by the sweeper. Without a bound a
/// loopback client could grow the map without limit.
const MAX_SESSIONS: usize = 256;

/// The request/response header carrying the session id.
pub(crate) const SESSION_HEADER: &str = "x-chibi-session";

struct Entry {
    session: Arc<Mutex<Session>>,
    /// Milliseconds since the registry was created; drives idle expiry.
    ///
    /// An atomic rather than a plain field: a request must be able to touch its
    /// session while holding only the read lock. A plain field would force a
    /// write lock on every request, which a busy sweeper could then starve.
    last_seen: AtomicU64,
    /// A strictly increasing tick, bumped on every touch; drives eviction order.
    ///
    /// Millisecond stamps cannot order requests that arrive inside the same
    /// millisecond, and a burst leaves every candidate equally "recent". The
    /// eviction pick would then be arbitrary and could drop a session that is
    /// mid-transaction while keeping an idle one.
    last_tick: AtomicU64,
}

pub(crate) struct SessionRegistry {
    started: Instant,
    /// Source of [`Entry::last_tick`]. Wraps after 2^64 touches, which is fine:
    /// it only ever has to order a few hundred live entries.
    tick: AtomicU64,
    entries: RwLock<HashMap<String, Arc<Entry>>>,
}

impl SessionRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Instant::now(),
            tick: AtomicU64::new(0),
            entries: RwLock::new(HashMap::new()),
        })
    }

    /// The session named by `id`, or a fresh one when the id is unknown.
    ///
    /// The returned id is what to echo back, which differs from `id` when the
    /// requested one was unknown: a stale id left over from an earlier server
    /// run must be harmless rather than an error. Note that a requested id is
    /// never *adopted* -- only ids this registry minted can name a session, so
    /// a client cannot choose its own.
    ///
    /// Returns the session as an `Arc` so the caller locks it *after* this call
    /// has released the registry lock. Holding the registry lock while locking a
    /// session would invert the order the sweeper uses.
    pub(crate) fn claim(&self, id: &str) -> (String, Arc<Mutex<Session>>) {
        let found = self.entries.read().get(id).cloned();
        if let Some(entry) = found {
            self.touch(&entry);
            return (id.to_string(), Arc::clone(&entry.session));
        }
        self.mint()
    }

    /// A brand new session, with an id to hand to the client.
    pub(crate) fn mint(&self) -> (String, Arc<Mutex<Session>>) {
        let id = new_session_id();
        let session = Arc::new(Mutex::new(Session::new()));
        let entry = Arc::new(Entry {
            session: Arc::clone(&session),
            last_seen: AtomicU64::new(self.millis()),
            last_tick: AtomicU64::new(self.tick.fetch_add(1, Ordering::Relaxed)),
        });
        self.entries.write().insert(id.clone(), entry);
        (id, session)
    }

    fn touch(&self, entry: &Entry) {
        entry.last_seen.store(self.millis(), Ordering::Relaxed);
        entry.last_tick.store(self.tick.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    }

    /// Drops sessions idle past [`IDLE_TIMEOUT`], then evicts the least recently
    /// used until the map is back within [`MAX_SESSIONS`]. Returns how many were
    /// dropped.
    ///
    /// Every dropped session is rolled back first. This is why a sweeper exists
    /// rather than relying on expiry-at-next-request: an abandoned session with
    /// `BEGIN` open holds row locks and its id in the open set, so it blocks
    /// `flush()` and VACUUM until the process exits.
    pub(crate) fn sweep(&self, instance: &Instance, now: Instant) -> usize {
        let now_millis = now.saturating_duration_since(self.started).as_millis() as u64;
        let cutoff = now_millis.saturating_sub(IDLE_TIMEOUT.as_millis() as u64);

        // Only remove from the map here. The rollback below needs the instance
        // and can block behind a long query, so it must not run under the
        // registry lock.
        let mut dropped: Vec<Arc<Entry>> = Vec::new();
        {
            let mut entries = self.entries.write();
            let expired: Vec<String> = entries
                .iter()
                .filter(|(_, entry)| entry.last_seen.load(Ordering::Relaxed) < cutoff)
                .map(|(id, _)| id.clone())
                .collect();
            for id in expired {
                if let Some(entry) = entries.remove(&id) {
                    dropped.push(entry);
                }
            }
            while entries.len() > MAX_SESSIONS {
                let oldest = entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_tick.load(Ordering::Relaxed))
                    .map(|(id, _)| id.clone());
                match oldest.and_then(|id| entries.remove(&id)) {
                    Some(entry) => dropped.push(entry),
                    None => break,
                }
            }
        }

        for entry in &dropped {
            let mut session = entry.session.lock();
            if let Err(e) = instance.rollback_session(&mut session) {
                eprintln!("http session cleanup error: {e}");
            }
        }
        dropped.len()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.read().len()
    }

    fn millis(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

/// Sweeps forever on a background thread.
pub(crate) fn spawn_sweeper(registry: Arc<SessionRegistry>, instance: SharedInstance) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(SWEEP_INTERVAL);
            registry.sweep(&instance, Instant::now());
        }
    });
}

/// A fresh session id.
///
/// `RandomState` is seeded per process from the OS, and the nanosecond clock plus
/// a counter keep concurrent ids distinct, so the result is not guessable from
/// outside without an OS-level leak. This is not a cryptographic token, and the
/// id is effectively a bearer credential: if the HTTP frontend is ever bound
/// beyond loopback, revisit this.
fn new_session_id() -> String {
    use std::hash::{BuildHasher, Hasher};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut id = String::with_capacity(32);
    for salt in 0u64..2 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(nanos);
        hasher.write_u64(counter);
        hasher.write_u64(salt);
        id.push_str(&format!("{:016x}", hasher.finish()));
    }
    id
}

/// `Session` crosses thread boundaries, so `Mutex<Session>` must be `Send + Sync`.
/// Asserted here so a future field cannot quietly break the registry.
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    fn check() {
        assert_send_sync::<Session>();
        assert_send_sync::<Mutex<Session>>();
    }
    let _ = check;
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn registry_with_instance() -> (Arc<SessionRegistry>, Instance) {
        let instance = Instance::open_in_memory(&Config::default()).unwrap();
        (SessionRegistry::new(), instance)
    }

    #[test]
    fn ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(new_session_id()));
        }
    }

    #[test]
    fn a_client_chosen_id_is_replaced_rather_than_adopted() {
        let (registry, _instance) = registry_with_instance();
        let (issued, _session) = registry.claim("client-chosen");
        assert_ne!(issued, "client-chosen");
        // And the issued id really does name a session from now on.
        assert_eq!(registry.claim(&issued).0, issued);
    }

    #[test]
    fn idle_sessions_are_rolled_back_and_dropped() {
        let (registry, instance) = registry_with_instance();
        let (id, session) = registry.mint();

        // Leave an open transaction behind, so there is something to roll back.
        {
            let mut guard = session.lock();
            instance.execute_with(&mut guard, "create table t (id int);").unwrap();
            instance.execute_with(&mut guard, "begin; insert into t values (1);").unwrap();
        }

        // Not idle yet: nothing is dropped.
        assert_eq!(registry.sweep(&instance, Instant::now()), 0);
        assert_eq!(registry.len(), 1);

        // Past the timeout: dropped, and its open transaction rolled back.
        let later = registry.started + IDLE_TIMEOUT + Duration::from_secs(1);
        assert_eq!(registry.sweep(&instance, later), 1);
        assert_eq!(registry.len(), 0);

        let mut fresh = Session::new();
        let rows = instance.execute_with(&mut fresh, "select * from t;").unwrap();
        assert!(
            format!("{rows:?}").contains("[]"),
            "the uncommitted row must not have survived the rollback: {rows:?}"
        );

        // The old id names nothing now, so it is replaced on the next use.
        assert_ne!(registry.claim(&id).0, id);
    }

    #[test]
    fn the_sweeper_evicts_the_least_recently_used_beyond_the_cap() {
        let (registry, instance) = registry_with_instance();

        let mut ids = Vec::new();
        for _ in 0..MAX_SESSIONS {
            ids.push(registry.mint().0);
        }
        assert_eq!(registry.len(), MAX_SESSIONS);

        // Touch everything but the first, so the first becomes the least recent.
        for id in &ids[1..] {
            registry.claim(id);
        }
        let newest = registry.mint().0;
        assert_eq!(registry.len(), MAX_SESSIONS + 1, "staying within the cap is the sweeper's job");

        registry.sweep(&instance, Instant::now());
        assert_eq!(registry.len(), MAX_SESSIONS);
        // The untouched one went first; the ones just used stayed.
        assert_ne!(registry.claim(&ids[0]).0, ids[0], "the least recently used should be gone");
        assert_eq!(registry.claim(&ids[1]).0, ids[1]);
        assert_eq!(registry.claim(&newest).0, newest);
    }
}
