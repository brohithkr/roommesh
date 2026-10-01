//! Real-time scheduling for the DSP thread (`roommesh-dsp`).
//!
//! An ordinary thread, even at user-interactive QoS, can wake late: under load, under App Nap
//! (the app is a menu-bar app with no window, so timers get coalesced), or behind other work.
//! When the DSP thread wakes late, the room speaker's output queue (30 ms) runs dry, far-end
//! frames miss their play time, and the room mic grid writes silence. So on macOS the thread:
//!
//! 1. takes the Mach **time-constraint policy** (`THREAD_TIME_CONSTRAINT_POLICY`): the kernel
//!    then schedules it ahead of every timeshare thread, by deadline, as CoreAudio's own I/O
//!    threads are;
//! 2. joins an **audio workgroup** (`AudioWorkIntervalCreate` + `os_workgroup_join`), and
//!    brackets each loop iteration with `os_workgroup_interval_start`/`_finish`, so the
//!    scheduler and the CPU performance controller see each wake's deadline (and can raise the
//!    clock or move the thread to a performance core when deadlines are at risk).
//!
//! Apple's order is followed: policy first, then join. The first `AudioWorkIntervalCreate` in a
//! process takes ~80 ms (AudioToolbox initialising itself; later ones take ~10 µs), so the
//! workgroup is created on a short-lived helper thread and handed over through a channel; the
//! DSP thread takes the policy at once and joins the workgroup when it arrives, outside a
//! measured interval ([`RealtimeThread::poll_workgroup`]). If the workgroup can't be created
//! or joined, the thread keeps the time-constraint policy alone ("time-constraint only"); if
//! the policy is refused, it stays at user-interactive QoS ("normal priority"). Each outcome
//! is logged once. The symbols exist since macOS 11 and the deployment target is 14.2, so they are
//! linked normally (no weak linking or `dlsym` needed). AudioToolbox is linked explicitly here
//! (cpal's `objc2-audio-toolbox` and the app's module map link it too).
//!
//! # Budget
//!
//! The loop wakes every 2 ms (`TICK`), or earlier for a message. Most wakes only move a few
//! buffers; once per 10 ms the coordinator produces a room-mic frame, which runs every enabled
//! mic through WebRTC AEC (+ noise suppression), the VAD and the scorer, and mixes. Measured on
//! an Apple Silicon Mac in release mode (`runtime::tests::dsp_iteration_cost`: coordinator +
//! room speaker, meeting audio playing, every mic carrying sound, WebRTC AEC + NS on), the
//! frame produced in one wake costs:
//!
//! | mics | frame wake p50 / p99 / max | other wakes p99 |
//! |------|----------------------------|-----------------|
//! | 1    | 184 / 239 / 271 µs         | 3 µs            |
//! | 4    | 723 / 880 / 983 µs         | 3 µs            |
//! | 8    | 1463 / 1718 / 1985 µs      | 3 µs            |
//!
//! That is ~200-220 µs per mic. With 8 mics one wake would need most of a 2 ms tick, which no
//! honest 2 ms budget covers. So the per-mic work of a frame is spread over consecutive wakes,
//! at most [`MICS_PER_WAKE`] (2) mics per wake (`CoordinatorPipeline::process_mics`). A frame
//! becomes due one frame (10 ms) ahead of its time; 8 mics finish on the 4th wake, ≤ 6 ms
//! later, still ≥ 4 ms ahead. A frame whose own time has come (a backlog after a stall) is
//! finished at once. Mic data is read at or after the moment it used to be, so spreading never
//! costs audio; the only effect is a steady phase shift of the mic ring's write edge, and the
//! driver reads 20 ms behind that edge. Spread, the heaviest wakes cost:
//!
//! | mics | p50 / p99 / max of wakes with room-mic work |
//! |------|---------------------------------------------|
//! | 1    | 190 / 243 / 305 µs                          |
//! | 4    | 435 / 628 / 718 µs                          |
//! | 8    | 256 / 911 / 1097 µs                         |
//!
//! Some of a frame isn't spread: the wake that finishes it also decodes, arbitrates and mixes
//! for every mic (~55 µs per mic). Hence the model [`wake_cost_ns`]: 50 µs + 55 µs per enabled
//! mic + 230 µs per mic processed in the wake (at most `MICS_PER_WAKE`) + 230 µs for the local
//! mic meter's echo canceller when it runs; it is above every measured p99 above.
//!
//! The policy values per wake are then (see [`budget`]):
//! - `period` = `TICK` (2 ms), the loop's cadence;
//! - `computation` = 1.5 × the modelled cost, at least half the constraint and at most all of
//!   it. The floor is the kernel's: it raises a smaller computation to constraint / 2 anyway
//!   (observed: 0.77 ms requested, 1.00 ms granted), so the budget asks for what it gets.
//!   Range: 1.00 ms (a member, or up to 3 mics) to 1.43 ms (8 mics), 1.77 ms with the meter;
//! - `constraint` = `TICK`: each wake's work is done before the next tick is due.
//!
//! The 1.5× headroom covers slower Macs and the AEC's occasional heavier frames, so a wake
//! rarely overruns its computation (an overrun only costs the thread its deadline order among
//! real-time threads for that wake). The thread blocks every iteration, far below the kernel's
//! fail-safe for runaway real-time threads, so it is never demoted for hogging the CPU. The
//! budget follows the mic count and the meter (re-applied, outside a measured interval, when
//! they change).
//!
//! The interval is started only after the iteration's housekeeping (control messages, which
//! may build pipelines and echo cancellers; device reconcile and opens; the virtual-device
//! monitor's `shm_open`/`mmap`; metrics and health publishing), so occasional setup work is not
//! counted against a deadline. Those costs are small anyway (an echo canceller: ~30 µs; an
//! 8-mic coordinator pipeline: ~260 µs), so they stay on the thread rather than moving to a
//! helper. Nothing in the loop takes a blocking lock that another thread holds for long.
//!
//! Demotion: the kernel may take real-time scheduling away from a thread (fail-safe, or when its
//! task is throttled). The thread polls its own scheduling policy once a second and counts
//! transitions from real-time back to timeshare ([`RealtimeThread::poll_demotion`]).
//!
//! `ROOMMESH_NO_REALTIME=1` in the environment skips steps 1 and 2 (for A/B diagnostics).
use std::time::Duration;

/// At most this many mics' per-frame processing in one wake (see the module docs).
pub const MICS_PER_WAKE: usize = 2;
/// Modelled p99 cost of a wake's fixed part (far-end, speaker), rounded up.
pub const BASE_NS: u64 = 50_000;
/// Modelled cost per enabled mic of the frame work that isn't spread (decode, arbitrate, mix).
pub const FRAME_PER_MIC_NS: u64 = 55_000;
/// Modelled p99 cost of one mic's processing (WebRTC AEC + NS, VAD, scoring), rounded up.
pub const PER_MIC_NS: u64 = 230_000;
/// Headroom over the measured cost, in percent.
pub const HEADROOM_PCT: u64 = 150;

/// Time-constraint policy values for one wake, in ns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtBudget {
    pub period_ns: u64,
    pub computation_ns: u64,
    pub constraint_ns: u64,
}

/// The modelled p99 cost of the heaviest wake (see the module docs) with `coordinator_mics`
/// enabled mics on this coordinator (0 elsewhere) and the local mic meter on or off.
pub fn wake_cost_ns(coordinator_mics: usize, meter: bool) -> u64 {
    let mics = coordinator_mics as u64;
    BASE_NS
        + FRAME_PER_MIC_NS * mics
        + PER_MIC_NS * mics.min(MICS_PER_WAKE as u64)
        + if meter { PER_MIC_NS } else { 0 }
}

/// The budget for a loop ticking every `tick` whose wakes cost up to `cost_ns`.
pub fn budget(tick: Duration, cost_ns: u64) -> RtBudget {
    let tick_ns = tick.as_nanos() as u64;
    // The kernel never grants less than half the constraint.
    let computation = (cost_ns * HEADROOM_PCT / 100).clamp(tick_ns / 2, tick_ns);
    RtBudget {
        period_ns: tick_ns,
        computation_ns: computation,
        constraint_ns: tick_ns,
    }
}

/// How the DSP thread is scheduled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RtStatus {
    /// An ordinary thread (user-interactive QoS on macOS).
    #[default]
    NormalPriority,
    /// The time-constraint policy, without an audio workgroup.
    TimeConstraintOnly,
    /// The time-constraint policy and an audio workgroup interval.
    WorkgroupAndTimeConstraint,
}

impl RtStatus {
    pub fn label(self) -> &'static str {
        match self {
            RtStatus::NormalPriority => "normal priority",
            RtStatus::TimeConstraintOnly => "time-constraint only",
            RtStatus::WorkgroupAndTimeConstraint => "workgroup + time-constraint",
        }
    }
}

/// Whether `ROOMMESH_NO_REALTIME` asks for an ordinary thread.
pub fn disabled_by_env() -> bool {
    std::env::var_os("ROOMMESH_NO_REALTIME").is_some_and(|v| !v.is_empty() && v != "0")
}

/// The calling thread's real-time scheduling. Created on the thread it applies to, and must be
/// dropped there (it leaves the workgroup), hence not `Send`.
pub struct RealtimeThread {
    status: RtStatus,
    budget: Option<RtBudget>,
    /// The thread was real-time at the last poll.
    was_rt: bool,
    demotions: u32,
    #[cfg(target_os = "macos")]
    wg: Option<mac::Workgroup>,
    /// The workgroup being prepared on the helper thread.
    #[cfg(target_os = "macos")]
    pending_wg: Option<crossbeam_channel::Receiver<Result<mac::PreparedWorkgroup, String>>>,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl RealtimeThread {
    /// Applies the time-constraint policy for `budget` to the calling thread and starts
    /// preparing its audio workgroup, which [`poll_workgroup`](Self::poll_workgroup) joins once
    /// ready. Falls back as described in the module docs; never fails.
    pub fn promote_current(budget: RtBudget) -> Self {
        let mut t = Self::normal();
        #[cfg(target_os = "macos")]
        {
            match mac::set_time_constraint(budget) {
                Ok(granted) => {
                    t.budget = Some(budget);
                    t.status = RtStatus::TimeConstraintOnly;
                    t.was_rt = true;
                    log::info!(
                        "roommesh-dsp: time-constraint policy set (period {:.2} ms, computation \
                         {:.2} ms, constraint {:.2} ms; kernel reports {granted})",
                        budget.period_ns as f64 / 1e6,
                        budget.computation_ns as f64 / 1e6,
                        budget.constraint_ns as f64 / 1e6,
                    );
                    t.pending_wg = Some(mac::PreparedWorkgroup::prepare());
                }
                Err(e) => log::warn!(
                    "roommesh-dsp: time-constraint policy refused ({e}); running at normal priority"
                ),
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = budget;
        t
    }

    /// An ordinary thread (real-time scheduling disabled).
    pub fn normal() -> Self {
        Self {
            status: RtStatus::NormalPriority,
            budget: None,
            was_rt: false,
            demotions: 0,
            #[cfg(target_os = "macos")]
            wg: None,
            #[cfg(target_os = "macos")]
            pending_wg: None,
            _not_send: std::marker::PhantomData,
        }
    }

    /// Joins the audio workgroup once the helper thread has prepared it (a channel poll until
    /// then). Returns whether a workgroup is still pending. Not for a measured interval.
    pub fn poll_workgroup(&mut self) -> bool {
        #[cfg(target_os = "macos")]
        {
            let Some(rx) = self.pending_wg.as_ref() else {
                return false;
            };
            let prepared = match rx.try_recv() {
                Ok(p) => p,
                Err(crossbeam_channel::TryRecvError::Empty) => return true,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    Err("the preparing thread stopped".into())
                }
            };
            self.pending_wg = None;
            match prepared.and_then(mac::Workgroup::join) {
                Ok(wg) => {
                    self.wg = Some(wg);
                    self.status = RtStatus::WorkgroupAndTimeConstraint;
                    log::info!("roommesh-dsp: joined an audio workgroup");
                }
                Err(e) => log::warn!(
                    "roommesh-dsp: no audio workgroup ({e}); continuing with the time-constraint \
                     policy only"
                ),
            }
        }
        false
    }

    pub fn status(&self) -> RtStatus {
        self.status
    }
    /// The budget in force (`None` at normal priority).
    pub fn budget(&self) -> Option<RtBudget> {
        self.budget
    }
    /// Times the kernel took real-time scheduling away (see [`poll_demotion`](Self::poll_demotion)).
    pub fn demotions(&self) -> u32 {
        self.demotions
    }

    /// Re-applies the time-constraint policy with `budget` if it differs from the one in force
    /// (only while real-time). Not for a measured interval: it is a system call.
    pub fn set_budget(&mut self, budget: RtBudget) {
        if self.status == RtStatus::NormalPriority || self.budget == Some(budget) {
            return;
        }
        #[cfg(target_os = "macos")]
        match mac::set_time_constraint(budget) {
            Ok(_) => self.budget = Some(budget),
            Err(e) => log::warn!("roommesh-dsp: updating the time-constraint policy failed ({e})"),
        }
    }

    /// Starts a measured interval now, due `constraint` from now. Returns whether one was
    /// started (then [`finish_interval`](Self::finish_interval) must follow).
    pub fn start_interval(&mut self) -> bool {
        #[cfg(target_os = "macos")]
        if let (Some(wg), Some(b)) = (self.wg.as_mut(), self.budget) {
            return wg.start(b.constraint_ns);
        }
        false
    }
    /// Ends the interval [`start_interval`](Self::start_interval) started.
    pub fn finish_interval(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(wg) = self.wg.as_mut() {
            wg.finish();
        }
    }

    /// Checks whether the thread is still scheduled real-time; counts (and logs) a demotion
    /// when it no longer is. Returns the current state. A system call: poll about once a second.
    pub fn poll_demotion(&mut self) -> bool {
        if self.status == RtStatus::NormalPriority {
            return false;
        }
        #[cfg(target_os = "macos")]
        {
            let Some(rt) = mac::is_realtime() else {
                return self.was_rt;
            };
            if self.was_rt && !rt {
                self.demotions += 1;
                log::warn!(
                    "roommesh-dsp: the system took real-time scheduling away ({} time(s))",
                    self.demotions
                );
            } else if !self.was_rt && rt {
                log::info!("roommesh-dsp: real-time scheduling restored");
            }
            self.was_rt = rt;
            rt
        }
        #[cfg(not(target_os = "macos"))]
        false
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::RtBudget;
    use crate::time::ns_to_host_ticks;
    use mach2::kern_return::KERN_SUCCESS;
    use mach2::mach_port::mach_port_deallocate;
    use mach2::thread_policy::{
        thread_policy_get, thread_policy_set, thread_time_constraint_policy_data_t,
        THREAD_TIME_CONSTRAINT_POLICY, THREAD_TIME_CONSTRAINT_POLICY_COUNT,
    };
    use mach2::traps::mach_task_self;
    use std::ffi::{c_char, c_int, c_void};

    type OsWorkgroup = *mut c_void;
    /// `OS_CLOCK_MACH_ABSOLUTE_TIME` from `<os/clock.h>`.
    const OS_CLOCK_MACH_ABSOLUTE_TIME: u32 = 32;
    /// `POLICY_TIMESHARE` from `<mach/policy.h>`.
    const POLICY_TIMESHARE: c_int = 1;

    /// `os_workgroup_join_token_s` (LP64 layout, `<os/workgroup_base.h>`).
    #[repr(C)]
    struct JoinToken {
        sig: u32,
        opaque: [u8; 36],
    }

    #[link(name = "AudioToolbox", kind = "framework")]
    extern "C" {
        fn AudioWorkIntervalCreate(
            name: *const c_char,
            clock: u32,
            attr: *mut c_void,
        ) -> OsWorkgroup;
    }
    extern "C" {
        fn os_workgroup_join(wg: OsWorkgroup, token_out: *mut JoinToken) -> c_int;
        fn os_workgroup_leave(wg: OsWorkgroup, token: *mut JoinToken);
        fn os_workgroup_interval_start(
            wg: OsWorkgroup,
            start: u64,
            deadline: u64,
            data: *mut c_void,
        ) -> c_int;
        fn os_workgroup_interval_finish(wg: OsWorkgroup, data: *mut c_void) -> c_int;
        fn os_release(object: *mut c_void);
    }

    /// Runs `f` with a send right to the calling thread (`mach_thread_self`), then releases it.
    fn with_self_thread<R>(f: impl FnOnce(mach2::mach_types::thread_t) -> R) -> R {
        let t = unsafe { mach2::mach_init::mach_thread_self() };
        let r = f(t);
        unsafe { mach_port_deallocate(mach_task_self(), t) };
        r
    }

    fn ticks(ns: u64) -> u32 {
        ns_to_host_ticks(ns).min(u32::MAX as u64) as u32
    }

    /// Sets the policy on the calling thread; returns what the kernel reports back.
    pub fn set_time_constraint(b: RtBudget) -> Result<String, String> {
        let mut p = thread_time_constraint_policy_data_t {
            period: ticks(b.period_ns),
            computation: ticks(b.computation_ns),
            constraint: ticks(b.constraint_ns),
            preemptible: 1,
        };
        with_self_thread(|t| unsafe {
            let kr = thread_policy_set(
                t,
                THREAD_TIME_CONSTRAINT_POLICY,
                &mut p as *mut _ as *mut i32,
                THREAD_TIME_CONSTRAINT_POLICY_COUNT,
            );
            if kr != KERN_SUCCESS {
                return Err(format!("thread_policy_set returned {kr}"));
            }
            let mut got = thread_time_constraint_policy_data_t {
                period: 0,
                computation: 0,
                constraint: 0,
                preemptible: 0,
            };
            let mut count = THREAD_TIME_CONSTRAINT_POLICY_COUNT;
            let mut default = 0;
            let kr = thread_policy_get(
                t,
                THREAD_TIME_CONSTRAINT_POLICY,
                &mut got as *mut _ as *mut i32,
                &mut count,
                &mut default,
            );
            if kr != KERN_SUCCESS || default != 0 {
                return Ok("no readback".into());
            }
            let ms = |ticks: u32| crate::time::host_ticks_to_ns(ticks as u64) as f64 / 1e6;
            Ok(format!(
                "period {:.2} ms, computation {:.2} ms, constraint {:.2} ms",
                ms(got.period),
                ms(got.computation),
                ms(got.constraint)
            ))
        })
    }

    /// The policy values in force for the calling thread (`None`: not time-constraint).
    #[cfg(test)]
    pub fn current_time_constraint() -> Option<RtBudget> {
        with_self_thread(|t| unsafe {
            let mut got = thread_time_constraint_policy_data_t {
                period: 0,
                computation: 0,
                constraint: 0,
                preemptible: 0,
            };
            let mut count = THREAD_TIME_CONSTRAINT_POLICY_COUNT;
            let mut default = 0;
            let kr = thread_policy_get(
                t,
                THREAD_TIME_CONSTRAINT_POLICY,
                &mut got as *mut _ as *mut i32,
                &mut count,
                &mut default,
            );
            let ns = |v: u32| crate::time::host_ticks_to_ns(v as u64);
            (kr == KERN_SUCCESS && default == 0).then(|| RtBudget {
                period_ns: ns(got.period),
                computation_ns: ns(got.computation),
                constraint_ns: ns(got.constraint),
            })
        })
    }

    /// Whether the calling thread is scheduled real-time (its basic info's policy is not
    /// timeshare); `None` if that can't be read.
    pub fn is_realtime() -> Option<bool> {
        with_self_thread(|t| unsafe {
            let mut info: libc::thread_basic_info = std::mem::zeroed();
            let mut count = libc::THREAD_BASIC_INFO_COUNT;
            let kr = libc::thread_info(
                t,
                libc::THREAD_BASIC_INFO as u32,
                &mut info as *mut _ as *mut i32,
                &mut count,
            );
            (kr == KERN_SUCCESS).then_some(info.policy != POLICY_TIMESHARE)
        })
    }

    /// A created, not yet joined audio workgroup interval (released on drop).
    pub struct PreparedWorkgroup(OsWorkgroup);
    // An os_workgroup is a thread-safe, reference-counted OS object: creating it on one thread
    // and joining it from another is how workgroups are meant to be used.
    unsafe impl Send for PreparedWorkgroup {}

    impl PreparedWorkgroup {
        /// Creates one on a short-lived helper thread (the first creation in a process is slow).
        pub fn prepare() -> crossbeam_channel::Receiver<Result<Self, String>> {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let spawned = std::thread::Builder::new()
                .name("roommesh-rt-prep".into())
                .spawn(move || {
                    let _ = tx.send(Self::create());
                });
            if let Err(e) = spawned {
                let (tx, rx) = crossbeam_channel::bounded(1);
                let _ = tx.send(Err(format!("cannot start the helper thread: {e}")));
                return rx;
            }
            rx
        }
        fn create() -> Result<Self, String> {
            let wg = unsafe {
                AudioWorkIntervalCreate(
                    c"roommesh-dsp".as_ptr(),
                    OS_CLOCK_MACH_ABSOLUTE_TIME,
                    std::ptr::null_mut(),
                )
            };
            if wg.is_null() {
                return Err("AudioWorkIntervalCreate returned NULL".into());
            }
            Ok(Self(wg))
        }
    }

    impl Drop for PreparedWorkgroup {
        fn drop(&mut self) {
            unsafe { os_release(self.0) };
        }
    }

    /// An audio workgroup interval the calling thread has joined. Leaves and releases it on
    /// drop, which must happen on the same thread.
    pub struct Workgroup {
        wg: OsWorkgroup,
        token: Box<JoinToken>,
        open: bool,
        start_failures: u32,
    }

    impl Workgroup {
        /// Joins the calling thread to `p`.
        pub fn join(p: PreparedWorkgroup) -> Result<Self, String> {
            let wg = p.0;
            std::mem::forget(p); // ownership moves to the Workgroup (or is released below)
            let mut token = Box::new(JoinToken {
                sig: 0,
                opaque: [0; 36],
            });
            let rc = unsafe { os_workgroup_join(wg, &mut *token) };
            if rc != 0 {
                unsafe { os_release(wg) };
                return Err(format!("os_workgroup_join returned {rc}"));
            }
            Ok(Self {
                wg,
                token,
                open: false,
                start_failures: 0,
            })
        }
        /// Starts an interval now with the deadline `deadline_ns` from now.
        pub fn start(&mut self, deadline_ns: u64) -> bool {
            if self.open {
                self.finish();
            }
            let now = unsafe { mach2::mach_time::mach_absolute_time() };
            let rc = unsafe {
                os_workgroup_interval_start(
                    self.wg,
                    now,
                    now + ns_to_host_ticks(deadline_ns),
                    std::ptr::null_mut(),
                )
            };
            self.open = rc == 0;
            if rc != 0 {
                self.start_failures += 1;
                if self.start_failures == 1 {
                    log::warn!("roommesh-dsp: os_workgroup_interval_start returned {rc}");
                }
            }
            self.open
        }
        pub fn finish(&mut self) {
            if std::mem::take(&mut self.open) {
                let _ = unsafe { os_workgroup_interval_finish(self.wg, std::ptr::null_mut()) };
            }
        }
    }

    impl Drop for Workgroup {
        fn drop(&mut self) {
            self.finish();
            unsafe {
                os_workgroup_leave(self.wg, &mut *self.token);
                os_release(self.wg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_millis(2);

    #[test]
    fn the_cost_model_covers_the_measured_p99s() {
        // Measured p99 of wakes with room-mic work, spread 2 per wake (module docs).
        for (mics, p99_us) in [(1, 243), (4, 628), (8, 911)] {
            let model = wake_cost_ns(mics, false);
            assert!(model >= p99_us * 1_000, "{mics} mics: {model} ns");
        }
        assert_eq!(wake_cost_ns(0, false), BASE_NS);
        assert_eq!(wake_cost_ns(0, true), BASE_NS + PER_MIC_NS);
        // Only MICS_PER_WAKE mics are processed in one wake.
        assert_eq!(
            wake_cost_ns(9, false) - wake_cost_ns(8, false),
            FRAME_PER_MIC_NS
        );
        assert_eq!(
            wake_cost_ns(1, false) - wake_cost_ns(0, false),
            FRAME_PER_MIC_NS + PER_MIC_NS
        );
    }

    #[test]
    fn budget_has_headroom_and_fits_the_tick() {
        for mics in 0..=10 {
            for meter in [false, true] {
                let cost = wake_cost_ns(mics, meter);
                let b = budget(TICK, cost);
                assert_eq!(b.period_ns, 2_000_000);
                assert_eq!(b.constraint_ns, 2_000_000);
                assert!(b.computation_ns >= b.constraint_ns / 2, "{b:?}");
                assert!(b.computation_ns <= b.constraint_ns, "{b:?}");
                if cost * 3 / 2 <= b.constraint_ns {
                    assert!(b.computation_ns >= cost * 3 / 2, "{mics} {meter}: {b:?}");
                }
            }
        }
        // The kernel's floor: half the constraint (a member, or up to 3 mics).
        assert_eq!(
            budget(TICK, wake_cost_ns(0, false)).computation_ns,
            1_000_000
        );
        assert_eq!(
            budget(TICK, wake_cost_ns(3, false)).computation_ns,
            1_012_500
        );
        assert_eq!(
            budget(TICK, wake_cost_ns(8, false)).computation_ns,
            1_425_000
        );
        assert_eq!(
            budget(TICK, wake_cost_ns(8, true)).computation_ns,
            1_770_000
        );
    }

    #[test]
    fn budget_never_exceeds_the_constraint() {
        let b = budget(TICK, wake_cost_ns(20, true));
        assert_eq!(b.computation_ns, b.constraint_ns);
        let short = budget(Duration::from_micros(300), 1_000_000);
        assert_eq!(short.computation_ns, 300_000);
        assert_eq!(
            budget(Duration::from_millis(10), 0).computation_ns,
            5_000_000
        );
    }

    #[test]
    fn status_labels() {
        assert_eq!(RtStatus::default(), RtStatus::NormalPriority);
        assert_eq!(
            RtStatus::WorkgroupAndTimeConstraint.label(),
            "workgroup + time-constraint"
        );
        assert_eq!(RtStatus::TimeConstraintOnly.label(), "time-constraint only");
        assert_eq!(RtStatus::NormalPriority.label(), "normal priority");
    }

    #[test]
    fn a_normal_thread_is_inert() {
        let mut t = RealtimeThread::normal();
        assert_eq!(t.status(), RtStatus::NormalPriority);
        assert!(!t.start_interval());
        t.finish_interval();
        assert!(!t.poll_demotion());
        assert!(!t.poll_workgroup());
        assert_eq!(t.demotions(), 0);
        t.set_budget(budget(TICK, 0));
        assert_eq!(t.budget(), None);
    }

    /// On this Mac (and headless CI runners: neither call needs a window server or entitlement)
    /// a thread gets the time-constraint policy and joins an audio workgroup.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_thread_gets_the_time_constraint_policy_and_a_workgroup() {
        std::thread::spawn(|| {
            assert_eq!(mac::is_realtime(), Some(false), "starts as timeshare");
            let b = budget(TICK, wake_cost_ns(2, false));
            let mut t = RealtimeThread::promote_current(b);
            assert_eq!(
                t.status(),
                RtStatus::TimeConstraintOnly,
                "workgroup still coming"
            );
            assert!(!t.start_interval(), "no interval without a workgroup");
            let until = std::time::Instant::now() + Duration::from_secs(5);
            while t.poll_workgroup() {
                assert!(std::time::Instant::now() < until, "workgroup never arrived");
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(t.status(), RtStatus::WorkgroupAndTimeConstraint);
            assert!(!t.poll_workgroup(), "joined once");
            assert_eq!(t.budget(), Some(b));
            let got = mac::current_time_constraint().expect("time-constraint policy in force");
            let close = |a: u64, b: u64| a.abs_diff(b) <= 1_000; // tick rounding
            assert!(close(got.period_ns, b.period_ns), "{got:?}");
            assert!(close(got.constraint_ns, b.constraint_ns), "{got:?}");
            assert!(
                close(got.computation_ns, b.computation_ns),
                "granted as asked: {got:?}"
            );
            assert_eq!(mac::is_realtime(), Some(true));
            assert!(t.poll_demotion());
            assert_eq!(t.demotions(), 0);
            for _ in 0..50 {
                assert!(t.start_interval(), "interval start");
                std::hint::black_box((0..10_000u64).sum::<u64>());
                t.finish_interval();
                std::thread::sleep(Duration::from_millis(1));
            }
            // A new budget is applied.
            let b4 = budget(TICK, wake_cost_ns(8, true));
            t.set_budget(b4);
            assert_eq!(t.budget(), Some(b4));
            let got = mac::current_time_constraint().unwrap();
            assert!(close(got.computation_ns, b4.computation_ns), "{got:?}");
            drop(t); // leaves the workgroup on this thread
        })
        .join()
        .expect("real-time thread checks");
    }
}
