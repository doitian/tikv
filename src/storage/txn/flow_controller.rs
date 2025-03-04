// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use std::cmp::PartialOrd;
use std::collections::VecDeque;
use std::f64::INFINITY;
use std::ops::{Add, AddAssign, Sub, SubAssign};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{Builder, JoinHandle};
use std::time::Duration;
use std::u64;

use collections::HashMap;
use engine_rocks::FlowInfo;
use engine_traits::KvEngine;
use num_traits::cast::{AsPrimitive, FromPrimitive};
use rand::Rng;
use tikv_util::time::{duration_to_sec, Consume, Instant, Limiter};

use crate::storage::config::FlowControlConfig;
use crate::storage::metrics::*;

const SPARE_TICK_DURATION: Duration = Duration::from_millis(1000);
const SPARE_TICKS_THRESHOLD: u64 = 10;
const RATIO_SCALE_FACTOR: f64 = 10000000.0;
const LIMIT_UP_PERCENT: f64 = 0.04; // 4%
const LIMIT_DOWN_PERCENT: f64 = 0.02; // 2%
const MIN_THROTTLE_SPEED: f64 = 16.0 * 1024.0; // 16KB
const MAX_THROTTLE_SPEED: f64 = 200.0 * 1024.0 * 1024.0; // 200MB

const EMA_FACTOR: f64 = 0.6; // EMA stands for Exponential Moving Average
const PID_KP_FACTOR: f64 = 0.15;
const PID_KD_FACTOR: f64 = 5.0;

#[derive(Eq, PartialEq, Debug)]
enum Trend {
    Increasing,
    Decreasing,
    NoTrend,
}

/// Flow controller is used to throttle the write rate at scheduler level, aiming
/// to substitute the write stall mechanism of RocksDB. It features in two points:
///   * throttle at scheduler, so raftstore and apply won't be blocked anymore
///   * better control on the throttle rate to avoid QPS drop under heavy write
///
/// When write stall happens, the max speed of write rate max_delayed_write_rate
/// is limited to 16MB/s by default which doesn't take real disk ability into
/// account. It may underestimate the disk's throughout that 16MB/s is too small
/// at once, causing a very large jitter on the write duration.
/// Also, it decreases the delayed write rate further if the factors still exceed
/// the threshold. So under heavy write load, the write rate may be throttled to
/// a very low rate from time to time, causing QPS drop eventually.
///
/// The main idea of this flow controller is to throttle at a steady write rate
/// so that the number of L0 keeps around the threshold. When it falls below the
/// threshold, the throttle state wouldn't exit right away. Instead, it may keep
/// or increase the throttle speed depending on some statistics.
///
/// How can we decide the throttle speed?
/// It uses 95th write rate of the last few seconds as the initial throttle speed.
/// Then as we can imagine, the consumption ability of L0 wouldn't change
/// dramatically corresponding to the ability of hardware. So we can record the
/// flush flow(L0 production flow) when reaching the threshold as target flow, and
/// increase or decrease the throttle speed based on whether current flush flow is
/// smaller or larger than target flow.

/// For compaction pending bytes, we use discardable ratio to do flow control
/// which is separated mechanism from throttle speed. Compaction pending bytes is
/// a approximate value, usually, changes up and down dramatically, so it's unwise
/// to map compaction pending bytes to a specified throttle speed. Instead,
/// mapping it from soft limit to hard limit as 0% to 100% discardable ratio. With
/// this, there must be a point that foreground write rate is equal to the
/// background compaction pending bytes consuming rate so that compaction pending
/// bytes is kept around a steady level.
///
/// Here is a brief flow showing where the mechanism works:
/// grpc -> check should drop(discardable ratio) -> limiter -> async write to raftstore
pub struct FlowController {
    discard_ratio: Arc<AtomicU32>,
    limiter: Arc<Limiter>,
    enabled: Arc<AtomicBool>,
    tx: SyncSender<Msg>,
    handle: Option<std::thread::JoinHandle<()>>,
}

enum Msg {
    Close,
    Enable,
    Disable,
}

impl Drop for FlowController {
    fn drop(&mut self) {
        let h = self.handle.take();
        if h.is_none() {
            return;
        }

        if let Err(e) = self.tx.send(Msg::Close) {
            error!("send quit message for time monitor worker failed"; "err" => ?e);
            return;
        }

        if let Err(e) = h.unwrap().join() {
            error!("join time monitor worker failed"; "err" => ?e);
            return;
        }
    }
}

impl FlowController {
    // only for test
    pub fn empty() -> Self {
        let (tx, _rx) = mpsc::sync_channel(0);

        Self {
            discard_ratio: Arc::new(AtomicU32::new(0)),
            limiter: Arc::new(Limiter::new(INFINITY)),
            enabled: Arc::new(AtomicBool::new(false)),
            tx,
            handle: None,
        }
    }

    pub fn new<E: KvEngine>(
        config: &FlowControlConfig,
        engine: E,
        flow_info_receiver: Receiver<FlowInfo>,
    ) -> Self {
        let limiter = Arc::new(Limiter::new(INFINITY));
        let discard_ratio = Arc::new(AtomicU32::new(0));
        let checker = FlowChecker::new(config, engine, discard_ratio.clone(), limiter.clone());
        let (tx, rx) = mpsc::sync_channel(5);

        tx.send(if config.enable {
            Msg::Enable
        } else {
            Msg::Disable
        })
        .unwrap();

        Self {
            discard_ratio,
            limiter,
            enabled: Arc::new(AtomicBool::new(config.enable)),
            tx,
            handle: Some(checker.start(rx, flow_info_receiver)),
        }
    }

    pub fn should_drop(&self) -> bool {
        let ratio = self.discard_ratio.load(Ordering::Relaxed);
        let mut rng = rand::thread_rng();
        rng.gen_ratio(ratio, RATIO_SCALE_FACTOR as u32)
    }

    pub fn consume(&self, bytes: usize) -> Consume {
        self.limiter.consume(bytes)
    }

    pub fn enable(&self, enable: bool) {
        self.enabled.store(enable, Ordering::Relaxed);
        if enable {
            self.tx.send(Msg::Enable).unwrap();
        } else {
            self.tx.send(Msg::Disable).unwrap();
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn is_unlimited(&self) -> bool {
        self.limiter.speed_limit() == INFINITY
    }
}

const SMOOTHER_STALE_RECORD_THRESHOLD: f64 = 300.0; // 5min

// Smoother is a sliding window used to provide steadier flow statistics.
struct Smoother<T, const CAP: usize>
where
    T: Default
        + Add<Output = T>
        + Sub<Output = T>
        + AddAssign
        + SubAssign
        + PartialOrd
        + AsPrimitive<f64>
        + FromPrimitive,
{
    records: VecDeque<(T, Instant)>,
    total: T,
}

impl<T, const CAP: usize> Default for Smoother<T, CAP>
where
    T: Default
        + Add<Output = T>
        + Sub<Output = T>
        + AddAssign
        + SubAssign
        + PartialOrd
        + AsPrimitive<f64>
        + FromPrimitive,
{
    fn default() -> Self {
        Self {
            records: VecDeque::with_capacity(CAP),
            total: Default::default(),
        }
    }
}

impl<T, const CAP: usize> Smoother<T, CAP>
where
    T: Default
        + Add<Output = T>
        + Sub<Output = T>
        + AddAssign
        + SubAssign
        + PartialOrd
        + AsPrimitive<f64>
        + FromPrimitive,
{
    pub fn observe(&mut self, record: T) {
        if self.records.len() == CAP {
            let v = self.records.pop_front().unwrap().0;
            self.total -= v;
        }

        self.total += record;

        self.records.push_back((record, Instant::now_coarse()));
        self.remove_stale_records();
    }

    fn remove_stale_records(&mut self) {
        // make sure there are two records left at least
        while self.records.len() > 2 {
            if self.records.front().unwrap().1.saturating_elapsed_secs()
                > SMOOTHER_STALE_RECORD_THRESHOLD
            {
                let v = self.records.pop_front().unwrap().0;
                self.total -= v;
            } else {
                break;
            }
        }
    }

    pub fn get_recent(&self) -> T {
        if self.records.is_empty() {
            return T::default();
        }
        self.records.back().unwrap().0
    }

    pub fn get_avg(&self) -> f64 {
        if self.records.is_empty() {
            return 0.0;
        }
        self.total.as_() / self.records.len() as f64
    }

    pub fn get_max(&self) -> T {
        if self.records.is_empty() {
            return T::default();
        }
        self.records
            .iter()
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
            .unwrap()
            .0
    }

    pub fn get_percentile_90(&mut self) -> T {
        if self.records.is_empty() {
            return FromPrimitive::from_u64(0).unwrap();
        }
        let mut v: Vec<_> = self.records.iter().collect();
        v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        v[((self.records.len() - 1) as f64 * 0.90) as usize].0
    }

    pub fn slope(&self) -> f64 {
        if self.records.len() <= 1 {
            return 0.0;
        }

        // calculate the average of left and right parts
        let half = self.records.len() / 2;
        let mut left = T::default();
        let mut right = T::default();
        for (i, r) in self.records.iter().enumerate() {
            if i < half {
                left += r.0;
            } else if self.records.len() - i - 1 < half {
                right += r.0;
            }
        }
        // use the two averages with the time span of oldest and latest records
        // to get a slope
        let elapsed = duration_to_sec(
            self.records
                .back()
                .unwrap()
                .1
                .duration_since(self.records.front().unwrap().1),
        );
        (right - left).as_() / half as f64 / (elapsed / 2.0)
    }

    pub fn trend(&self) -> Trend {
        if self.records.len() <= 1 {
            return Trend::NoTrend;
        }

        // calculate the average of left and right parts
        let half = self.records.len() / 2;
        let mut left = T::default();
        let mut right = T::default();
        for (i, r) in self.records.iter().enumerate() {
            if i < half {
                left += r.0;
            } else if self.records.len() - i - 1 < half {
                right += r.0;
            }
        }

        // decide if there is a trend by the two averages
        // adding 2 here is to give a tolerance
        if right > left + FromPrimitive::from_u64(2).unwrap() {
            Trend::Increasing
        } else if left > right + FromPrimitive::from_u64(2).unwrap() {
            Trend::Decreasing
        } else {
            Trend::NoTrend
        }
    }
}

// CFFlowChecker records some statistics and states related to one CF.
// These statistics fall into five categories:
//   * memtable
//   * L0 files
//   * L0 production flow (flush flow)
//   * L0 consumption flow (compaction read flow of L0)
//   * pending compaction bytes
// And all of them are collected from the hook of RocksDB's event listener.
struct CFFlowChecker {
    // Memtable related
    last_num_memtables: Smoother<u64, 20>,
    memtable_debt: f64,
    init_speed: bool,

    // L0 files related
    // last number of l0 files right after flush or L0 compaction
    last_num_l0_files: u64,
    // last number of l0 files right after flush
    last_num_l0_files_from_flush: u64,
    // a few records of number of l0 files right after L0 compaction
    // As we know, after flush the number of L0 files must increase by 1,
    // whereas, after L0 compaction the number of L0 files must decrease a lot
    // considering L0 compactions nearly includes all L0 files in a round.
    // So to evaluate the accumulation of L0 files, here only records the number
    // of L0 files right after L0 compactions.
    long_term_num_l0_files: Smoother<u64, 20>,

    // L0 production flow related
    last_flush_bytes_time: Instant,
    last_flush_bytes: u64,
    short_term_l0_production_flow: Smoother<u64, 10>,
    long_term_l0_production_flow: Smoother<u64, 60>,

    // L0 consumption flow related
    last_l0_bytes: u64,
    last_l0_bytes_time: Instant,
    short_term_l0_consumption_flow: Smoother<u64, 3>,

    // Pending compaction bytes related
    long_term_pending_bytes: Smoother<f64, 60>,

    // On start related markers. Because after restart, the memtable, l0 files
    // and compaction pending bytes may be high on start. If throttle on start
    // at once, it may get a low throttle speed as initialization cause it may
    // has no write flow after restart. So use the markers to make sure only
    // throttled after the the memtable, l0 files and compaction pending bytes
    // go beyond the threshold again.
    on_start_memtable: bool,
    on_start_l0_files: bool,
    on_start_pending_bytes: bool,
}

impl Default for CFFlowChecker {
    fn default() -> Self {
        Self {
            last_num_memtables: Smoother::default(),
            long_term_pending_bytes: Smoother::default(),
            long_term_num_l0_files: Smoother::default(),
            last_num_l0_files: 0,
            last_num_l0_files_from_flush: 0,
            last_flush_bytes: 0,
            last_flush_bytes_time: Instant::now_coarse(),
            short_term_l0_production_flow: Smoother::default(),
            long_term_l0_production_flow: Smoother::default(),
            last_l0_bytes: 0,
            last_l0_bytes_time: Instant::now_coarse(),
            short_term_l0_consumption_flow: Smoother::default(),
            memtable_debt: 0.0,
            init_speed: false,
            on_start_memtable: true,
            on_start_l0_files: true,
            on_start_pending_bytes: true,
        }
    }
}

struct FlowChecker<E: KvEngine> {
    soft_pending_compaction_bytes_limit: u64,
    hard_pending_compaction_bytes_limit: u64,
    memtables_threshold: u64,
    l0_files_threshold: u64,

    // CFFlowChecker for each CF.
    cf_checkers: HashMap<String, CFFlowChecker>,
    // Record which CF is taking control of throttling, the throttle speed is
    // decided based on the statistics of the throttle CF. If the multiple CFs
    // exceed the threshold, choose the larger one.
    throttle_cf: Option<String>,
    // The target flow of L0, the algorithm's goal is to make flush flow close
    // to L0 target flow.
    l0_target_flow: f64,
    // The number of L0 files when the last update of L0 target flow.
    num_l0_for_last_update_target_flow: Option<u64>,
    // Discard ratio is decided by pending compaction bytes, it's the ratio to
    // drop write requests(return ServerIsBusy to TiDB) randomly.
    discard_ratio: Arc<AtomicU32>,

    engine: E,
    limiter: Arc<Limiter>,
    // Records the foreground write flow at scheduler level of last few seconds.
    write_flow_recorder: Smoother<u64, 30>,
    last_record_time: Instant,
}

impl<E: KvEngine> FlowChecker<E> {
    pub fn new(
        config: &FlowControlConfig,
        engine: E,
        discard_ratio: Arc<AtomicU32>,
        limiter: Arc<Limiter>,
    ) -> Self {
        let mut cf_checkers = map![];

        for cf in engine.cf_names() {
            cf_checkers.insert(cf.to_owned(), CFFlowChecker::default());
        }

        Self {
            soft_pending_compaction_bytes_limit: config.soft_pending_compaction_bytes_limit.0,
            hard_pending_compaction_bytes_limit: config.hard_pending_compaction_bytes_limit.0,
            memtables_threshold: config.memtables_threshold,
            l0_files_threshold: config.l0_files_threshold,
            engine,
            discard_ratio,
            limiter,
            write_flow_recorder: Smoother::default(),
            cf_checkers,
            throttle_cf: None,
            l0_target_flow: 0.0,
            num_l0_for_last_update_target_flow: None,
            last_record_time: Instant::now_coarse(),
        }
    }

    fn start(self, rx: Receiver<Msg>, flow_info_receiver: Receiver<FlowInfo>) -> JoinHandle<()> {
        Builder::new()
            .name(thd_name!("flow-checker"))
            .spawn(move || {
                tikv_alloc::add_thread_memory_accessor();
                let mut checker = self;
                let mut deadline = std::time::Instant::now();
                let mut spare_ticks = 0;
                let mut enabled = true;
                loop {
                    match rx.try_recv() {
                        Ok(Msg::Close) => break,
                        Ok(Msg::Disable) => {
                            enabled = false;
                            checker.reset_statistics();
                        }
                        Ok(Msg::Enable) => {
                            enabled = true;
                        }
                        Err(_) => {}
                    }

                    if !enabled {
                        // do nothing, just consume the flow info channel
                        let _ = flow_info_receiver.recv();
                        continue;
                    }

                    match flow_info_receiver.recv_deadline(deadline) {
                        Ok(FlowInfo::L0(cf, l0_bytes)) => {
                            if let Some(throttle_cf) = checker.throttle_cf.as_ref() {
                                if throttle_cf == &cf {
                                    spare_ticks = 0;
                                }
                            }
                            checker.on_l0_decr(cf, l0_bytes)
                        }
                        Ok(FlowInfo::L0Intra(cf, diff_bytes)) => {
                            if let Some(throttle_cf) = checker.throttle_cf.as_ref() {
                                if throttle_cf == &cf {
                                    spare_ticks = 0;
                                }
                            }
                            if diff_bytes > 0 {
                                // Intra L0 merges some deletion records, so regard it as a L0 compaction.
                                checker.on_l0_decr(cf, diff_bytes)
                            }
                        }
                        Ok(FlowInfo::Flush(cf, flush_bytes)) => {
                            if let Some(throttle_cf) = checker.throttle_cf.as_ref() {
                                if throttle_cf == &cf {
                                    spare_ticks = 0;
                                }
                            }
                            checker.on_memtable_decrs(&cf);
                            checker.on_l0_incr(cf, flush_bytes)
                        }
                        Ok(FlowInfo::Compaction(cf)) => {
                            if let Some(throttle_cf) = checker.throttle_cf.as_ref() {
                                if throttle_cf == &cf {
                                    spare_ticks = 0;
                                }
                            }
                            checker.on_pending_compaction_bytes_change(cf);
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            spare_ticks += 1;
                            if spare_ticks == SPARE_TICKS_THRESHOLD {
                                // there is no flush/compaction happens, we should speed up if throttled
                                checker.tick_l0();
                                spare_ticks = 0;
                            }
                            checker.update_statistics();
                            deadline = std::time::Instant::now() + SPARE_TICK_DURATION;
                        }
                        Err(e) => {
                            error!("failed to receive compaction info {:?}", e);
                        }
                    }
                }
                tikv_alloc::remove_thread_memory_accessor();
            })
            .unwrap()
    }

    fn reset_statistics(&mut self) {
        SCHED_L0_TARGET_FLOW_GAUGE.set(0);
        for cf in self.cf_checkers.keys() {
            SCHED_THROTTLE_CF_GAUGE.with_label_values(&[cf]).set(0);
            SCHED_PENDING_COMPACTION_BYTES_GAUGE
                .with_label_values(&[cf])
                .set(0);
            SCHED_MEMTABLE_GAUGE.with_label_values(&[cf]).set(0);
            SCHED_L0_GAUGE.with_label_values(&[cf]).set(0);
            SCHED_L0_AVG_GAUGE.with_label_values(&[cf]).set(0);
            SCHED_L0_FLOW_GAUGE.with_label_values(&[cf]).set(0);
            SCHED_FLUSH_L0_GAUGE.with_label_values(&[cf]).set(0);
            SCHED_FLUSH_FLOW_GAUGE.with_label_values(&[cf]).set(0);
            SCHED_LONG_TERM_FLUSH_FLOW_GAUGE
                .with_label_values(&[cf])
                .set(0);
            SCHED_UP_FLOW_GAUGE.set(0);
            SCHED_DOWN_FLOW_GAUGE.set(0);
        }
        SCHED_WRITE_FLOW_GAUGE.set(0);
        SCHED_THROTTLE_FLOW_GAUGE.set(0);
        self.limiter.set_speed_limit(INFINITY);
        SCHED_DISCARD_RATIO_GAUGE.set(0);
        self.discard_ratio.store(0, Ordering::Relaxed);
    }

    fn update_statistics(&mut self) {
        if self.num_l0_for_last_update_target_flow.is_some() {
            SCHED_L0_TARGET_FLOW_GAUGE.set(self.l0_target_flow as i64);
        } else {
            SCHED_L0_TARGET_FLOW_GAUGE.set(0);
        }

        if let Some(throttle_cf) = self.throttle_cf.as_ref() {
            SCHED_THROTTLE_CF_GAUGE
                .with_label_values(&[throttle_cf])
                .set(1);
            for cf in self.cf_checkers.keys() {
                if cf != throttle_cf {
                    SCHED_THROTTLE_CF_GAUGE.with_label_values(&[cf]).set(0);
                }
            }
        } else {
            for cf in self.cf_checkers.keys() {
                SCHED_THROTTLE_CF_GAUGE.with_label_values(&[cf]).set(0);
            }
        }

        // calculate foreground write flow
        let rate = self.limiter.total_bytes_consumed() as f64
            / self.last_record_time.saturating_elapsed_secs();
        // don't record those write rate of 0.
        // For closed loop system, if all the requests are delayed(assume > 1s),
        // then in the next second, the write rate would be 0. But it doesn't
        // reflect the real write rate, so just ignore it.
        if self.limiter.total_bytes_consumed() != 0 {
            self.write_flow_recorder.observe(rate as u64);
        }
        SCHED_WRITE_FLOW_GAUGE.set(rate as i64);
        self.last_record_time = Instant::now_coarse();

        self.limiter.reset_statistics();
    }

    fn on_pending_compaction_bytes_change(&mut self, cf: String) {
        let hard = (self.hard_pending_compaction_bytes_limit as f64).log2();
        let soft = (self.soft_pending_compaction_bytes_limit as f64).log2();

        // Because pending compaction bytes changes dramatically, take the
        // logarithm of pending compaction bytes to make the values fall into
        // a relative small range
        let num = (self
            .engine
            .get_cf_pending_compaction_bytes(&cf)
            .unwrap_or(None)
            .unwrap_or(0) as f64)
            .log2();
        let checker = self.cf_checkers.get_mut(&cf).unwrap();
        checker.long_term_pending_bytes.observe(num);
        SCHED_PENDING_COMPACTION_BYTES_GAUGE
            .with_label_values(&[&cf])
            .set(checker.long_term_pending_bytes.get_avg() as i64);

        // do special check on start, see the comment of the variable definition for detail.
        if checker.on_start_pending_bytes {
            if num < soft || checker.long_term_pending_bytes.trend() == Trend::Increasing {
                // the write is accumulating, still need to throttle
                checker.on_start_pending_bytes = false;
            } else {
                // still on start, should not throttle now
                return;
            }
        }

        let pending_compaction_bytes = checker.long_term_pending_bytes.get_avg();

        for checker in self.cf_checkers.values() {
            if num < checker.long_term_pending_bytes.get_recent() {
                return;
            }
        }

        let ratio = if pending_compaction_bytes < soft {
            0
        } else {
            let new_ratio = (pending_compaction_bytes - soft) / (hard - soft);
            let old_ratio = self.discard_ratio.load(Ordering::Relaxed);

            // Because pending compaction bytes changes up and down, so using
            // EMA(Exponential Moving Average) to smooth it.
            (if old_ratio != 0 {
                EMA_FACTOR * (old_ratio as f64 / RATIO_SCALE_FACTOR)
                    + (1.0 - EMA_FACTOR) * new_ratio
            } else if new_ratio > 0.01 {
                0.01
            } else {
                new_ratio
            } * RATIO_SCALE_FACTOR) as u32
        };
        SCHED_DISCARD_RATIO_GAUGE.set(ratio as i64);
        self.discard_ratio.store(ratio, Ordering::Relaxed);
    }

    fn on_memtable_decrs(&mut self, cf: &str) {
        let num_memtables = self
            .engine
            .get_cf_num_immutable_mem_table(cf)
            .unwrap_or(None)
            .unwrap_or(0);
        let checker = self.cf_checkers.get_mut(cf).unwrap();
        SCHED_MEMTABLE_GAUGE
            .with_label_values(&[cf])
            .set(num_memtables as i64);
        let prev = checker.last_num_memtables.get_recent();
        checker.last_num_memtables.observe(num_memtables);

        // do special check on start, see the comment of the variable definition for detail.
        if checker.on_start_memtable {
            if num_memtables < self.memtables_threshold
                || checker.last_num_memtables.trend() == Trend::Increasing
            {
                // the write is accumulating, still need to throttle
                checker.on_start_memtable = false;
            } else {
                // still on start, should not throttle now
                return;
            }
        }

        for c in self.cf_checkers.values() {
            if num_memtables < c.last_num_memtables.get_recent() {
                return;
            }
        }

        let checker = self.cf_checkers.get_mut(cf).unwrap();
        let is_throttled = self.limiter.speed_limit() != INFINITY;
        let should_throttle =
            checker.last_num_memtables.get_avg() > self.memtables_threshold as f64;
        let throttle = if !is_throttled {
            if should_throttle {
                SCHED_THROTTLE_ACTION_COUNTER
                    .with_label_values(&[cf, "memtable_init"])
                    .inc();
                checker.init_speed = true;
                let x = self.write_flow_recorder.get_percentile_90();
                if x == 0 { INFINITY } else { x as f64 }
            } else {
                INFINITY
            }
        } else if !should_throttle
            || checker.last_num_memtables.get_recent() < self.memtables_threshold
        {
            // should not throttle memtable
            checker.memtable_debt = 0.0;
            if checker.init_speed {
                INFINITY
            } else {
                self.limiter.speed_limit() + checker.memtable_debt * 1024.0 * 1024.0
            }
        } else {
            // should throttle
            let diff = match checker.last_num_memtables.get_recent().cmp(&prev) {
                std::cmp::Ordering::Greater => {
                    checker.memtable_debt += 1.0;
                    -1.0
                }
                std::cmp::Ordering::Less => {
                    checker.memtable_debt -= 1.0;
                    1.0
                }
                std::cmp::Ordering::Equal => {
                    // keep, do nothing
                    0.0
                }
            };
            self.limiter.speed_limit() + diff * 1024.0 * 1024.0
        };

        self.update_speed_limit(throttle);
    }

    fn tick_l0(&mut self) {
        if self.limiter.speed_limit() != INFINITY {
            let cf = self.throttle_cf.as_ref().unwrap();
            let checker = self.cf_checkers.get_mut(cf).unwrap();
            if checker.last_num_l0_files <= self.l0_files_threshold {
                SCHED_THROTTLE_ACTION_COUNTER
                    .with_label_values(&[cf, "tick_spare"])
                    .inc();

                let throttle = if checker.long_term_num_l0_files.get_avg()
                    >= self.l0_files_threshold as f64 * 0.5
                    || checker.long_term_num_l0_files.get_recent() as f64
                        >= self.l0_files_threshold as f64 * 0.5
                    || checker.last_num_l0_files_from_flush >= self.l0_files_threshold
                {
                    SCHED_THROTTLE_ACTION_COUNTER
                        .with_label_values(&[cf, "keep_spare"])
                        .inc();
                    self.limiter.speed_limit()
                } else {
                    SCHED_THROTTLE_ACTION_COUNTER
                        .with_label_values(&[cf, "up_spare"])
                        .inc();
                    self.limiter.speed_limit() * (1.0 + 5.0 * LIMIT_UP_PERCENT)
                };

                self.update_speed_limit(throttle)
            }
        }
    }

    // Check the number of l0 files to decide whether need to adjust target flow
    fn on_l0_decr(&mut self, cf: String, l0_bytes: u64) {
        let num_l0_files = self
            .engine
            .get_cf_num_files_at_level(&cf, 0)
            .unwrap_or(None)
            .unwrap_or(0);
        let checker = self.cf_checkers.get_mut(&cf).unwrap();
        checker.last_l0_bytes += l0_bytes;
        checker.long_term_num_l0_files.observe(num_l0_files);
        checker.last_num_l0_files = num_l0_files;
        SCHED_L0_GAUGE
            .with_label_values(&[&cf])
            .set(num_l0_files as i64);
        SCHED_L0_AVG_GAUGE
            .with_label_values(&[&cf])
            .set(checker.long_term_num_l0_files.get_avg() as i64);
        SCHED_THROTTLE_ACTION_COUNTER
            .with_label_values(&[&cf, "tick"])
            .inc();

        // do special check on start, see the comment of the variable definition for detail.
        if checker.on_start_l0_files {
            if num_l0_files < self.l0_files_threshold
                || checker.long_term_num_l0_files.trend() == Trend::Increasing
            {
                // the write is accumulating, still need to throttle
                checker.on_start_l0_files = false;
            } else {
                // still on start, should not throttle now
                return;
            }
        }

        if let Some(throttle_cf) = self.throttle_cf.as_ref() {
            if &cf != throttle_cf {
                // to avoid throttle cf changes back and forth, only change it
                // when the other is much higher.
                if num_l0_files
                    > self.cf_checkers[throttle_cf]
                        .long_term_num_l0_files
                        .get_max()
                        + 4
                {
                    SCHED_THROTTLE_ACTION_COUNTER
                        .with_label_values(&[&cf, "change_throttle_cf"])
                        .inc();
                    self.throttle_cf = Some(cf.clone());
                    self.num_l0_for_last_update_target_flow = Some(num_l0_files);
                    self.l0_target_flow = self.cf_checkers[&cf]
                        .short_term_l0_production_flow
                        .get_avg();
                } else {
                    return;
                }
            }
        }

        let checker = self.cf_checkers.get_mut(&cf).unwrap();

        let is_throttled = self.limiter.speed_limit() != INFINITY;
        let should_throttle = checker.last_num_l0_files > self.l0_files_threshold;

        let throttle = if !is_throttled && should_throttle {
            SCHED_THROTTLE_ACTION_COUNTER
                .with_label_values(&[&cf, "init"])
                .inc();
            self.throttle_cf = Some(cf.clone());
            self.num_l0_for_last_update_target_flow = Some(checker.last_num_l0_files);
            self.l0_target_flow = checker.short_term_l0_production_flow.get_avg();
            let x = self.write_flow_recorder.get_percentile_90();
            if x == 0 { INFINITY } else { x as f64 }
        } else if is_throttled && should_throttle {
            // refresh down flow if last num l0 files
            if let Some(num_l0_for_last_update_target_flow) =
                self.num_l0_for_last_update_target_flow
            {
                if checker.last_num_l0_files > num_l0_for_last_update_target_flow + 3
                    && self.l0_target_flow > checker.short_term_l0_consumption_flow.get_avg()
                {
                    self.l0_target_flow = checker.short_term_l0_consumption_flow.get_avg();
                    self.num_l0_for_last_update_target_flow = Some(checker.last_num_l0_files);
                    SCHED_THROTTLE_ACTION_COUNTER
                        .with_label_values(&[&cf, "refresh_down_flow"])
                        .inc();
                }
            } else {
                self.num_l0_for_last_update_target_flow = Some(checker.last_num_l0_files);
                self.l0_target_flow = checker.short_term_l0_production_flow.get_avg();
            }
            self.limiter.speed_limit()
        } else if is_throttled && !should_throttle {
            if checker.long_term_num_l0_files.get_avg() >= self.l0_files_threshold as f64 * 0.5
                || checker.last_num_l0_files_from_flush >= self.l0_files_threshold
            {
                SCHED_THROTTLE_ACTION_COUNTER
                    .with_label_values(&[&cf, "keep"])
                    .inc();
                self.limiter.speed_limit()
            } else {
                if self.num_l0_for_last_update_target_flow.is_some()
                    && checker.short_term_l0_consumption_flow.get_avg() > self.l0_target_flow
                {
                    let new = 0.5 * checker.short_term_l0_consumption_flow.get_avg()
                        + 0.5 * self.l0_target_flow;
                    if new > self.l0_target_flow {
                        self.l0_target_flow = new;
                        SCHED_THROTTLE_ACTION_COUNTER
                            .with_label_values(&[&cf, "refresh_up_flow"])
                            .inc();
                    }
                }
                SCHED_THROTTLE_ACTION_COUNTER
                    .with_label_values(&[&cf, "up"])
                    .inc();
                self.limiter.speed_limit() * (1.0 + LIMIT_UP_PERCENT)
            }
        } else {
            INFINITY
        };

        self.update_speed_limit(throttle)
    }

    fn update_speed_limit(&mut self, mut throttle: f64) {
        if throttle < MIN_THROTTLE_SPEED {
            throttle = MIN_THROTTLE_SPEED;
        }
        if throttle > MAX_THROTTLE_SPEED {
            self.throttle_cf = None;
            self.num_l0_for_last_update_target_flow = None;
            throttle = INFINITY;
        }
        SCHED_THROTTLE_FLOW_GAUGE.set(if throttle == INFINITY {
            0
        } else {
            throttle as i64
        });
        self.limiter.set_speed_limit(throttle)
    }

    // Check flush flow to compare with target flow to decide whether need to adjust throttle speed
    fn on_l0_incr(&mut self, cf: String, flush_bytes: u64) {
        let num_l0_files = self
            .engine
            .get_cf_num_files_at_level(&cf, 0)
            .unwrap_or(None)
            .unwrap_or(0);

        let checker = self.cf_checkers.get_mut(&cf).unwrap();
        checker.last_flush_bytes += flush_bytes;
        // no need to add it to long_term_num_l0_files which only records result right after L0 compaction.
        checker.last_num_l0_files = num_l0_files;
        checker.last_num_l0_files_from_flush = num_l0_files;
        SCHED_FLUSH_L0_GAUGE
            .with_label_values(&[&cf])
            .set(num_l0_files as i64);

        if checker.last_flush_bytes_time.saturating_elapsed_secs() > 5.0 {
            // update flush flow
            let flush_flow = checker.last_flush_bytes as f64
                / checker.last_flush_bytes_time.saturating_elapsed_secs();
            checker
                .short_term_l0_production_flow
                .observe(flush_flow as u64);
            checker
                .long_term_l0_production_flow
                .observe(flush_flow as u64);
            SCHED_FLUSH_FLOW_GAUGE
                .with_label_values(&[&cf])
                .set(checker.short_term_l0_production_flow.get_avg() as i64);
            SCHED_LONG_TERM_FLUSH_FLOW_GAUGE
                .with_label_values(&[&cf])
                .set(checker.long_term_l0_production_flow.get_avg() as i64);

            // update l0 flow
            if checker.last_l0_bytes != 0 {
                let l0_flow = checker.last_l0_bytes as f64
                    / checker.last_l0_bytes_time.saturating_elapsed_secs();
                checker.last_l0_bytes_time = Instant::now_coarse();
                checker
                    .short_term_l0_consumption_flow
                    .observe(l0_flow as u64);
                SCHED_L0_FLOW_GAUGE
                    .with_label_values(&[&cf])
                    .set(checker.short_term_l0_consumption_flow.get_avg() as i64);
            }

            checker.last_flush_bytes_time = Instant::now_coarse();
            checker.last_l0_bytes = 0;
            checker.last_flush_bytes = 0;

            if checker.on_start_l0_files {
                if num_l0_files < self.l0_files_threshold
                    || checker.long_term_num_l0_files.trend() == Trend::Increasing
                {
                    // the write is accumulating, still need to throttle
                    checker.on_start_l0_files = false;
                } else {
                    // still on start, should not throttle now
                    return;
                }
            }

            if let Some(throttle_cf) = self.throttle_cf.as_ref() {
                if &cf != throttle_cf {
                    return;
                }
            }

            // adjust throttle speed based on flush flow and target flow
            if let Some(_num_l0_for_last_update_target_flow) =
                self.num_l0_for_last_update_target_flow
            {
                if self.cf_checkers[&cf].long_term_l0_production_flow.get_avg()
                    > self.l0_target_flow
                    && self.cf_checkers[&cf]
                        .short_term_l0_production_flow
                        .get_recent() as f64
                        > self.l0_target_flow
                {
                    SCHED_THROTTLE_ACTION_COUNTER
                        .with_label_values(&[&cf, "down_flow"])
                        .inc();
                    self.decrease_speed_limit(cf);
                } else if (self.cf_checkers[&cf]
                    .short_term_l0_production_flow
                    .get_avg()
                    < self.l0_target_flow
                    || (self.cf_checkers[&cf]
                        .short_term_l0_production_flow
                        .get_recent() as f64)
                        < self.l0_target_flow)
                    && self.write_flow_recorder.get_recent() as f64
                        > self.limiter.speed_limit() * 0.95
                {
                    SCHED_THROTTLE_ACTION_COUNTER
                        .with_label_values(&[&cf, "up_flow"])
                        .inc();
                    self.increase_speed_limit(cf);
                } else {
                    SCHED_THROTTLE_ACTION_COUNTER
                        .with_label_values(&[&cf, "keep_flow"])
                        .inc();
                }
            } else {
                SCHED_THROTTLE_ACTION_COUNTER
                    .with_label_values(&[&cf, "no_target_flow"])
                    .inc();
            }
        }
    }

    fn increase_speed_limit(&mut self, cf: String) {
        let throttle = if self.limiter.speed_limit() == INFINITY {
            self.throttle_cf = Some(cf);
            let x = self.write_flow_recorder.get_percentile_90();
            if x == 0 { INFINITY } else { x as f64 }
        } else {
            // Use PID algorithm to change the flow so up flow can be increased
            // rapidly when the target flow is quite larger than flush flow.
            let mut u = PID_KP_FACTOR
                * (self.l0_target_flow
                    - self.cf_checkers[&cf]
                        .short_term_l0_production_flow
                        .get_avg()
                    + PID_KD_FACTOR * -self.cf_checkers[&cf].short_term_l0_production_flow.slope());
            if u > self.limiter.speed_limit() {
                u = self.limiter.speed_limit();
            } else if u < 0.0 {
                u = 0.0;
            };
            SCHED_UP_FLOW_GAUGE.set((u * RATIO_SCALE_FACTOR) as i64);

            self.limiter.speed_limit() + u
        };
        self.update_speed_limit(throttle)
    }

    fn decrease_speed_limit(&mut self, cf: String) {
        let throttle = if self.limiter.speed_limit() == INFINITY {
            self.throttle_cf = Some(cf);
            let x = self.write_flow_recorder.get_percentile_90();
            if x == 0 { INFINITY } else { x as f64 }
        } else {
            self.limiter.speed_limit() * (1.0 - LIMIT_DOWN_PERCENT)
        };
        self.update_speed_limit(throttle)
    }
}

#[cfg(test)]
mod tests {
    use super::Smoother;
    use super::Trend;

    #[test]
    fn test_smoother() {
        let mut smoother = Smoother::<u64, 5>::default();
        smoother.observe(1);
        smoother.observe(6);
        smoother.observe(2);
        smoother.observe(3);
        smoother.observe(4);
        smoother.observe(5);
        smoother.observe(0);

        assert_eq!(smoother.get_avg(), 2.8);
        assert_eq!(smoother.get_recent(), 0);
        assert_eq!(smoother.get_max(), 5);
        assert_eq!(smoother.get_percentile_90(), 4);
        // assert!(smoother.slope() - 0.0 < f64::EPSILON);
        assert_eq!(smoother.trend(), Trend::NoTrend);

        let mut smoother = Smoother::<f64, 5>::default();
        smoother.observe(1.0);
        smoother.observe(6.0);
        smoother.observe(2.0);
        smoother.observe(3.0);
        smoother.observe(4.0);
        smoother.observe(5.0);
        smoother.observe(9.0);
        assert_eq!(smoother.get_avg(), 4.6);
        assert_eq!(smoother.get_recent(), 9.0);
        assert_eq!(smoother.get_max(), 9.0);
        assert_eq!(smoother.get_percentile_90(), 5.0);
        assert_eq!(smoother.trend(), Trend::Increasing);
    }
}
