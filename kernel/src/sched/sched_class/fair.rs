// SPDX-License-Identifier: MPL-2.0

use alloc::{collections::BinaryHeap, sync::Arc};
use fixed::types::extra::True;
use core::{
    cmp::{self, Reverse}, sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed}, u64::MAX
};
// use core::sync::atomic::Ordering::Relaxed;
use ostd::{
    cpu::{num_cpus, CpuId},
    task::{
        scheduler::{EnqueueFlags, UpdateFlags},
        Task,
    },
};

use super::{
    time::{base_slice_clocks, min_period_clocks},
    CurrentRuntime, SchedAttr, SchedClassRq,
};
use crate::{
    sched::nice::{Nice, NiceValue},
    thread::{task, AsThread},
};

use super::augment_tree::AugmentTree;
use super::augment_tree::AugmentTreeNode;

const WEIGHT_0: u64 = 1024;
pub const fn nice_to_weight(nice: Nice) -> u64 {
    // return 1;

    // Calculated by the formula below:
    //
    //     weight = 1024 * 1.25^(-nice)
    //
    // We propose that every increment of the nice value results
    // in 12.5% change of the CPU load weight.
    const FACTOR_NUMERATOR: u64 = 5;
    const FACTOR_DENOMINATOR: u64 = 4;

    const NICE_TO_WEIGHT: [u64; 40] = const {
        let mut ret = [0; 40];

        let mut index = 0;
        let mut nice = NiceValue::MIN.get();
        while nice <= NiceValue::MAX.get() {
            ret[index] = match nice {
                0 => WEIGHT_0,
                nice @ 1.. => {
                    let numerator = FACTOR_DENOMINATOR.pow(nice as u32);
                    let denominator = FACTOR_NUMERATOR.pow(nice as u32);
                    WEIGHT_0 * numerator / denominator
                }
                nice => {
                    let numerator = FACTOR_NUMERATOR.pow((-nice) as u32);
                    let denominator = FACTOR_DENOMINATOR.pow((-nice) as u32);
                    WEIGHT_0 * numerator / denominator
                }
            };

            index += 1;
            nice += 1;
        }
        ret
    };

    NICE_TO_WEIGHT[(nice.value().get() + 20) as usize]
}

/// The scheduling entity for the FAIR scheduling class.
///
/// The structure contains a significant indicator: `vruntime`.
///
/// # `vruntime`
///
/// The vruntime (virtual runtime) is calculated by the formula:
///
///     vruntime += runtime_delta * WEIGHT_0 / weight
///
/// and a thread with a lower vruntime gains a greater privilege to be
/// scheduled, making the whole run queue balanced on vruntime (thus FAIR).
///
/// # Scheduling periods
///
/// Scheduling periods is designed to calculate the time slice for each threads.
///
/// The time slice for each threads is calculated by the formula:
///
///     time_slice = period * weight / total_weight
///
/// where `total_weight` is the sum of all weights in the run queue including
/// the current thread and [`period`](FairClassRq::period) is calculated
/// regarding the number of running threads.
///
/// When a thread meets the condition below, it will be preempted to the
/// run queue. See [`FairClassRq::update_current`] for more details.
///
///     period_delta > time_slice
///         || vruntime > rq_min_vruntime + normalized_time_slice
#[derive(Debug)]
pub struct FairAttr {
    // why weight is atomic?
    weight: AtomicU64,
    vruntime: AtomicU64,
    eligible_vruntime: AtomicU64, //同时等于每一次时间片开始的时间vruntime_start
    vruntime_deadline: AtomicU64,
    request_timeslice: AtomicU64,
    timeslice: AtomicU64,    //剩余时间片
    start_time: AtomicU64,
    excuting_time: AtomicU64,     //单次实际执行的时间
    total_ex_time: AtomicU64,
    lag: AtomicI64,               //暂时不知道有什么用
}

impl FairAttr {
    pub fn new(nice: Nice) -> Self {
        FairAttr {
            weight: nice_to_weight(nice).into(),
            request_timeslice: Default::default(),

            vruntime: AtomicU64::new(2<<10),
            eligible_vruntime: Default::default(),
            vruntime_deadline: Default::default(),
            timeslice: Default::default(),

            start_time: Default::default(),
            excuting_time: Default::default(),
            total_ex_time: Default::default(),

            lag: Default::default(),

        }
    }

    pub fn update(&self, nice: Nice) {
        self.weight.store(nice_to_weight(nice), Relaxed);
    }

    pub fn update_request_timeslice(&self, request_timeslice: u64) {
        self.request_timeslice.store(request_timeslice, Relaxed);
    }
}

#[derive(Debug)]
pub(super) struct FairClassRq {
    #[expect(unused)]
    cpu: CpuId,
    tree: AugmentTree,
    current: Option<Arc<Task>>,
    min_vruntime: u64,
    total_vruntime: u64,
    total_weight: u64,
}

const VRUNTIME_BASE: u64 = 1024;
impl FairClassRq {
    pub fn new(cpu: CpuId) -> Self {
        Self {
            cpu,
            tree: AugmentTree::new(),
            current: None,
            min_vruntime: 0,
            total_vruntime: 0,
            total_weight: 0,
        }
    }

    fn period(&self) -> u64 {
        let base_slice_clks = base_slice_clocks();
        let min_period_clks = min_period_clocks();

        // `+ 1` means including the current running thread.
        let period_single_cpu =
            (base_slice_clks * (self.tree.len + 1) as u64).max(min_period_clks);
        period_single_cpu * u64::from((1 + num_cpus()).ilog2())
    }

    //先统一时间片吧
    fn time_slice(&self, cur_weight: u64) -> u64 {
        // self.period() * cur_weight / (self.total_weight + cur_weight)
        self.period() / (1 + self.tree.len as u64)
    }

    // 计算ve, vd, timeslice
    fn request(&self, fair_attr: &FairAttr, flags: Option<EnqueueFlags>) -> (u64, u64){
        let (ve, vd, timeslice) = match flags {
            Some(EnqueueFlags::Spawn) => {
                let timeslice = fair_attr.request_timeslice.load(Relaxed);
                let vruntime = fair_attr.vruntime.load(Relaxed);
                let weight = fair_attr.weight.load(Relaxed);
                (vruntime, vruntime + timeslice / weight, timeslice)
            },
            _ => {
                let timeslice = fair_attr.request_timeslice.load(Relaxed);
                let vruntime = fair_attr.vruntime.load(Relaxed);
                let weight = fair_attr.weight.load(Relaxed);
                (vruntime, vruntime + timeslice / weight, timeslice)
            }
        };

        fair_attr.timeslice.store(timeslice, Relaxed);
        fair_attr.eligible_vruntime.store(ve, Relaxed);
        fair_attr.vruntime_deadline.store(vd, Relaxed);
        fair_attr.excuting_time.store(0, Relaxed);

        (ve, vd)
    }
}
use crate::println;
use crate::print;

impl SchedClassRq for FairClassRq {
    ///入队分类讨论
    /// 从外界入队
    /// 1. 新生成的
    /// 2. 睡醒之类的
    /// 从cpu入队
    /// 3. 不配得
    /// 4. 时间片没有完被抢占的
    /// 
    /// 
    fn enqueue(&mut self, entity: Arc<Task>, flags: Option<EnqueueFlags>) { //当运行中的任务重新入队，flags为None
        let fair_attr = &entity.as_thread().unwrap().sched_attr().fair;
        match flags {
            Some(EnqueueFlags::Spawn) => {
                // myself
                let mut total_weight = self.total_weight;
                let mut total_vrumtime = self.total_vruntime;
                if let Some(task) = self.current.as_ref() {
                    let fair_attr = &task.as_thread().unwrap().sched_attr().fair;
                    let weight = fair_attr.weight.load(Relaxed);
                    let vruntime = fair_attr.vruntime.load(Relaxed);
                    total_weight += weight;
                    total_vrumtime += vruntime * weight;
                }
                let weight = fair_attr.weight.load(Relaxed);
                let vruntime = match total_weight {
                    0 => fair_attr.vruntime.load(Relaxed),
                    _ => total_vrumtime / total_weight,
                };
                fair_attr.update_request_timeslice(self.time_slice(weight));
                fair_attr.vruntime.store(vruntime, Relaxed);
                fair_attr.start_time.store(vruntime, Relaxed);
                fair_attr.total_ex_time.store(0, Relaxed);

                //queue
                self.total_weight += weight;
                self.total_vruntime += vruntime * weight;
                
                let (ve, mut vd) = self.request(fair_attr, flags);
                self.tree.insert(ve, vd, entity.clone());

            }
            //睡醒了
            Some(EnqueueFlags::Wake) => {
                // myself
                let mut total_weight = self.total_weight;
                let mut total_vrumtime = self.total_vruntime;
                if let Some(task) = self.current.as_ref() {
                    let fair_attr = &task.as_thread().unwrap().sched_attr().fair;
                    let weight = fair_attr.weight.load(Relaxed);
                    total_weight += weight;
                    total_vrumtime += fair_attr.vruntime.load(Relaxed) * weight;
                }
                let weight = fair_attr.weight.load(Relaxed);
                let mut vruntime = match total_weight {
                    0 => fair_attr.vruntime.load(Relaxed),
                    _ => total_vrumtime / total_weight,
                };
                // special for wake
                let lag = fair_attr.lag.load(Relaxed);
                vruntime = (vruntime as i64 - lag / ((total_weight + weight) as i64)) as u64;

                //myslef
                fair_attr.update_request_timeslice(self.time_slice(weight));
                fair_attr.vruntime.store(vruntime, Relaxed);
                fair_attr.start_time.store(vruntime, Relaxed);
                fair_attr.total_ex_time.store(0, Relaxed);
                //queue
                self.total_weight += weight;
                self.total_vruntime += vruntime * weight;
                
                let (ve, mut vd) = self.request(fair_attr, flags);
                self.tree.insert(ve, vd, entity.clone());

            }
            // 时间片未用完被抢占/不配得放回队列,也就是3.4
            None => {
                let weight = fair_attr.weight.load(Relaxed);
                let vruntime = fair_attr.vruntime.load(Relaxed);
                self.total_weight += weight;
                self.total_vruntime += vruntime * weight;

                let ve = fair_attr.eligible_vruntime.load(Relaxed);
                let vd = fair_attr.vruntime_deadline.load(Relaxed);
                self.tree.insert(ve, vd, entity.clone());
            }
        }
    }

    //目前还不能睡眠，因为持有锁
    //TODO: 释放锁让出时间片直到配得，注意part_current()的race condition
    //具体做法，配得时，先判断is_wake，如果醒了则不用dequeue了
    //注意is_wake和dequeue必须是wake()异步的临界区。
    fn dequeue(&mut self, task: Arc<Task>, mut rt: CurrentRuntime) -> bool {
        let fair_attr = &task.as_thread().unwrap().sched_attr().fair;
        let weight = fair_attr.weight.load(Relaxed);
        let vruntime = fair_attr.vruntime.load(Relaxed);
        let total_weight = self.total_weight + weight;
        let total_vruntime = self.total_vruntime + vruntime * weight;

        let weight = weight as i64;
        let start: i64 = fair_attr.start_time.load(Relaxed) as i64;
        let end: i64 = total_vruntime as i64 / total_weight as i64;
        let total_ex_time: i64 = fair_attr.total_ex_time.load(Relaxed) as i64;
        let lag = (end - start) * weight - total_ex_time;
        fair_attr.lag.store(lag, Relaxed);

        self.current = None;
        true
    }

    fn len(&self) -> usize {
        self.tree.len as usize
    }

    fn is_empty(&self) -> bool {
        self.tree.len == 0
    }

    fn pick_next(&mut self) -> Option<Arc<Task>> {
        if self.is_empty() {
            return None;
        }

        let mut total_weight = self.total_weight;
        let mut total_vrumtime = self.total_vruntime;
        if let Some(task) = self.current.as_ref() {
            let fair_attr = &task.as_thread().unwrap().sched_attr().fair;
            let weight = fair_attr.weight.load(Relaxed);
            total_weight += weight;
            total_vrumtime += fair_attr.vruntime.load(Relaxed) * weight;
        }

        let node = self.tree.pick(total_weight, total_vrumtime)?;
        self.tree.delete(node.clone(), None);

        let task = node.lock().task.clone();
        let fair_attr = &task.as_thread().unwrap().sched_attr().fair;
        let weight = fair_attr.weight.load(Relaxed);
        let vruntime = fair_attr.vruntime.load(Relaxed);
        self.total_weight -= weight;
        self.total_vruntime -= weight * vruntime;

        self.current = Some(task.clone());
        Some(task)
    }

    fn update_current(
        &mut self,
        rt: &CurrentRuntime,
        attr: &SchedAttr,
        flags: UpdateFlags,
    ) -> bool {
        //base information
        let fair_attr = &attr.fair;
        let weight = fair_attr.weight.load(Relaxed);
        let total_weight = self.total_weight + weight;
        //calculate time
        let vruntime_delta = rt.delta / total_weight;
        let realtime_delta = vruntime_delta * total_weight;
        //update
        fair_attr.vruntime.fetch_add(vruntime_delta, Relaxed);
        fair_attr.excuting_time.fetch_add(realtime_delta, Relaxed);
        fair_attr.total_ex_time.fetch_add(realtime_delta, Relaxed);

        match flags {
            UpdateFlags::Tick => {
                if fair_attr.excuting_time.load(Relaxed) < base_slice_clocks() {
                    return false;
                }

                let vruntime = fair_attr.vruntime.load(Relaxed);
                let total_vruntime = self.total_vruntime + vruntime * weight;
                if let Some(node) = self.tree.pick(total_weight, total_vruntime) {
                    if node.lock().vruntime_deadline < fair_attr.vruntime_deadline.load(Relaxed) {
                        return true;
                    }
                }
                if fair_attr.excuting_time.load(Relaxed) >= fair_attr.timeslice.load(Relaxed) {
                    self.request(&fair_attr, None);
                    if !vruntime_less(fair_attr.eligible_vruntime.load(Relaxed), total_weight, total_vruntime) {
                        return true
                    }
                }
                false
            }
            UpdateFlags::Yield => {
                true
            }
            UpdateFlags::Wait => {
                true
            }
        }
    }
}

#[inline(always)]
pub fn vruntime_less(ev: u64, total_weight: u64, total_vruntime: u64) -> bool {
    return (ev * total_weight) as i64 - total_vruntime as i64 <= 0;
}