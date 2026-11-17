//! How a strategy runs, one way for all: its switch and the live account at the start, then a loop ticking every
//! `tick_s` that steps the strategy when its step is due (a bar closed, or every so often) and checks its positions
//! every `check_s`. The strategies only say what a step and a check do (`Strategy`).

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;

use anyhow::Result;

use crate::api::now;
use crate::log;
use crate::venue::Venue;

/// When a strategy's step is due.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Due {
    /// On each bar of `len` s (from the epoch) once it closed and `after` s more went by; the step gets the bar's open.
    Bar { len: f64, after: f64 },
    /// Every `s` s from the start (0: every tick); the step gets the time.
    Every(f64),
}

/// A strategy's schedule.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Schedule {
    pub(crate) tick_s: f64,
    pub(crate) step: Due,
    /// The positions checked every `check_s` from the start (0: every tick).
    pub(crate) check_s: f64,
}

/// A strategy the runner runs: a step when it is due, the positions' check.
pub(crate) trait Strategy: Send + 'static {
    fn schedule(&self) -> Schedule;
    /// The last bar stepped (`Due::Bar`): a bar after it is due.
    fn stepped(&self) -> f64 {
        0.0
    }
    fn step(&mut self, at: f64) -> impl Future<Output = ()> + Send;
    /// The positions' check (none: a strategy that checks them in its step).
    fn check(&mut self) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// The open of the last bar of `len` s closed `after` s or more before `t`.
pub(crate) fn last_closed(t: f64, len: f64, after: f64) -> f64 {
    ((t - after) / len).floor() * len - len
}

/// Something done every `s` s: due at once, then `s` s after it was last done.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Every {
    s: f64,
    at: Option<f64>,
}

impl Every {
    pub(crate) fn new(s: f64) -> Every {
        Every { s, at: None }
    }

    /// Counted as done at `t` already: first due `s` after it.
    pub(crate) fn since(s: f64, t: f64) -> Every {
        Every { s, at: Some(t) }
    }

    /// Due at `t` (and then counted as done).
    pub(crate) fn due(&mut self, t: f64) -> bool {
        let due = self.ready(t);
        if due {
            self.done(t);
        }
        due
    }

    /// Due at `t`, not counted as done (see `done`: something counted only when it worked).
    pub(crate) fn ready(&self, t: f64) -> bool {
        self.at.is_none_or(|at| t - at >= self.s)
    }

    pub(crate) fn done(&mut self, t: f64) {
        self.at = Some(t);
    }
}

/// One tick at `now()`: the step if it is due, then the check if it is due.
pub(crate) async fn tick<S: Strategy>(s: &mut S, step: &mut Every, check: &mut Every) {
    let sc = s.schedule();
    let t = now();
    match sc.step {
        Due::Bar { len, after } => {
            let bar = last_closed(t, len, after);
            if bar > s.stepped() {
                s.step(bar).await;
            }
        }
        Due::Every(_) => {
            if step.due(t) {
                s.step(t).await;
            }
        }
    }
    if check.due(now()) {
        s.check().await;
    }
}

/// The strategy `name` run in the background, ticking on its schedule; each tick done is its pulse (`beat`).
pub(crate) fn spawn<S: Strategy>(name: &'static str, mut s: S) {
    let sc = s.schedule();
    born(name, sc.tick_s);
    tokio::spawn(async move {
        let mut step = Every::new(match sc.step { Due::Every(e) => e, Due::Bar { .. } => 0.0 });
        let mut check = Every::new(sc.check_s);
        let mut iv = tokio::time::interval(std::time::Duration::from_secs_f64(sc.tick_s));
        loop {
            iv.tick().await;
            let t0 = now();
            tick(&mut s, &mut step, &mut check).await;
            beat(name, t0);
        }
    });
}

/// A loop's pulse (for `health`): its tick (s), when it started, when its last tick was done (0: none yet), its ticks
/// done, its slowest tick (s).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub(crate) struct Pulse {
    pub(crate) tick_s: f64,
    pub(crate) since: f64,
    pub(crate) done: f64,
    pub(crate) ticks: u64,
    pub(crate) slowest_s: f64,
}

static PULSES: Mutex<BTreeMap<&'static str, Pulse>> = Mutex::new(BTreeMap::new());

/// The loop `name` started now, ticking every `tick_s`.
pub(crate) fn born(name: &'static str, tick_s: f64) {
    PULSES.lock().unwrap().insert(name, Pulse { tick_s, since: now(), done: 0.0, ticks: 0, slowest_s: 0.0 });
}

/// The loop `name`'s tick begun at `t0` done now.
pub(crate) fn beat(name: &'static str, t0: f64) {
    let t = now();
    if let Some(p) = PULSES.lock().unwrap().get_mut(name) {
        p.done = t;
        p.ticks += 1;
        p.slowest_s = p.slowest_s.max(t - t0);
    }
}

/// Every loop's pulse.
pub(crate) fn pulses() -> Vec<(&'static str, Pulse)> {
    PULSES.lock().unwrap().iter().map(|(n, p)| (*n, *p)).collect()
}

/// Off when its switch `var` is "0" (logged so).
pub(crate) fn off(kind: &str, var: &str) -> bool {
    let off = crate::trades::switch(var).is_none();
    if off {
        log!("{kind}: off ({var}=0)");
    }
    off
}

/// The live account (and whether it is unified), or none (logged: the strategy stays off) without its keys.
pub(crate) async fn venue(kind: &str) -> Result<Option<(Venue, bool)>> {
    let v = crate::venue::venue_from_env(None).await?;
    if v.is_none() {
        log!("{kind}: LIVE_KEY / LIVE_ACCOUNT not set, off");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Clock;

    const H: f64 = 3600.0;

    #[test]
    fn the_last_closed_bar_once_its_wait_is_over() {
        // a 4h bar's step a minute after its close (squeeze): 08:00:59 still the 00:00 bar, 08:01 the 04:00 one
        assert_eq!(last_closed(8.0 * H + 59.0, 4.0 * H, 60.0), 0.0);
        assert_eq!(last_closed(8.0 * H + 60.0, 4.0 * H, 60.0), 4.0 * H);
        // an hour's 20 s after (breakout); a day's 5 min after (trend)
        assert_eq!(last_closed(10.0 * H + 19.0, H, 20.0), 8.0 * H);
        assert_eq!(last_closed(10.0 * H + 20.0, H, 20.0), 9.0 * H);
        assert_eq!(last_closed(3.0 * 86_400.0 + 300.0, 86_400.0, 300.0), 2.0 * 86_400.0);
        assert_eq!(last_closed(3.0 * 86_400.0 + 299.0, 86_400.0, 300.0), 86_400.0);
    }

    #[test]
    fn every_is_due_at_once_then_after_its_wait() {
        let mut e = Every::new(300.0);
        assert!(e.due(1000.0));
        assert!(!e.due(1299.0));
        assert!(e.due(1300.0));
        assert!(!e.due(1300.0));
        let mut z = Every::new(0.0);
        assert!(z.due(5.0) && z.due(5.0));
        // counted only when done
        let mut w = Every::new(60.0);
        assert!(w.ready(0.0) && w.ready(10.0));
        w.done(10.0);
        assert!(!w.ready(69.0) && w.ready(70.0));
        // from a start: first due a wait after it
        let s = Every::since(86_400.0, 100.0);
        assert!(!s.ready(100.0) && !s.ready(86_499.0) && s.ready(86_500.0));
    }

    #[tokio::test]
    async fn a_loop_s_pulse_its_start_then_each_tick_done_and_its_slowest() {
        let c = Clock::at(5000.0);
        born("pulse-test", 5.0);
        assert_eq!(pulses().into_iter().find(|x| x.0 == "pulse-test").unwrap().1, Pulse { tick_s: 5.0, since: 5000.0, done: 0.0, ticks: 0, slowest_s: 0.0 });
        c.go(3.0);
        beat("pulse-test", 4999.0); // a 4 s tick
        c.go(5.0);
        beat("pulse-test", 5007.0); // 1 s
        beat("never-born", 0.0); // a loop not started: nothing
        let p = pulses().into_iter().find(|x| x.0 == "pulse-test").unwrap().1;
        assert_eq!((p.done, p.ticks, p.slowest_s), (5008.0, 2, 4.0));
        assert!(pulses().iter().all(|x| x.0 != "never-born"));
    }

    #[tokio::test]
    async fn a_spawned_loop_ticks_and_beats() {
        let sc = Schedule { tick_s: 0.01, step: Due::Every(0.0), check_s: 0.0 };
        spawn("spawn-test", Rec { sc, stepped: 0.0, did: vec![] });
        for _ in 0..200 {
            if pulses().iter().any(|x| x.0 == "spawn-test" && x.1.ticks >= 2) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("no beat: {:?}", pulses());
    }

    /// A strategy that writes down what it did.
    struct Rec {
        sc: Schedule,
        stepped: f64,
        did: Vec<(char, f64)>,
    }

    impl Strategy for Rec {
        fn schedule(&self) -> Schedule {
            self.sc
        }
        fn stepped(&self) -> f64 {
            self.stepped
        }
        async fn step(&mut self, at: f64) {
            self.did.push(('s', at));
            self.stepped = at;
        }
        async fn check(&mut self) {
            self.did.push(('c', now()));
        }
    }

    #[tokio::test]
    async fn a_bar_strategy_steps_each_closed_bar_once_and_checks_on_its_own_clock() {
        let sc = Schedule { tick_s: 20.0, step: Due::Bar { len: 4.0 * H, after: 60.0 }, check_s: 300.0 };
        let mut r = Rec { sc, stepped: 0.0, did: vec![] };
        let (mut step, mut check) = (Every::new(0.0), Every::new(sc.check_s));
        let c = Clock::at(8.0 * H + 30.0);
        tick(&mut r, &mut step, &mut check).await; // the 00:00 bar: closed at 04:00 (stepped is 0, the same bar: not again)
        c.go(30.0); // 08:01: the 04:00 bar
        tick(&mut r, &mut step, &mut check).await;
        c.go(20.0);
        tick(&mut r, &mut step, &mut check).await; // nothing: the bar stepped, the check 50 s ago
        c.go(250.0);
        tick(&mut r, &mut step, &mut check).await; // the check, 300 s after the first
        assert_eq!(r.did, vec![('c', 8.0 * H + 30.0), ('s', 4.0 * H), ('c', 8.0 * H + 330.0)]);
    }

    #[tokio::test]
    async fn an_every_strategy_steps_at_once_then_on_its_wait() {
        // listing: a look every hour from the start, the check every 5 min; expiry: both every tick
        let sc = Schedule { tick_s: 30.0, step: Due::Every(H), check_s: 300.0 };
        let mut r = Rec { sc, stepped: 0.0, did: vec![] };
        let (mut step, mut check) = (Every::new(H), Every::new(sc.check_s));
        let c = Clock::at(1000.0);
        tick(&mut r, &mut step, &mut check).await;
        c.go(300.0);
        tick(&mut r, &mut step, &mut check).await;
        c.go(H - 300.0);
        tick(&mut r, &mut step, &mut check).await;
        assert_eq!(r.did, vec![('s', 1000.0), ('c', 1000.0), ('c', 1300.0), ('s', 1000.0 + H), ('c', 1000.0 + H)]);
        let sc = Schedule { tick_s: 30.0, step: Due::Every(0.0), check_s: 0.0 };
        let mut r = Rec { sc, stepped: 0.0, did: vec![] };
        let (mut step, mut check) = (Every::new(0.0), Every::new(0.0));
        tick(&mut r, &mut step, &mut check).await;
        tick(&mut r, &mut step, &mut check).await;
        assert_eq!(r.did.len(), 4);
    }
}
