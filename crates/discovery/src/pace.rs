//! Request pacing and a per-registrable-domain request budget (P0-12).
//!
//! Two controls, both enforced at the one place every HTTP request this crate sends passes
//! through (`HttpTransport::post`, in this crate's `transport` module):
//!
//! - **A minimum interval between consecutive requests to the same registrable domain.**
//!   The unit is *requests to that domain*, not *hosts processed*, so it spaces the whole
//!   sequence one host sees — P0-11's modern-first probe, the legacy handshake, the
//!   `notifications/initialized` POST and `tools/list` — and it spaces two registry entries
//!   that happen to point at the same vendor. It does **not** space unrelated hosts: a
//!   domain's first request never waits, so a sequential sweep over 1,000 distinct vendors
//!   pays the interval only *inside* each sequence.
//! - **A hard cap on how many requests one registrable domain may receive per run.** A
//!   per-host budget is the wrong unit: the committed July census holds 18 registry entries
//!   on 18 *distinct* hosts under `klymax402.com`, and 149 entries on the single host
//!   `gateway.pipeworx.io`. A per-host budget is satisfied by all of them while the vendor
//!   absorbs hundreds of requests.
//!
//! Why these exist rather than being a nice-to-have: this project's own O-01 probe lost a
//! transcript to an HTTP 429 caused by three requests landing inside one second against one
//! host, P0-11's live verification therefore imposed a 1.2 s floor **by hand**, and the
//! committed `results/census/class_b_annotation_coverage.json` already contains two
//! `http status: 429` records. See [ADR-014] for the derivation of the registrable domain,
//! the defaults, and what the derivation gets wrong.
//!
//! **Nothing here retries.** A `Retry-After` is captured for reporting (P0-11) and a
//! throttled host is skipped, never re-requested within a run.
//!
//! [ADR-014]: ../../../docs/adr/014-request-pacing-and-domain-budget.md

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Requests one [`crate::DiscoveryClient::discover`] call may send, reserved up front by
/// [`Pacer::admit`] so the cap can never be overshot — including under concurrency, where
/// two admissions must not each see the same remaining headroom.
///
/// Equal to the request budget `discover`'s *handshake* half spends ("at most five, and the
/// fallback is always a single bounded step, never a loop"). That coupling is asserted, not
/// trusted: [`Permit::overshoot`] counts any discovery that sent more than its own permit was
/// reserved for, and the accounting publishes the total.
///
/// **Unchanged by P0-13, deliberately.** `tools/list` pagination adds one request per extra
/// page, which is not knowable at admission time, and raising this constant to cover a
/// worst-case walk would have been the wrong fix. At the derived cap of 120, a reservation of
/// 5 admits at least 24 sequences per registrable domain, which is what lets every group in
/// the committed July corpus except the one 149-entry outlier be attempted in full (the
/// largest legitimate group holds 22 entries). A reservation sized for a 40-page walk — 44 —
/// would admit **two**, refusing 20 of those 22 and destroying far more data than pagination
/// recovers. So the reservation stays 5 and a walk buys each extra page from the same budget
/// as it goes, through [`Permit::try_extend`]: the cap stays hard, and a walk that cannot
/// buy its next page is *truncated and recorded as such*.
///
/// **What buying ahead does not do, on its own:** protect the other hosts on the domain.
/// The pages a walk buys come out of the same shared pool later sequences on that domain
/// are admitted from, so an unlimited extension hands one listing the power to refuse its
/// co-tenants — the effect a 44-request reservation was rejected for, delivered on demand.
/// [`MAX_SEQUENCE_SHARE_DIVISOR`] is what bounds it. See ADR-015 §3.
pub const MAX_REQUESTS_PER_DISCOVERY: usize = 5;

/// One sequence may hold at most `cap / MAX_SEQUENCE_SHARE_DIVISOR` requests of its
/// registrable domain's budget, extensions included (never less than its own admission
/// reservation).
///
/// **Why a per-sequence ceiling exists (P0-13 security review, B2).** [`Permit::try_extend`]
/// draws from the registrable domain's *shared* pool with no per-sequence accounting, so a
/// 40-page walk used to take 41 of 120 requests against an honest sequence's 5 — 8.2×. On
/// one shared domain that measured as honest co-tenants admitted falling 24 → 15 with one
/// hostile listing, → 8 with two, → 2 with four; and every refused co-tenant was recorded
/// `registrable_domain_budget_exhausted`, i.e. **attributed to this harness's own politeness
/// control**, while the listing that displaced them recorded a clean success. The grouping
/// key makes it broad rather than exotic: it is a last-two-labels heuristic, so shared
/// platforms collapse (`alice.vercel.app` and `bob.vercel.app` both key to `vercel.app`),
/// and the committed corpus already holds a 149-entry and a 22-entry group on such keys.
///
/// **Why 8.** At the census cap of 120 it is 15 requests: the 5-request handshake plus ten
/// extra `tools/list` pages, so eleven pages in all — still ~an order of magnitude above any
/// real tool list this project has observed (one page; 112 tools at most) — and one
/// sequence can never hold more than 12.5% of a domain. At least eight sequences' worth of
/// headroom therefore always survives any one listing, and the same co-tenant measurement
/// becomes 24 → 21 → 18 → 12
/// (`one_paginating_listing_cannot_starve_its_co_tenants_out_of_the_census`). A walk that
/// hits its share is truncated with `budget_exhausted`, counted per domain in
/// `page_extensions_denied`, and is visible as such.
///
/// **Considered and not built:** granting an extension only from headroom left over after
/// the sweep's *pending* candidates on that domain. Tighter, but it needs the pacer to know
/// the corpus, which it deliberately does not, and the per-sequence ceiling already closes
/// the measured attack structurally.
pub const MAX_SEQUENCE_SHARE_DIVISOR: usize = 8;

/// Longest key this module will store or publish. The DNS limit on a presentation-format
/// name; a key is server-influenceable (it comes from a registry-supplied endpoint) and
/// becomes both a map key in published accounting and part of a failure detail string.
const MAX_KEY_BYTES: usize = 253;

/// Generic second-level labels that act as registry suffixes under a ccTLD, so that
/// `example.co.uk` and `example.com.br` group as one registrant rather than as `co.uk` and
/// `com.br`.
///
/// Deliberately a modest, conventional list rather than an exhaustive one: an *absent*
/// entry makes the grouping **coarser** (`moema.sampa.br` and `campobelo.sampa.br` share
/// the key `sampa.br`), which spends fewer requests, while a *wrong* entry would make it
/// finer. The failure direction that matters is therefore the safe one. See [ADR-014] §2.
///
/// [ADR-014]: ../../../docs/adr/014-request-pacing-and-domain-budget.md
const REGISTRY_SECOND_LEVELS: &[&str] = &[
    "ac", "asn", "co", "com", "ed", "edu", "firm", "gen", "go", "gob", "gouv", "gov", "gr",
    "id", "in", "ind", "info", "int", "lg", "ltd", "me", "mil", "ne", "net", "nhs", "nom",
    "or", "org", "plc", "sch", "web",
];

/// How politely a run contacts third parties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacePolicy {
    /// Minimum wall-clock gap between two consecutive requests to one registrable domain.
    pub min_interval: Duration,
    /// Hard cap on requests to one registrable domain for the lifetime of the [`Pacer`].
    pub max_requests_per_registrable_domain: usize,
}

impl PacePolicy {
    /// The census default: 500 ms spacing, 120 requests per registrable domain.
    ///
    /// **500 ms, the top of P0-12's 250–500 ms range, not the bottom.** 250 ms permits four
    /// requests per second; three requests inside one second is the rate that actually
    /// provoked a 429 from a real host in this project's own history (O-01). Choosing the
    /// bottom of the range would pick a rate already observed to fail. 500 ms permits two
    /// per second — still 2.4× looser than the 1.2 s floor P0-11's live verification
    /// imposed by hand, which is the gap this default deliberately leaves for the operator
    /// to close with `--min-interval-ms` when a run is small enough to afford it.
    ///
    /// **120 requests, derived from the committed July corpus rather than picked.** Grouping
    /// its 1,000 Class B entries by [`registrable_domain`] gives 619 groups; the largest
    /// after the outlier holds 22 entries. A cap of 120 with a 5-request reservation admits
    /// at least 24 full discovery sequences per group, so every group in that corpus is
    /// attempted in full **except** `pipeworx.io` — 149 entries on one host, which this cap
    /// cuts off at 116 requests instead of the ~298 it would otherwise receive. The cap
    /// therefore binds exactly where the risk was measured and nowhere else.
    pub const CENSUS_DEFAULT: Self = Self {
        min_interval: Duration::from_millis(500),
        max_requests_per_registrable_domain: 120,
    };
}

/// Per-domain bookkeeping.
#[derive(Debug, Default)]
struct DomainState {
    /// Requests actually sent to this domain.
    charged: usize,
    /// Outstanding reservations, `MAX_REQUESTS_PER_DISCOVERY` each.
    reserved: usize,
    /// Discovery sequences admitted, and refused for want of budget.
    admitted: usize,
    denied: usize,
    /// Mid-sequence reservation extensions refused for want of budget (P0-13: one per
    /// `tools/list` page a walk could not buy). Counted separately from `denied` because a
    /// refused *extension* truncates a tool list while a refused *sequence* means the host
    /// was never contacted — two different findings that must not share a bucket.
    extensions_denied: usize,
    /// Of `extensions_denied`, how many were refused because the *sequence* had reached its
    /// own share ([`MAX_SEQUENCE_SHARE_DIVISOR`]) rather than because the domain was full.
    extensions_denied_by_share: usize,
    /// Extra requests granted to sequences on this domain beyond their admission
    /// reservation — what pagination took out of the shared pool. Published so a reader can
    /// see that one host's walk consumed headroom other sequences could have been admitted
    /// against, rather than infer it.
    extensions_granted: usize,
    /// The largest reservation any one sequence on this domain ever held.
    largest_sequence_reservation: usize,
    /// The earliest instant at which the next request to this domain may be sent. `None`
    /// until the first request, so a domain's first request never waits.
    next_allowed: Option<Instant>,
}

#[derive(Debug, Default)]
struct State {
    domains: BTreeMap<String, DomainState>,
    requests: usize,
    waited: Duration,
    denials: usize,
    extension_denials: usize,
    overshoot: usize,
}

/// Enforces a [`PacePolicy`] across a whole run.
///
/// Shared behind an [`Arc`] and internally locked, so one `Pacer` governs every worker: the
/// cap is a property of the run, not of a thread. The lock is never held across a sleep —
/// a waiting request reserves its slot in the domain's schedule and then releases the lock,
/// so concurrent callers to *different* domains never serialise against each other and
/// concurrent callers to the *same* domain get non-overlapping wake times rather than all
/// waking together.
#[derive(Debug)]
pub struct Pacer {
    policy: PacePolicy,
    state: Mutex<State>,
}

impl Pacer {
    /// A pacer enforcing `policy`.
    #[must_use]
    pub fn new(policy: PacePolicy) -> Self {
        Self { policy, state: Mutex::new(State::default()) }
    }

    /// The policy this pacer enforces.
    #[must_use]
    pub fn policy(&self) -> PacePolicy {
        self.policy
    }

    /// Reserve budget for one discovery sequence against `endpoint`, or refuse it.
    ///
    /// The reservation is [`MAX_REQUESTS_PER_DISCOVERY`], taken atomically, so the cap is a
    /// hard bound rather than one the sequence can overshoot while it runs. Returning
    /// [`Denied`] is **not** a silent skip: the caller is required to record it, and the
    /// refusal carries the domain, the counts and the cap so the record says why.
    pub fn admit(self: &Arc<Self>, endpoint: &str) -> Result<Permit, Denied> {
        self.admit_with_reservation(endpoint, MAX_REQUESTS_PER_DISCOVERY)
    }

    /// [`Self::admit`] for a sequence whose request bound is not `discover()`'s.
    ///
    /// Track B's probe protocol is why this exists: one `probe_idempotent_hint` run is a
    /// handshake, a `resources/list`, two `tools/call`s and up to two full `resources/read`
    /// sweeps -- far more than [`MAX_REQUESTS_PER_DISCOVERY`]. Admitting it against a
    /// 5-request reservation would make the cap soft for *later* sequences on the same
    /// domain, which is the erosion a reservation exists to prevent, so the reservation is
    /// the caller's own bound rather than one shared constant.
    ///
    /// `reservation` is clamped to at least 1: a zero reservation would admit an unbounded
    /// number of sequences against a domain whose budget is already spent.
    pub fn admit_with_reservation(
        self: &Arc<Self>,
        endpoint: &str,
        reservation: usize,
    ) -> Result<Permit, Denied> {
        let reservation = reservation.max(1);
        let domain = registrable_domain_of_endpoint(endpoint);
        let cap = self.policy.max_requests_per_registrable_domain;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = state.domains.entry(domain.clone()).or_default();

        if entry.charged + entry.reserved + reservation > cap {
            let denied = Denied {
                registrable_domain: domain,
                charged: entry.charged,
                reserved: entry.reserved,
                cap,
            };
            entry.denied += 1;
            state.denials += 1;
            return Err(denied);
        }
        entry.reserved += reservation;
        entry.admitted += 1;
        entry.largest_sequence_reservation = entry.largest_sequence_reservation.max(reservation);
        Ok(Permit {
            pacer: Arc::clone(self),
            registrable_domain: domain,
            stats: Arc::new(PermitStats::new(reservation)),
            // Never below the admission reservation: Track B admits with its own, larger
            // bound, and a ceiling under it would refuse requests it was admitted for.
            ceiling: reservation.max(cap / MAX_SEQUENCE_SHARE_DIVISOR),
        })
    }

    /// What this pacer did, for the results header.
    #[must_use]
    pub fn accounting(&self) -> Accounting {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut domains: Vec<DomainAccounting> = state
            .domains
            .iter()
            .map(|(name, d)| DomainAccounting {
                registrable_domain: name.clone(),
                requests: d.charged,
                sequences_admitted: d.admitted,
                sequences_denied: d.denied,
                page_extensions_granted: d.extensions_granted,
                page_extensions_denied: d.extensions_denied,
                page_extensions_denied_by_sequence_share: d.extensions_denied_by_share,
                largest_sequence_reservation: d.largest_sequence_reservation,
            })
            .collect();
        // Most-loaded first, ties broken by name so the list is deterministic.
        domains.sort_by(|a, b| {
            b.requests.cmp(&a.requests).then_with(|| a.registrable_domain.cmp(&b.registrable_domain))
        });
        Accounting {
            policy: self.policy,
            per_discovery_reservation: MAX_REQUESTS_PER_DISCOVERY,
            max_reservation_per_sequence: MAX_REQUESTS_PER_DISCOVERY
                .max(self.policy.max_requests_per_registrable_domain / MAX_SEQUENCE_SHARE_DIVISOR),
            requests_sent: state.requests,
            waited: state.waited,
            sequences_denied: state.denials,
            page_extensions_denied: state.extension_denials,
            overshoot_requests: state.overshoot,
            max_requests_to_one_registrable_domain: domains.first().map_or(0, |d| d.requests),
            registrable_domains_contacted: domains.iter().filter(|d| d.requests > 0).count(),
            domains,
        }
    }
}

/// One discovery sequence's own request and wait counters.
///
/// Separate from [`Permit`] and shared with it, so a caller can keep a handle
/// ([`Permit::stats`]) after moving the permit into the client that will spend it — which is
/// what lets a sweep write per-server pacing accounting onto the record for the attempt that
/// just finished.
#[derive(Debug)]
pub struct PermitStats {
    requests: AtomicUsize,
    waited_micros: AtomicU64,
    /// The reservation this permit currently holds, so [`Self::overshoot`] measures against
    /// the caller's own request bound rather than against a constant that may not describe
    /// this sequence at all.
    ///
    /// Atomic and mutable since P0-13: a `tools/list` cursor walk does not know its own
    /// length at admission time and buys each extra page through [`Permit::try_extend`], so
    /// a permit's reservation **grows**. Overshoot therefore keeps meaning "requests this
    /// sequence sent that nobody reserved for it" — a legitimately extended walk is not
    /// overshoot, and a bug that sent an unbudgeted request still is.
    reservation: AtomicUsize,
}

impl PermitStats {
    fn new(reservation: usize) -> Self {
        Self {
            requests: AtomicUsize::new(0),
            waited_micros: AtomicU64::new(0),
            reservation: AtomicUsize::new(reservation),
        }
    }

    /// The reservation this permit currently holds — its admission reservation plus every
    /// extension [`Permit::try_extend`] has granted it.
    #[must_use]
    pub fn reservation(&self) -> usize {
        self.reservation.load(Ordering::Relaxed)
    }

    /// Requests sent under this permit.
    #[must_use]
    pub fn requests_sent(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }

    /// Time spent waiting for the minimum interval under this permit.
    #[must_use]
    pub fn waited(&self) -> Duration {
        Duration::from_micros(self.waited_micros.load(Ordering::Relaxed))
    }

    /// Requests sent beyond this permit's reservation, which should always be zero.
    #[must_use]
    pub fn overshoot(&self) -> usize {
        self.requests_sent().saturating_sub(self.reservation())
    }
}

/// Permission to run one discovery sequence against one registrable domain.
///
/// Dropping it releases the reservation, so an interrupted or panicking sequence cannot
/// strand budget. A permit belongs to exactly one in-flight sequence; the [`Pacer`] behind
/// it is what is shared across workers.
#[derive(Debug)]
pub struct Permit {
    pacer: Arc<Pacer>,
    registrable_domain: String,
    stats: Arc<PermitStats>,
    /// The most this one sequence may ever hold, extensions included. See
    /// [`MAX_SEQUENCE_SHARE_DIVISOR`].
    ceiling: usize,
}

impl Permit {
    /// The registrable domain this permit is for.
    #[must_use]
    pub fn registrable_domain(&self) -> &str {
        &self.registrable_domain
    }

    /// A handle to this permit's counters, readable after the permit itself has been moved
    /// into the client that spends it.
    #[must_use]
    pub fn stats(&self) -> Arc<PermitStats> {
        Arc::clone(&self.stats)
    }

    /// Requests sent under this permit so far.
    #[must_use]
    pub fn requests_sent(&self) -> usize {
        self.stats.requests_sent()
    }

    /// Time this permit spent waiting for the minimum interval.
    #[must_use]
    pub fn waited(&self) -> Duration {
        self.stats.waited()
    }

    /// Requests sent beyond this permit's reservation, which should always be zero.
    #[must_use]
    pub fn overshoot(&self) -> usize {
        self.stats.overshoot()
    }

    /// The reservation this permit currently holds — its admission reservation plus every
    /// extension [`Self::try_extend`] has granted it.
    #[must_use]
    pub fn reservation(&self) -> usize {
        self.stats.reservation()
    }

    /// Buy `extra` more requests on this permit's registrable domain, for a sequence whose
    /// length is not knowable at admission time.
    ///
    /// `tools/list` pagination (P0-13) is why this exists: a cursor walk discovers how many
    /// requests it needs one page at a time. The alternative shapes were both worse. Letting
    /// the walk simply send its extra requests would make the per-domain cap **soft** — a
    /// paginating server's pages would be charged against headroom reserved for later hosts
    /// on the same domain, which is exactly the erosion a reservation exists to prevent, and
    /// `overshoot_requests` would report it only *after* the requests had gone out. Raising
    /// [`MAX_REQUESTS_PER_DISCOVERY`] to a worst-case walk would refuse legitimate hosts
    /// wholesale (see that constant's own doc comment for the arithmetic). Buying one page at
    /// a time keeps the cap hard, keeps the reservation exactly as tight as the walk turned
    /// out to be, and turns "the budget ran out" into a decidable, recordable branch at the
    /// one moment it can still be acted on — before the request leaves.
    ///
    /// Two tests, either of which refuses:
    ///
    /// - **The sequence's own share.** The permit may never hold more than its ceiling —
    ///   `cap / MAX_SEQUENCE_SHARE_DIVISOR`, or its admission reservation if larger. Without
    ///   this an extension is drawn from the domain's *shared* pool with nothing stopping one
    ///   sequence draining it, which is how a single paginating listing used to refuse the
    ///   other hosts on its domain (see [`MAX_SEQUENCE_SHARE_DIVISOR`]).
    /// - **The domain's headroom**, the same test [`Pacer::admit`] makes, with the same
    ///   conservatism: a permit's `charged` requests are not deducted from its `reserved`
    ///   until it drops, so an in-flight sequence is double-counted against the cap while it
    ///   runs. That errs toward fewer requests, the safe direction for a politeness control.
    ///
    /// # Errors
    ///
    /// [`Denied`], naming the domain, the counts and the cap, when either test fails.
    /// **Nothing is charged and nothing is sent on that path** — the caller's obligation is
    /// to stop and record it, which is what [`crate::StopReason::BudgetExhausted`] is for.
    pub fn try_extend(&self, extra: usize) -> Result<(), Denied> {
        if extra == 0 {
            return Ok(());
        }
        let cap = self.pacer.policy.max_requests_per_registrable_domain;
        let mut state = self.pacer.state.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = state.domains.entry(self.registrable_domain.clone()).or_default();
        let over_share = self.stats.reservation().saturating_add(extra) > self.ceiling;
        if over_share || entry.charged + entry.reserved + extra > cap {
            let denied = Denied {
                registrable_domain: self.registrable_domain.clone(),
                charged: entry.charged,
                reserved: entry.reserved,
                cap,
            };
            entry.extensions_denied += 1;
            if over_share {
                entry.extensions_denied_by_share += 1;
            }
            state.extension_denials += 1;
            return Err(denied);
        }
        entry.reserved += extra;
        entry.extensions_granted += extra;
        let held = self.stats.reservation.fetch_add(extra, Ordering::Relaxed) + extra;
        entry.largest_sequence_reservation = entry.largest_sequence_reservation.max(held);
        Ok(())
    }

    /// Block until this domain's minimum interval has elapsed, then charge one request.
    ///
    /// Called immediately before the request leaves, from the single place every HTTP
    /// request in this crate is built. The wait happens *outside* the HTTP agent's own
    /// timeout, so pacing never eats into a request's time budget — and the elapsed time of
    /// the previous request counts toward the interval, so a slow host costs no extra
    /// spacing at all.
    ///
    /// **Consequence for anyone asserting on [`Permit::waited`] or
    /// [`Accounting::waited`]:** what is accumulated is `max(0, slot − now)`, the spacing
    /// this function *had to add*, not the spacing between requests. A slower machine makes
    /// it smaller and can legitimately drive it to zero, so a lower bound near
    /// `n × interval` is asserting the negation of the paragraph above. Assert wall clock
    /// for spacing; assert accounted wait only as non-zero.
    ///
    /// Infallible on purpose. Refusal belongs at [`Pacer::admit`], where it can be recorded
    /// against a server; a refusal mid-sequence would surface as an error indistinguishable
    /// from a server's own, which is the taxonomy damage P0-11 went to trouble to avoid.
    ///
    /// Public, not crate-private: this is the hook *any* paced client calls, and `probe`
    /// (Track B) is a second JSON-RPC-over-HTTP client in this workspace that will need the
    /// same control. It grants nothing — it sleeps and counts, and cannot send anything —
    /// so it does not widen P0-01's structural guarantee that no caller outside this crate
    /// can name an MCP method.
    pub fn before_request(&self) {
        let wait = {
            let mut state = self.pacer.state.lock().unwrap_or_else(PoisonError::into_inner);
            let interval = self.pacer.policy.min_interval;
            state.requests += 1;
            let entry = state.domains.entry(self.registrable_domain.clone()).or_default();
            entry.charged += 1;
            let now = Instant::now();
            // The slot this request takes, and the slot the next one may take. Computed and
            // committed under the lock, so two concurrent requests to one domain are spaced
            // from each other rather than both measuring the same `next_allowed`.
            let at = entry.next_allowed.map_or(now, |t| t.max(now));
            // `checked_add`, not `+`: `Instant + Duration` panics on overflow, and
            // `Pacer::new` takes any `Duration` a caller passes. The census CLI bounds the
            // interval to 60 s, but a panic in a sweep worker would discard a whole results
            // file, so the one-line defence is worth having at the source.
            entry.next_allowed = at.checked_add(interval).or(entry.next_allowed);
            let wait = at.saturating_duration_since(now);
            state.waited += wait;
            wait
        };
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        self.stats.waited_micros.fetch_add(
            u64::try_from(wait.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }

    /// Consume this permit into the HTTP client that will spend it.
    ///
    /// **This is the only way to obtain a configured [`ureq::Agent`] in this workspace**,
    /// and the agent is never handed back — see [`PacedAgent`]. Taking the permit by value
    /// is what makes the coupling structural: a caller cannot hold a permit and
    /// *separately* build an agent, because there is no other agent constructor, and cannot
    /// use the agent without the permit, because the agent lives inside it.
    ///
    /// Take [`Self::stats`] first if the per-attempt counters are wanted afterwards; the
    /// permit itself is gone once this returns.
    #[must_use]
    pub fn into_agent(self, timeout: Duration, statuses: StatusHandling) -> PacedAgent {
        PacedAgent { agent: PacedAgent::agent_for(timeout, statuses), permit: Some(self) }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let overshoot = self.overshoot();
        // `into_inner`, never `expect`: this runs in `Drop`, and a panic in a drop while
        // another panic is already unwinding aborts the process -- which, in a sweep worker,
        // discards a whole results file rather than one attempt. A poisoned pacer lock means
        // some other thread panicked while holding it; the state behind it is a set of
        // counters that is still structurally sound, and refusing to read it would turn one
        // lost attempt into a lost run. Same reasoning, less urgently, at the three other
        // lock sites in this module.
        let mut state = self.pacer.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.overshoot += overshoot;
        if let Some(entry) = state.domains.get_mut(&self.registrable_domain) {
            // The *current* reservation, extensions included (P0-13): a walk that bought
            // three extra pages must return four reservations' worth, or the headroom it
            // bought is stranded for the rest of the run and later hosts on the domain are
            // refused for budget nobody is using.
            entry.reserved = entry.reserved.saturating_sub(self.stats.reservation());
        }
    }
}

// ---- The only HTTP agent constructor in the harness ----

/// Redirects followed by any HTTP client this module hands out: **none**.
///
/// Zero is a containment property, not a preference, and it is baked into
/// [`PacedAgent`] rather than left to a call site. Three separate things break when a
/// redirect is followed below the pacing hook, which is where `ureq`'s own redirect loop
/// lives:
///
/// - **Pacing and the budget are bypassed.** [`Permit::before_request`] runs once, before
///   the body is handed to the agent; every hop the agent then takes on its own is
///   unspaced and uncharged. At `ureq`'s default of 10 redirects, one `post` is up to 11
///   requests in a few milliseconds, the per-domain cap of 120 permits up to 1,320, and
///   [`Accounting::requests_sent`] understates real load by up to 11× — with the redirect
///   target's share of it appearing under no key at all.
/// - **It is a server-controlled request.** A `Location` header is the one input from a
///   server under test that the census path would act on as an *instruction* rather than
///   record as evidence, which architecture.md §1 draws its trust boundary specifically to
///   forbid. A sweep's vantage point reaches loopback, the host, the local subnet, RFC1918,
///   link-local metadata and a public /21, and none of those are in the registry.
/// - **The record would attest to the wrong principal.** P0-02's pin is taken over the
///   bytes a named server returned. Under a cross-host redirect the bytes come from
///   whatever answered, the record still names the listed server, and nothing says which —
///   `save_redirect_history` is off, so not even the hop list survives.
///
/// What this *costs*: a server that answers a `301`/`302`/`307`/`308` is not followed, so
/// its 3xx arrives as an ordinary response, fails `call_raw`'s status gate, and lands in
/// the `transport` failure bucket — the same bucket the committed July file's 15
/// `redirect failed` entries already occupy. Only the detail string changes.
///
/// If following a redirect is ever wanted, it must be done **hop by hop at the call site**,
/// with each hop re-keyed through [`Pacer::admit`]/[`Permit::before_request`],
/// `save_redirect_history(true)`, and a `final_url` identity field on the record so a pin
/// says which principal it attests to. See ADR-014 for that design; it is recorded, not
/// built.
pub const MAX_REDIRECTS: u32 = 0;

/// How a non-2xx HTTP status reaches the caller.
///
/// Orthogonal to redirects (which are never followed either way) and kept as an explicit
/// choice because this workspace's two HTTP clients genuinely need different answers, and
/// silently changing either one's answer would move records between published failure
/// categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusHandling {
    /// A non-2xx arrives as an ordinary response whose body can be read.
    ///
    /// Mandatory for `discovery`: the dual-era algorithm (ADR-013) decides a server's era
    /// from the *body* of a `400`/`404`, and `ureq::Error::StatusCode` carries only a
    /// number.
    AsResponse,
    /// A non-2xx arrives as `ureq::Error::StatusCode`, rendering as `http status: NNN`.
    ///
    /// What `probe` has always done, and what Track B's committed results file records its
    /// `401`/`429`/`400` outcomes as. Changing it would reclassify those records.
    AsError,
}

/// An HTTP client that cannot follow a redirect, and — unless explicitly constructed
/// otherwise — cannot send a request without first paying its registrable domain's
/// interval and charging it to that domain's budget.
///
/// **This is the only way to obtain a configured [`ureq::Agent`] in this workspace, and it
/// never hands the agent back.** That is the point: the hook being *reachable* was not
/// enough, because `HttpTransport` and `probe::ProbeClient` each built their own agent and
/// each inherited `ureq`'s default of ten redirects below the hook. Making the agent
/// unobtainable without a [`Permit`] turns "paced, and no redirects" from a convention two
/// call sites happened to follow into the only shape a caller can write.
///
/// The unpaced shape still exists — a sweep is not the only caller, and every pre-P0-12
/// request-count test depends on it — but it is spelled [`PacedAgent::unpaced`], which is
/// conspicuous in review in a way an omitted argument is not.
#[derive(Debug)]
pub struct PacedAgent {
    agent: ureq::Agent,
    permit: Option<Permit>,
}

impl PacedAgent {
    fn agent_for(timeout: Duration, statuses: StatusHandling) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // See `MAX_REDIRECTS`. Set here, once, where no call site can forget it.
            .max_redirects(MAX_REDIRECTS)
            .http_status_as_error(statuses == StatusHandling::AsError)
            .build()
            .into()
    }

    /// An agent that is **not** paced, for a caller with no [`Pacer`] — a unit test driving
    /// a local fake, or a pre-P0-12 code path whose request sequence must stay unchanged.
    ///
    /// It still cannot follow a redirect: [`MAX_REDIRECTS`] is a containment property of
    /// every client this module builds, not part of the pacing bargain.
    #[must_use]
    pub fn unpaced(timeout: Duration, statuses: StatusHandling) -> Self {
        Self { agent: Self::agent_for(timeout, statuses), permit: None }
    }

    /// This agent's permit, if it has one.
    #[must_use]
    pub fn permit(&self) -> Option<&Permit> {
        self.permit.as_ref()
    }

    /// Pay this domain's interval, charge one request, and POST `body` to `url` with
    /// `headers`.
    ///
    /// Exactly one request leaves per call: the wait is taken immediately before `ureq` is
    /// given the body (so it sits outside the agent's own `timeout_global` and the previous
    /// request's time on the wire counts toward the interval), the builder is consumed by
    /// `send`, and the agent will not follow a `Location`. There is no way to reach the
    /// agent without passing through here.
    ///
    /// # Errors
    ///
    /// Whatever `ureq` returns — a connection, TLS or timeout failure, and, under
    /// [`StatusHandling::AsError`], a non-2xx status.
    pub fn post(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        if let Some(permit) = &self.permit {
            permit.before_request();
        }
        let mut builder = self.agent.post(url);
        for (key, value) in headers {
            builder = builder.header(*key, *value);
        }
        builder.send(body)
    }
}

/// A discovery sequence refused because its registrable domain's budget is spent.
///
/// Carries everything a results record needs to say *why*, because a budget exhaustion that
/// is not recorded is indistinguishable from a host that was never sampled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    /// The registrable domain whose budget is spent.
    pub registrable_domain: String,
    /// Requests already sent to it.
    pub charged: usize,
    /// Requests reserved by sequences still in flight against it.
    pub reserved: usize,
    /// The cap in force.
    pub cap: usize,
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "registrable-domain request budget exhausted for {}: {} sent, {} reserved, cap {} \
             (no request was made to this host)",
            self.registrable_domain, self.charged, self.reserved, self.cap
        )
    }
}

/// Per-domain line of a [`Pacer`]'s accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainAccounting {
    /// The grouping key.
    pub registrable_domain: String,
    /// Requests sent to it.
    pub requests: usize,
    /// Discovery sequences admitted against it.
    pub sequences_admitted: usize,
    /// Discovery sequences refused against it.
    pub sequences_denied: usize,
    /// Extra requests granted to sequences on this domain beyond their admission
    /// reservation — the share of the shared pool pagination took (P0-13). Beside
    /// `sequences_denied`, this is what lets a reader see one host's walk displacing other
    /// sequences rather than infer it.
    pub page_extensions_granted: usize,
    /// Page extensions refused on this domain, for any reason.
    pub page_extensions_denied: usize,
    /// Of those, the ones refused because the sequence had reached its own share
    /// ([`MAX_SEQUENCE_SHARE_DIVISOR`]) while the domain still had room.
    pub page_extensions_denied_by_sequence_share: usize,
    /// The largest reservation any one sequence on this domain ever held.
    pub largest_sequence_reservation: usize,
}

/// What a [`Pacer`] did over a whole run — the preimage of the `pacing` block in a census
/// results file, so a published sweep's politeness is auditable rather than asserted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accounting {
    /// The policy in force.
    pub policy: PacePolicy,
    /// [`MAX_REQUESTS_PER_DISCOVERY`], recorded so a reader can reproduce the admission
    /// arithmetic without reading this crate.
    pub per_discovery_reservation: usize,
    /// The most any one [`MAX_REQUESTS_PER_DISCOVERY`]-admitted sequence may hold,
    /// extensions included: `max(reservation, cap / MAX_SEQUENCE_SHARE_DIVISOR)`.
    pub max_reservation_per_sequence: usize,
    /// Requests sent under all permits.
    pub requests_sent: usize,
    /// Total time spent waiting for the minimum interval.
    pub waited: Duration,
    /// Discovery sequences refused for want of budget.
    pub sequences_denied: usize,
    /// Mid-sequence reservation extensions refused for want of budget (P0-13: one per
    /// `tools/list` page a walk could not buy).
    ///
    /// Separate from [`Self::sequences_denied`] because the two are different findings: a
    /// refused *sequence* means a host was never contacted at all, while a refused
    /// *extension* means a host was contacted and its tool list came back **truncated** —
    /// which is a caveat on that server's coverage numbers, not an absence of them. Pooling
    /// them would hide the second inside the first.
    pub page_extensions_denied: usize,
    /// Requests sent beyond a permit's reservation, across the run. Should be zero — a
    /// paginating walk extends its reservation rather than exceeding it.
    pub overshoot_requests: usize,
    /// The largest per-domain request count.
    pub max_requests_to_one_registrable_domain: usize,
    /// How many distinct registrable domains received at least one request.
    pub registrable_domains_contacted: usize,
    /// Every domain seen, most-loaded first.
    pub domains: Vec<DomainAccounting>,
}

/// The registrable domain of an endpoint URL — the grouping key for both controls.
///
/// Extracts the host itself (no URL crate is in this crate's dependency graph, and the
/// input is registry-supplied and therefore untrusted) and then applies
/// [`registrable_domain`].
#[must_use]
pub fn registrable_domain_of_endpoint(endpoint: &str) -> String {
    registrable_domain(&host_of_endpoint(endpoint))
}

/// The host component of an endpoint URL, with scheme, userinfo, port, path, query and
/// fragment removed. An IPv6 literal keeps its brackets.
///
/// Returned **as written**: case folding, length bounding and byte filtering all happen
/// once, in [`sanitise_key`], so a key is normalised in exactly one place.
///
/// Hand-rolled rather than pulled from a URL crate: this is the only place in the harness
/// that needs it, the input is untrusted, and the output is sanitised by
/// [`registrable_domain`] regardless of what this extracts.
fn host_of_endpoint(endpoint: &str) -> String {
    let after_scheme = endpoint.split_once("://").map_or(endpoint, |(_, rest)| rest);
    // Authority ends at the first path, query or fragment delimiter.
    let authority = after_scheme
        .find(['/', '?', '#'])
        .map_or(after_scheme, |i| &after_scheme[..i]);
    // Userinfo may itself contain '@', so the *last* one wins — taking the first would let
    // `http://evil.test@real.test/` be read as host `evil.test`.
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    if let Some(rest) = host_port.strip_prefix('[') {
        // IPv6 literal: everything up to the closing bracket, brackets kept so it can never
        // be confused with a dotted name.
        let inner = rest.split_once(']').map_or(rest, |(inner, _)| inner);
        return format!("[{inner}]");
    }
    host_port.split_once(':').map_or(host_port, |(host, _)| host).to_string()
}

/// The registrable domain of a host, by the heuristic [ADR-014] §2 records.
///
/// - An IP literal, or anything with fewer than two labels, is its own key — there is no
///   registrable domain to take.
/// - Otherwise the last two labels, or the last three when the second-to-last is one of a
///   fixed list of generic registry second levels (`REGISTRY_SECOND_LEVELS`, in this
///   module's source).
///
/// The result is lowercased, trimmed of a trailing root dot, bounded to the last 253 bytes
/// (the DNS presentation limit), and reduced to `[a-z0-9.\-_:\[\]]`, because it becomes a
/// published value and part of a failure detail.
///
/// Lowercasing and byte substitution can only make the grouping *coarser*, never finer,
/// which is the safe direction for a politeness control. Length bounding is **unreachable**
/// rather than coarse: a host long enough to hit it exceeds the DNS presentation limit, so
/// this transport could never have resolved or connected to it. Keeping the *last* 253 bytes
/// rather than the first is what makes that honest — the registrable domain is a
/// suffix, so truncating from the front would have split one registrant across many keys
/// (finer, the unsafe direction) instead of merging several into one.
///
/// [ADR-014]: ../../../docs/adr/014-request-pacing-and-domain-budget.md
#[must_use]
pub fn registrable_domain(host: &str) -> String {
    let host = sanitise_key(host);
    if host.is_empty() {
        return "<no-host>".to_string();
    }
    // An IPv6 literal keeps its brackets; an IPv4 literal is all-numeric labels. Either way
    // there are no registry labels to strip, and stripping would merge unrelated addresses.
    if host.starts_with('[') || is_ipv4_literal(&host) {
        return host;
    }
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() <= 2 {
        return labels.join(".");
    }
    let take = if REGISTRY_SECOND_LEVELS.contains(&labels[labels.len() - 2]) { 3 } else { 2 };
    labels[labels.len() - take..].join(".")
}

fn is_ipv4_literal(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    labels.len() == 4
        && labels.iter().all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()))
}

/// Lowercase, strip the DNS root dot, bound the length, and keep only the byte set a
/// hostname may legitimately use — anything else becomes `_`.
///
/// Substitution rather than rejection: a key that cannot be stored is a host that cannot be
/// paced, and the replacement can only merge keys together (coarser grouping, fewer
/// requests). The practical case is an internationalised domain that was never
/// punycode-encoded, which this transport could not have connected to anyway.
///
/// **The bound keeps the last [`MAX_KEY_BYTES`], not the first.** The registrable domain is
/// a *suffix*, so dropping the tail would have turned one over-long registrant into an
/// unbounded family of distinct keys — finer grouping, the one direction this control must
/// never fail in, since each key carries its own full budget. Keeping the suffix instead
/// means the only thing an over-long host can do is share a key with its own siblings. The
/// case is unreachable in practice (253 bytes *is* the DNS presentation limit, so such a
/// host cannot be resolved, let alone connected to), which is why this is a correctness
/// tidy rather than a fix.
fn sanitise_key(host: &str) -> String {
    let trimmed = host.trim().trim_end_matches('.');
    let mut out: String = trimmed
        .chars()
        .map(|ch| {
            let lower = ch.to_ascii_lowercase();
            if lower.is_ascii_alphanumeric() || matches!(lower, '.' | '-' | '_' | ':' | '[' | ']')
            {
                lower
            } else {
                '_'
            }
        })
        .collect();
    if out.len() > MAX_KEY_BYTES {
        // Char-boundary safe: every character above is ASCII by construction, so any byte
        // index is a boundary.
        out = out.split_off(out.len() - MAX_KEY_BYTES);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_is_extracted_from_every_url_shape_a_registry_can_hand_us() {
        let cases = [
            ("https://mcp.example.com/mcp", "mcp.example.com"),
            ("http://mcp.example.com:8080/mcp?x=1#y", "mcp.example.com"),
            ("mcp.example.com/mcp", "mcp.example.com"),
            // Returned as written: case folding and the root dot are normalised once, in
            // `sanitise_key`, which the assertion after this loop covers.
            ("https://MCP.Example.COM./mcp", "MCP.Example.COM."),
            ("https://user:pw@mcp.example.com/mcp", "mcp.example.com"),
            // The *last* '@' wins: taking the first would read this as `evil.test`.
            ("https://evil.test@real.test/mcp", "real.test"),
            ("https://[2001:db8::1]:8443/mcp", "[2001:db8::1]"),
            ("https://192.0.2.7/mcp", "192.0.2.7"),
            ("", ""),
        ];
        for (url, expected) in cases {
            assert_eq!(host_of_endpoint(url), expected, "host of {url}");
        }
        assert_eq!(
            registrable_domain_of_endpoint("https://MCP.Example.COM./mcp"),
            "example.com",
            "case and the DNS root dot are normalised once, in the key"
        );
    }

    #[test]
    fn the_registrable_domain_groups_one_vendor_behind_many_hosts() {
        // The exact shape the July corpus holds: 18 distinct hosts, one vendor.
        assert_eq!(registrable_domain("barcode-generator.api.klymax402.com"), "klymax402.com");
        assert_eq!(registrable_domain("base64-codec.api.klymax402.com"), "klymax402.com");
        assert_eq!(registrable_domain("gateway.pipeworx.io"), "pipeworx.io");
        assert_eq!(registrable_domain("server.smithery.ai"), "smithery.ai");
    }

    #[test]
    fn a_cctld_second_level_is_not_mistaken_for_a_registrable_domain() {
        assert_eq!(registrable_domain("mcp.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("api.example.com.br"), "example.com.br");
        assert_eq!(registrable_domain("x.y.example.ac.jp"), "example.ac.jp");
        // From the July corpus: `com.ai` is a real registry second level, so this is one
        // vendor with three hosts rather than three vendors.
        assert_eq!(registrable_domain("lenny-rachitsky.run.mcp.com.ai"), "mcp.com.ai");
        assert_eq!(registrable_domain("contabo.run.mcp.com.ai"), "mcp.com.ai");
    }

    /// The documented error direction: a registry second level this list does not know
    /// groups *more* hosts together, never fewer. `sampa.br` is a real ICANN suffix and is
    /// not in the list, and the three July hosts under it share one key as a result.
    #[test]
    fn an_unknown_registry_second_level_errs_coarse_not_fine() {
        for host in ["moema.sampa.br", "campobelo.sampa.br", "vilabuarque.sampa.br"] {
            assert_eq!(registrable_domain(host), "sampa.br", "{host}");
        }
    }

    /// Shared-hosting suffixes are the heuristic's known over-grouping: these are 22
    /// distinct registrants in the July corpus and one key here. Pinned as a test so the
    /// behaviour is a recorded property rather than a surprise in a sweep.
    #[test]
    fn shared_hosting_suffixes_over_group_and_that_is_recorded() {
        assert_eq!(registrable_domain("a-mcp.vercel.app"), "vercel.app");
        assert_eq!(registrable_domain("b-mcp.vercel.app"), "vercel.app");
        assert_eq!(registrable_domain("cf-worker.selbyventurecap.workers.dev"), "workers.dev");
        assert_eq!(registrable_domain("clipzyn-mcp-production.up.railway.app"), "railway.app");
    }

    #[test]
    fn an_ip_literal_is_its_own_key_and_is_never_split() {
        assert_eq!(registrable_domain("192.0.2.7"), "192.0.2.7");
        assert_eq!(registrable_domain("198.51.100.9"), "198.51.100.9");
        assert_eq!(registrable_domain("[2001:db8::1]"), "[2001:db8::1]");
        assert_ne!(
            registrable_domain("192.0.2.7"),
            registrable_domain("203.0.113.7"),
            "two unrelated addresses must not share a budget"
        );
    }

    /// The live corpus shape that would break a careless parse: `server.smithery.ai`
    /// appears 22 times in the committed July file, always as
    /// `https://server.smithery.ai/@owner/tool/mcp`. The `@` is in the **path**, not in
    /// userinfo, so authority-before-userinfo is the only order that gets the key right —
    /// splitting on `@` first would read the host as `owner/tool/mcp` and shred the key,
    /// silently giving 22 entries 22 separate budgets instead of one shared one.
    #[test]
    fn an_at_sign_in_the_path_does_not_reach_the_host() {
        for url in [
            "https://server.smithery.ai/@blbl147/xhs-mcp/mcp",
            "https://server.smithery.ai/@owner/tool/mcp?api_key=x",
            "https://server.smithery.ai/@a/@b/@c/mcp#frag",
        ] {
            assert_eq!(host_of_endpoint(url), "server.smithery.ai", "host of {url}");
            assert_eq!(registrable_domain_of_endpoint(url), "smithery.ai", "key of {url}");
        }
        // Real userinfo still works, and the *last* `@` in the authority wins.
        assert_eq!(
            registrable_domain_of_endpoint("https://user:pw@server.smithery.ai/@owner/t/mcp"),
            "smithery.ai"
        );
        assert_eq!(
            registrable_domain_of_endpoint("https://evil.test@real.test/@owner/t/mcp"),
            "real.test",
            "taking the first `@` would read the host as `evil.test`"
        );
        // All 22 July entries must land on one key, or the budget they share is 22 budgets.
        let keys: std::collections::BTreeSet<String> = (0..22)
            .map(|i| {
                registrable_domain_of_endpoint(&format!(
                    "https://server.smithery.ai/@owner{i}/tool{i}/mcp"
                ))
            })
            .collect();
        assert_eq!(keys.len(), 1, "22 shared-host entries must share one key, got {keys:?}");
    }

    #[test]
    fn a_hostile_host_string_is_bounded_and_sanitised_before_becoming_a_key() {
        let long = format!("{}.example.com", "a".repeat(4096));
        let key = registrable_domain(&long);
        assert!(key.len() <= MAX_KEY_BYTES, "key is {} bytes", key.len());
        // Bounding keeps the **suffix**, so an over-long host can only ever share a key
        // with its own siblings. Truncating the front instead would have split one
        // registrant into an unbounded family of keys, each carrying its own full budget —
        // finer grouping, which is the one direction this control must not fail in.
        assert_eq!(key, "example.com", "the registrable suffix must survive the bound");
        let sibling = registrable_domain(&format!("{}.example.com", "b".repeat(4096)));
        assert_eq!(key, sibling, "two over-long hosts of one registrant must share a key");

        let nasty = registrable_domain("ev\u{1b}[31mil\n.exa mple.com");
        assert!(
            nasty.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:[]".contains(&b)),
            "sanitised key still holds unexpected bytes: {nasty:?}"
        );
        assert_eq!(registrable_domain(""), "<no-host>");
        assert_eq!(registrable_domain("..."), "<no-host>");
    }

    fn pacer(interval_ms: u64, cap: usize) -> Arc<Pacer> {
        Arc::new(Pacer::new(PacePolicy {
            min_interval: Duration::from_millis(interval_ms),
            max_requests_per_registrable_domain: cap,
        }))
    }

    /// The core pacing claim: within one sequence the first request is immediate and every
    /// later one is spaced. Asserted on observed wall-clock, with the tolerance one-sided
    /// (at least the interval) so a slow machine cannot make it flaky.
    #[test]
    fn requests_within_one_sequence_are_spaced_and_the_first_is_not() {
        let pacer = pacer(60, 100);
        let permit = pacer.admit("https://mcp.example.com/mcp").expect("budget available");

        let start = Instant::now();
        permit.before_request();
        assert!(
            start.elapsed() < Duration::from_millis(60),
            "a domain's first request must not wait, waited {:?}",
            start.elapsed()
        );

        let before_second = Instant::now();
        permit.before_request();
        permit.before_request();
        let spent = before_second.elapsed();
        assert!(
            spent >= Duration::from_millis(120),
            "two further requests must cost two intervals, cost {spent:?}"
        );
        assert_eq!(permit.requests_sent(), 3);
        // `> ZERO`, not a tight lower bound. The wall-clock assertion above is the real
        // claim. `waited()` sums `max(0, slot − now)` — the spacing this module *had to
        // add* — so a loaded machine makes it **smaller**, not larger: that is the
        // documented "a slow host costs no extra spacing at all" property of
        // `before_request`, and a `>= 110 ms` bound here asserted its negation and failed
        // deterministically under CPU saturation.
        assert!(permit.waited() > Duration::ZERO, "waited {:?}", permit.waited());
    }

    /// The "do not slow the sweep down" requirement: unrelated domains never wait on each
    /// other, so a sequential sweep over distinct vendors pays nothing between them.
    #[test]
    fn unrelated_domains_never_wait_on_each_other() {
        let pacer = pacer(500, 100);
        let start = Instant::now();
        for i in 0..20 {
            let permit =
                pacer.admit(&format!("https://mcp.vendor{i}.test/mcp")).expect("budget available");
            permit.before_request();
        }
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "20 distinct domains, one request each, must not sleep at all; took {:?}",
            start.elapsed()
        );
        assert_eq!(pacer.accounting().waited, Duration::ZERO);
    }

    /// Two registry entries on *different hosts of one vendor* are spaced from each other.
    /// This is the eTLD+1 half of the pacing claim, and the reason the key is not the host.
    #[test]
    fn two_entries_on_one_vendors_distinct_hosts_are_still_spaced() {
        let pacer = pacer(60, 100);
        let first = pacer.admit("https://a.api.vendor.test/mcp").expect("budget");
        first.before_request();
        drop(first);

        let start = Instant::now();
        let second = pacer.admit("https://b.api.vendor.test/mcp").expect("budget");
        second.before_request();
        assert!(
            start.elapsed() >= Duration::from_millis(50),
            "a second host of the same vendor must still be spaced, waited {:?}",
            start.elapsed()
        );
    }

    /// Exhaustion is **recorded**, not silent, and the record names the domain, the counts
    /// and the cap.
    #[test]
    fn budget_exhaustion_is_refused_with_a_reason_rather_than_skipped() {
        let pacer = pacer(0, 10);
        // Two sequences of five requests each exactly fills a cap of 10.
        for _ in 0..2 {
            let permit = pacer.admit("https://mcp.vendor.test/mcp").expect("budget available");
            for _ in 0..5 {
                permit.before_request();
            }
        }
        let denied = pacer
            .admit("https://other.vendor.test/mcp")
            .expect_err("the third sequence must be refused");
        assert_eq!(denied.registrable_domain, "vendor.test");
        assert_eq!((denied.charged, denied.reserved, denied.cap), (10, 0, 10));
        assert!(denied.to_string().contains("no request was made to this host"), "{denied}");

        let accounting = pacer.accounting();
        assert_eq!(accounting.sequences_denied, 1);
        assert_eq!(accounting.requests_sent, 10);
        assert_eq!(accounting.max_requests_to_one_registrable_domain, 10);
        let row = accounting
            .domains
            .iter()
            .find(|d| d.registrable_domain == "vendor.test")
            .expect("the domain appears in per-domain accounting");
        assert_eq!((row.sequences_admitted, row.sequences_denied), (2, 1));
    }

    /// The cap is **hard**: the reservation is taken at admission, so no sequence can push
    /// the total past it even by sending its full allowance.
    #[test]
    fn the_cap_is_never_overshot_even_when_every_sequence_spends_its_whole_allowance() {
        let cap = 120;
        let pacer = pacer(0, cap);
        let mut admitted = 0;
        for _ in 0..64 {
            let Ok(permit) = pacer.admit("https://mcp.vendor.test/mcp") else { break };
            admitted += 1;
            for _ in 0..MAX_REQUESTS_PER_DISCOVERY {
                permit.before_request();
            }
        }
        assert_eq!(admitted, 24, "cap 120 with a 5-request reservation admits 24 sequences");
        let accounting = pacer.accounting();
        assert_eq!(accounting.requests_sent, 120);
        assert!(accounting.requests_sent <= cap);
        assert_eq!(accounting.overshoot_requests, 0);
    }

    /// The July corpus's outlier, reproduced: 149 entries on one host at the real
    /// per-sequence cost those entries had (an SSE probe plus one legacy handshake attempt),
    /// against the default cap. 58 admitted, 91 refused — and every refusal is a record.
    #[test]
    fn the_julys_149_entry_outlier_is_cut_off_at_the_default_cap() {
        let pacer = Arc::new(Pacer::new(PacePolicy {
            min_interval: Duration::ZERO,
            ..PacePolicy::CENSUS_DEFAULT
        }));
        let (mut admitted, mut denied) = (0, 0);
        for _ in 0..149 {
            match pacer.admit("https://gateway.pipeworx.io/mcp") {
                Ok(permit) => {
                    admitted += 1;
                    permit.before_request();
                    permit.before_request();
                }
                Err(_) => denied += 1,
            }
        }
        assert_eq!((admitted, denied), (58, 91));
        let accounting = pacer.accounting();
        assert_eq!(accounting.requests_sent, 116);
        assert_eq!(accounting.sequences_denied, 91);
        assert_eq!(admitted + denied, 149, "every entry is accounted for, none skipped");
    }

    /// Every group in the July corpus other than that outlier is 22 entries or fewer, and
    /// the default cap admits 24 full sequences — so the cap binds on the outlier and
    /// nowhere else. This is the arithmetic `PacePolicy::CENSUS_DEFAULT` is justified by.
    #[test]
    fn the_default_cap_admits_every_other_july_group_in_full() {
        let pacer = Arc::new(Pacer::new(PacePolicy {
            min_interval: Duration::ZERO,
            ..PacePolicy::CENSUS_DEFAULT
        }));
        for i in 0..22 {
            let permit = pacer
                .admit("https://mcp.vendor.test/mcp")
                .unwrap_or_else(|e| panic!("entry {i} of a 22-entry group must be admitted: {e}"));
            for _ in 0..MAX_REQUESTS_PER_DISCOVERY {
                permit.before_request();
            }
        }
        assert_eq!(pacer.accounting().sequences_denied, 0);
    }

    /// A dropped permit returns its reservation, so an abandoned sequence does not strand
    /// budget for the rest of the run.
    #[test]
    fn dropping_a_permit_releases_its_reservation() {
        let pacer = pacer(0, MAX_REQUESTS_PER_DISCOVERY);
        let permit = pacer.admit("https://mcp.vendor.test/mcp").expect("first fits exactly");
        assert!(pacer.admit("https://mcp.vendor.test/mcp").is_err(), "no headroom while held");
        drop(permit);
        assert!(pacer.admit("https://mcp.vendor.test/mcp").is_ok(), "reservation returned on drop");
        assert_eq!(pacer.accounting().requests_sent, 0);
    }

    /// Overshoot — a sequence sending more than the reservation — is counted rather than
    /// ignored, so `tools/list` pagination landing later shows up as a published number
    /// instead of silently eroding the cap.
    #[test]
    fn a_sequence_that_exceeds_its_reservation_is_counted_as_overshoot() {
        let pacer = pacer(0, 1_000);
        {
            let permit = pacer.admit("https://mcp.vendor.test/mcp").expect("budget");
            for _ in 0..MAX_REQUESTS_PER_DISCOVERY + 3 {
                permit.before_request();
            }
            assert_eq!(permit.overshoot(), 3);
        }
        assert_eq!(pacer.accounting().overshoot_requests, 3);
    }

    /// Under concurrency the cap still holds and requests to one domain are still spaced
    /// from each other — the lock is released around the sleep, but the *schedule* is
    /// committed under it, so concurrent waiters do not all wake at once.
    #[test]
    fn concurrent_sequences_against_one_domain_respect_both_controls() {
        let pacer = pacer(30, 20);
        let admitted = std::sync::atomic::AtomicUsize::new(0);
        let start = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    if let Ok(permit) = pacer.admit("https://mcp.vendor.test/mcp") {
                        admitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        for _ in 0..4 {
                            permit.before_request();
                        }
                    }
                });
            }
        });
        let accounting = pacer.accounting();
        let admitted = admitted.load(std::sync::atomic::Ordering::SeqCst);
        // How *many* sequences get in is scheduling-dependent: a reservation released early
        // frees headroom a later thread can take. So the count is asserted as a range. What
        // is not scheduling-dependent, and is the whole point of the test, is that the cap
        // holds, that every refusal is counted, and that every request was still spaced.
        assert!(
            (3..=4).contains(&admitted),
            "cap 20 with a 5-request reservation admits 3 or 4 of 8, saw {admitted}"
        );
        assert_eq!(accounting.requests_sent, admitted * 4);
        assert!(accounting.requests_sent <= 20, "the cap holds under concurrency");
        assert_eq!(accounting.sequences_denied, 8 - admitted, "every refusal is counted");
        let gaps = (accounting.requests_sent - 1) as u64;
        assert!(
            start.elapsed() >= Duration::from_millis(gaps * 30),
            "concurrent requests to one domain must still be spaced; {} requests took {:?}",
            accounting.requests_sent,
            start.elapsed()
        );
    }

    /// **B1.** Every agent this module builds refuses to follow a redirect, paced or not.
    ///
    /// Asserted against `ureq`'s own config getter rather than against the builder call,
    /// because the defect this closes was precisely that the builder call was *absent* at
    /// two call sites and `ureq`'s default of 10 applied silently. A test that re-reads the
    /// line it is checking would not have caught that; this reads what the agent ended up
    /// with.
    #[test]
    fn no_agent_this_module_builds_will_follow_a_redirect() {
        for statuses in [StatusHandling::AsResponse, StatusHandling::AsError] {
            let unpaced = PacedAgent::unpaced(Duration::from_secs(1), statuses);
            assert_eq!(
                unpaced.agent.config().max_redirects(),
                0,
                "an unpaced agent ({statuses:?}) would follow redirects"
            );
            assert!(unpaced.permit().is_none(), "unpaced must mean unpaced");

            let pacer = pacer(0, 100);
            let permit = pacer.admit("https://mcp.example.com/mcp").expect("budget");
            let paced = permit.into_agent(Duration::from_secs(1), statuses);
            assert_eq!(
                paced.agent.config().max_redirects(),
                0,
                "a paced agent ({statuses:?}) would follow redirects"
            );
            assert!(paced.permit().is_some(), "a paced agent must carry its permit");
        }
        assert_eq!(MAX_REDIRECTS, 0, "the published policy value must match the built agent");
    }

    /// `StatusHandling` is preserved per caller, because the two HTTP clients in this
    /// workspace need different answers and silently changing either one's answer would move
    /// records between published failure categories.
    #[test]
    fn status_handling_is_whatever_the_caller_asked_for() {
        let as_response = PacedAgent::unpaced(Duration::from_secs(1), StatusHandling::AsResponse);
        assert!(
            !as_response.agent.config().http_status_as_error(),
            "`discovery` must be able to read a 4xx body (ADR-013)"
        );
        let as_error = PacedAgent::unpaced(Duration::from_secs(1), StatusHandling::AsError);
        assert!(
            as_error.agent.config().http_status_as_error(),
            "`probe` records a non-2xx as `http status: NNN` through `ProbeError::Transport`"
        );
    }

    /// A reservation that is not `discover()`'s is honoured as the admission unit, so a
    /// probe protocol cannot quietly erode the cap for whatever runs next on that domain.
    #[test]
    fn a_custom_reservation_is_the_admission_unit_and_bounds_overshoot() {
        let pacer = pacer(0, 60);
        let big = pacer.admit_with_reservation("https://mcp.vendor.test/mcp", 26).expect("fits");
        assert_eq!(big.reservation(), 26);
        // 26 reserved; a second 26 fits (52 <= 60), a third does not.
        let second = pacer.admit_with_reservation("https://mcp.vendor.test/mcp", 26).expect("fits");
        assert!(
            pacer.admit_with_reservation("https://mcp.vendor.test/mcp", 26).is_err(),
            "a third 26-request reservation must not fit under a cap of 60"
        );
        // Overshoot is measured against *this* permit's reservation, not a shared constant.
        for _ in 0..26 {
            big.before_request();
        }
        assert_eq!(big.overshoot(), 0, "26 requests under a 26-request reservation is not overshoot");
        big.before_request();
        assert_eq!(big.overshoot(), 1);
        drop(second);
        drop(big);
        assert_eq!(pacer.accounting().overshoot_requests, 1);
    }

    /// A zero reservation would admit an unbounded number of sequences against a spent
    /// domain, so it is clamped rather than trusted.
    #[test]
    fn a_zero_reservation_is_clamped_so_the_cap_cannot_be_bypassed() {
        let pacer = pacer(0, 2);
        let a = pacer.admit_with_reservation("https://mcp.vendor.test/mcp", 0).expect("first");
        let b = pacer.admit_with_reservation("https://mcp.vendor.test/mcp", 0).expect("second");
        assert_eq!((a.reservation(), b.reservation()), (1, 1));
        assert!(
            pacer.admit_with_reservation("https://mcp.vendor.test/mcp", 0).is_err(),
            "a cap of 2 must admit exactly two 1-request reservations"
        );
    }

    #[test]
    fn accounting_orders_domains_by_load_deterministically() {
        let pacer = pacer(0, 1_000);
        for (host, n) in [("https://a.test/", 1), ("https://b.test/", 3), ("https://c.test/", 3)] {
            let permit = pacer.admit(host).expect("budget");
            for _ in 0..n {
                permit.before_request();
            }
        }
        let accounting = pacer.accounting();
        let names: Vec<&str> =
            accounting.domains.iter().map(|d| d.registrable_domain.as_str()).collect();
        assert_eq!(names, vec!["b.test", "c.test", "a.test"], "by load, then by name");
    }

    // ---- P0-13: mid-sequence reservation extensions ----

    #[test]
    fn an_extension_grows_the_reservation_and_is_charged_to_the_same_domain() {
        let pacer = pacer(0, 100);
        let permit = pacer.admit("https://mcp.example.com/mcp").expect("budget");
        assert_eq!(permit.reservation(), MAX_REQUESTS_PER_DISCOVERY);

        permit.try_extend(1).expect("headroom available");
        permit.try_extend(2).expect("headroom available");
        assert_eq!(permit.reservation(), MAX_REQUESTS_PER_DISCOVERY + 3);

        // Eight requests under an eight-request reservation is not overshoot.
        for _ in 0..8 {
            permit.before_request();
        }
        assert_eq!(permit.requests_sent(), 8);
        assert_eq!(permit.overshoot(), 0);
        drop(permit);
        assert_eq!(pacer.accounting().overshoot_requests, 0);
        assert_eq!(pacer.accounting().page_extensions_denied, 0);
    }

    /// The cap stays **hard** across an extension: nothing is granted and nothing is charged
    /// when the domain has no room, which is what stops a paginating walk spending headroom
    /// reserved for a later host on the same domain.
    #[test]
    fn an_extension_beyond_the_cap_is_refused_and_grants_nothing() {
        // Cap 5: admission takes the whole budget, so no extension can fit.
        let pacer = pacer(0, 5);
        let permit = pacer.admit("https://mcp.example.com/mcp").expect("budget");
        let denied = permit.try_extend(1).expect_err("the cap must be hard");

        assert_eq!(denied.cap, 5);
        assert_eq!(denied.registrable_domain, "example.com");
        assert_eq!(
            permit.reservation(),
            MAX_REQUESTS_PER_DISCOVERY,
            "a refused extension must leave the reservation untouched"
        );

        let accounting = pacer.accounting();
        assert_eq!(accounting.page_extensions_denied, 1);
        assert_eq!(
            accounting.sequences_denied, 0,
            "a refused extension is not a refused sequence: one truncates a tool list, the \
             other means the host was never contacted"
        );
    }

    /// A refused extension must not consume the headroom a *later* sequence on the same
    /// domain is entitled to — the erosion the whole buy-ahead design exists to prevent.
    #[test]
    fn a_refused_extension_leaves_a_later_sequence_on_the_domain_admissible() {
        let pacer = pacer(0, 10);
        let first = pacer.admit("https://a.example.com/mcp").expect("budget");
        for _ in 0..5 {
            first.before_request();
        }
        // charged 5 + reserved 5 + 1 = 11 > 10.
        assert!(first.try_extend(1).is_err());
        drop(first);

        // 5 charged, 0 reserved: a second sequence still fits exactly.
        let second = pacer.admit("https://b.example.com/mcp").expect("the budget was not eroded");
        assert_eq!(second.reservation(), MAX_REQUESTS_PER_DISCOVERY);
        assert_eq!(pacer.accounting().page_extensions_denied, 1);
    }

    #[test]
    fn an_extension_of_zero_is_a_no_op_rather_than_an_error() {
        let pacer = pacer(0, 5);
        let permit = pacer.admit("https://mcp.example.com/mcp").expect("budget");
        permit.try_extend(0).expect("zero must never be refused");
        assert_eq!(permit.reservation(), MAX_REQUESTS_PER_DISCOVERY);
        assert_eq!(pacer.accounting().page_extensions_denied, 0);
    }

    /// The extension must be released with the permit, or a long walk strands headroom for
    /// the rest of the run and later hosts are refused for budget nobody is using.
    #[test]
    fn dropping_a_permit_releases_its_extensions_too() {
        // Cap 200: a share of 25, so twenty extensions fit and the release is what is tested.
        let pacer = pacer(0, 200);
        let permit = pacer.admit("https://mcp.example.com/mcp").expect("budget");
        for _ in 0..20 {
            permit.try_extend(1).expect("headroom");
        }
        assert_eq!(permit.reservation(), MAX_REQUESTS_PER_DISCOVERY + 20);
        drop(permit);

        // Nothing was ever sent, so the whole 200 must be available again — forty full
        // sequences after the one that reserved 25 and released it.
        for i in 0..40 {
            pacer
                .admit("https://mcp.example.com/mcp")
                .map(std::mem::forget)
                .unwrap_or_else(|_| panic!("sequence {i} refused: the extensions were stranded"));
        }
    }

    // ---- B2: one sequence's share of its domain ----

    /// A sequence admitted with a reservation larger than the share (Track B's probe) keeps
    /// its whole admission: the share is a ceiling on *extensions*, never a cut to what was
    /// admitted.
    #[test]
    fn the_share_never_cuts_below_an_admitted_reservation() {
        let pacer = pacer(0, 120);
        let permit =
            pacer.admit_with_reservation("https://mcp.example.com/mcp", 40).expect("budget");
        assert_eq!(permit.reservation(), 40);
        assert!(permit.try_extend(1).is_err(), "already at its own ceiling of 40");
        let row = &pacer.accounting().domains[0];
        assert_eq!(row.page_extensions_denied_by_sequence_share, 1);
    }

    /// **The measured B2 attack, closed.** On one shared registrable domain at the census
    /// cap, run H "hostile" listings that each paginate for as long as anything will grant
    /// them a page — up to `MAX_PAGES` — and spend everything they bought; then admit honest
    /// five-request sequences until the domain refuses one.
    ///
    /// Before the per-sequence share a hostile walk took 41 of 120 and the honest admissions
    /// measured 24 → 15 → 8 → 2 for H = 0, 1, 2, 4. With the share each walk holds at most
    /// 15, so the series is 24 → 21 → 18 → 12: one listing can no longer refuse more than
    /// three of its co-tenants, and four together cannot take half the domain.
    #[test]
    fn one_paginating_listing_cannot_starve_its_co_tenants_out_of_the_census() {
        fn honest_admitted_after(hostile: usize) -> (usize, Accounting) {
            let pacer = pacer(0, PacePolicy::CENSUS_DEFAULT.max_requests_per_registrable_domain);
            for h in 0..hostile {
                let walk = pacer
                    .admit(&format!("https://evil{h}.shared.app/mcp"))
                    .expect("a hostile listing is admitted like any other");
                for _ in 1..crate::paginate::MAX_PAGES {
                    if walk.try_extend(1).is_err() {
                        break;
                    }
                }
                for _ in 0..walk.reservation() {
                    walk.before_request();
                }
            }
            let mut honest = 0;
            while let Ok(seq) = pacer.admit(&format!("https://ok{honest}.shared.app/mcp")) {
                for _ in 0..MAX_REQUESTS_PER_DISCOVERY {
                    seq.before_request();
                }
                honest += 1;
            }
            (honest, pacer.accounting())
        }

        let series: Vec<usize> =
            [0, 1, 2, 4].iter().map(|&h| honest_admitted_after(h).0).collect();
        assert_eq!(series, vec![24, 21, 18, 12]);

        // And the displacement is visible in the published per-domain row rather than
        // inferred: what the hostile walks bought, and that it was their share that stopped
        // them, not the domain filling.
        let (_, accounting) = honest_admitted_after(1);
        let row = &accounting.domains[0];
        assert_eq!(row.registrable_domain, "shared.app");
        assert_eq!(row.page_extensions_granted, 10);
        assert_eq!(row.page_extensions_denied_by_sequence_share, 1);
        assert_eq!(row.largest_sequence_reservation, 15);
        assert_eq!(row.sequences_admitted, 1 + 21);
        assert_eq!(row.sequences_denied, 1);
    }
}
