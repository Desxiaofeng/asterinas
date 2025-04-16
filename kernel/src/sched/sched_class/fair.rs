// SPDX-License-Identifier: MPL-2.0

use alloc::{collections::BinaryHeap, sync::Arc};
use fixed::types::extra::True;
use core::{
    cmp::{self, Reverse}, sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed}
};
use core::sync::atomic::Ordering::SeqCst;
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
    thread::AsThread,
};

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
    eligible_vruntime: AtomicU64, //同时等于每一次时间片开始的时间vruntime_start
    vruntime_deadline: AtomicU64,
    request_timeslice: AtomicU64,
    timeslice: AtomicU64,    //剩余时间片
    start_time: AtomicU64,
    excuting_time: AtomicU64,     //实际执行的时间
    total_ex_time: AtomicU64,
    lag: AtomicI64,               //暂时不知道有什么用
}

impl FairAttr {
    pub fn new(nice: Nice) -> Self {
        FairAttr {
            weight: nice_to_weight(nice).into(),
            request_timeslice: Default::default(),

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

struct ActiveItem {
    task: Arc<Task>,
    ve: u64,
    vd: u64,
}

impl ActiveItem {
    fn new(task: Arc<Task>, ve: u64, vd: u64) -> Self{
        ActiveItem {
            task,
            ve,
            vd,
        }
    }

    fn key(&self) -> u64 {
        self.ve
    }
}

impl core::fmt::Debug for ActiveItem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.key())
    }
}

impl PartialEq for ActiveItem {
    fn eq(&self, other: &Self) -> bool {
        self.key().eq(&other.key())
    }
}

impl Eq for ActiveItem {}

impl PartialOrd for ActiveItem {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ActiveItem {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

#[derive(Debug)]
pub(super) struct FairClassRq {
    #[expect(unused)]
    cpu: CpuId,
    ves: BinaryHeap<Reverse<ActiveItem>>,
    vruntime: u64,
    total_weight: u64,
}

const VRUNTIME_BASE: u64 = 1024;
impl FairClassRq {
    pub fn new(cpu: CpuId) -> Self {
        Self {
            cpu,
            ves: BinaryHeap::new(),
            vruntime: 1<<10,
            total_weight: 0,
        }
    }

    fn period(&self) -> u64 {
        let base_slice_clks = base_slice_clocks();
        let min_period_clks = min_period_clocks();

        // `+ 1` means including the current running thread.
        let period_single_cpu =
            (base_slice_clks * (self.ves.len() + 1) as u64).max(min_period_clks);
        period_single_cpu * u64::from((1 + num_cpus()).ilog2())
    }

    //先统一时间片吧
    fn time_slice(&self, cur_weight: u64) -> u64 {
        // self.period() * cur_weight / (self.total_weight + cur_weight)
        self.period() / (1 + self.len() as u64)
    }

    fn request(&self, fair_attr: &FairAttr, flags: Option<EnqueueFlags>) -> (u64, u64){
        let (ve, vd, timeslice) = match flags {
            Some(EnqueueFlags::Spawn) => {
                let timeslice = fair_attr.request_timeslice.load(SeqCst);
                fair_attr.start_time.store(self.vruntime, SeqCst);
                fair_attr.total_ex_time.store(0, SeqCst);

                (self.vruntime, self.vruntime + timeslice / fair_attr.weight.load(SeqCst), timeslice)
            },
            _ => {
                let timeslice = fair_attr.request_timeslice.load(SeqCst);
                let ve = fair_attr.eligible_vruntime.load(SeqCst) 
                    + fair_attr.excuting_time.load(SeqCst)/ fair_attr.weight.load(SeqCst);
                let vd = ve + timeslice / fair_attr.weight.load(SeqCst);

                (ve, vd, timeslice)
            }
        };

        fair_attr.timeslice.store(timeslice, SeqCst);
        fair_attr.eligible_vruntime.store(ve, SeqCst);
        fair_attr.vruntime_deadline.store(vd, SeqCst);
        fair_attr.excuting_time.store(0, SeqCst);

        (ve, vd)
    }

    fn update_ves(&mut self, attr: &FairAttr) {
        let delta = attr.eligible_vruntime.load(Relaxed) as i64 
        - self.vruntime as i64;
        if delta > 0 && self.is_empty(){
            println!("hanpen with {}", delta);
            // self.vruntime = attr.eligible_vruntime.load(Relaxed);
        }
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
        let weight = fair_attr.weight.load(Relaxed);
        

        match flags {
            Some(EnqueueFlags::Spawn) => {
                fair_attr.update_request_timeslice(self.time_slice(weight));
                self.total_weight += weight;

                let (ve, vd) = self.request(fair_attr, flags);

                self.ves.push(Reverse(ActiveItem{
                    task: Arc::clone(&entity), 
                    ve: ve,
                    vd: vd,
                }));
            }
            //睡醒了
            Some(EnqueueFlags::Wake) => {
                self.total_weight += weight;
                let lag = fair_attr.lag.load(Relaxed);

                if self.is_empty() || lag >= 0 {
                    let (ve, vd) = self.request(fair_attr, Some(EnqueueFlags::Spawn));
                    self.ves.push(Reverse(ActiveItem{
                        task: Arc::clone(&entity), 
                        ve: ve,
                        vd: vd,
                    }));
                } else {
                    // 伪装过去申请时间片
                    fair_attr.eligible_vruntime.store(self.vruntime, Relaxed);
                    fair_attr.vruntime_deadline.store(
                        self.vruntime + fair_attr.timeslice.load(Relaxed) / weight, 
                        Relaxed);
                    fair_attr.start_time.store(self.vruntime , Relaxed);
                    fair_attr.excuting_time.store((-lag)as u64 , Relaxed);
                    fair_attr.total_ex_time.store((-lag)as u64, Relaxed);

                    //误差来源
                    self.vruntime = (self.vruntime as i64 - (lag / (self.total_weight) as i64) as i64) as u64;
                    

                    self.ves.push(Reverse(ActiveItem{
                        task: Arc::clone(&entity), 
                        ve: fair_attr.eligible_vruntime.load(Relaxed),
                        vd: fair_attr.vruntime_deadline.load(Relaxed),
                    }));
                }
            }
            // 时间片未用完被抢占/不配得放回队列,也就是3.4
            None => {
                let ve = fair_attr.eligible_vruntime.load(Relaxed);
                let vd = fair_attr.vruntime_deadline.load(Relaxed);
                
                self.ves.push(Reverse(ActiveItem{
                    task: entity, 
                    ve: ve,
                    vd: vd,
                }));
            }
        }
    }

    //目前还不能睡眠，因为持有锁
    //TODO: 释放锁让出时间片直到配得，注意part_current()的race condition
    //具体做法，配得时，先判断is_wake，如果醒了则不用dequeue了
    //注意is_wake和dequeue必须是wake()异步的临界区。
    fn dequeue(&mut self, task: Arc<Task>, mut rt: CurrentRuntime) -> bool {
        let fair_attr = &task.as_thread().unwrap().sched_attr().fair;
        // rt.update();
        // fair_attr.excuting_time.fetch_add(rt.delta, Relaxed);
        // self.vruntime += rt.delta / self.total_weight;
        // if fair_attr.eligible_vruntime.load(SeqCst) > self.vruntime{
        //     println!("{}, {}", fair_attr.eligible_vruntime.load(Relaxed)-self.vruntime, self.len());
        // }
    
        let start: i64 = fair_attr.start_time.load(SeqCst) as i64;
        let end: i64 = self.vruntime as i64;
        let weight = fair_attr.weight.load(SeqCst) as i64;
        let total_ex_time: i64 = fair_attr.total_ex_time.load(SeqCst) as i64;
        let lag = (end - start) * weight - total_ex_time;
        
        fair_attr.lag.store(lag, SeqCst);
        self.total_weight -= fair_attr.weight.load(SeqCst);

        // let mut total_lag = 0 as i64;

        // for Reverse(ActiveItem{task, ve, vd}) in &self.ves {
        //     total_lag += 
        //     ((self.vruntime) as i64 - (task.as_thread().unwrap().sched_attr().fair.start_time.load(SeqCst)) as i64)
        //     * (task.as_thread().unwrap().sched_attr().fair.weight.load(SeqCst)) as i64
        //     - task.as_thread().unwrap().sched_attr().fair.total_ex_time.load(SeqCst) as i64
        // }
        // total_lag += lag;
        // println!("{}, {}, {}", lag, total_lag, self.len());

        if self.total_weight == 0 {
            return true;
        } else {
            //误差来源
            self.vruntime = (self.vruntime as i64 + ((lag + (self.total_weight*73/100)as i64)/ self.total_weight as i64) as i64) as u64;

            return true
        }

    }

    fn len(&self) -> usize {
        self.ves.len()
    }

    fn is_empty(&self) -> bool {
        self.ves.is_empty()
    }

    fn pick_next(&mut self) -> Option<Arc<Task>> {
        let Reverse(ActiveItem{task, ve, vd}) = self.ves.pop()?;
        if ve > self.vruntime{
            
            // println!("wrong! {}, {}", ve - self.vruntime, self.len());
        }
        Some(task)
    }

    fn update_current(
        &mut self,
        rt: &CurrentRuntime,
        attr: &SchedAttr,
        flags: UpdateFlags,
    ) -> bool {
        let weight = attr.fair.weight.load(SeqCst);
        let vruntime_delta = rt.delta / self.total_weight;
        let realtime_delta = vruntime_delta * self.total_weight;
        self.vruntime += vruntime_delta;
        attr.fair.excuting_time.fetch_add(realtime_delta, SeqCst);
        attr.fair.total_ex_time.fetch_add(realtime_delta, SeqCst);
        // println!("{}, {}, {}", rt.delta, rt.delta / self.total_weight, self.total_weight);
        if attr.fair.excuting_time.load(SeqCst) >= attr.fair.timeslice.load(Relaxed) {
            self.request(&attr.fair, None);
        }
        // println!("{},{}", self.vruntime, attr.fair.eligible_vruntime.load(Relaxed));
        // self.update_ves(&attr.fair);
        match flags {
            UpdateFlags::Yield => {
                true
            },
            UpdateFlags::Tick => {

                // if attr.fair.excuting_time.load(Relaxed) < base_slice_clocks() {
                //     return false;
                // }
                //尝试从vds中拿一个出来，如果vd更小则抢占
                if let Some(Reverse(ActiveItem{task, ve, vd})) = self.ves.peek() {
                    if *vd < attr.fair.vruntime_deadline.load(Relaxed) 
                    || true {
                        return true;
                    }
                }
                if attr.fair.eligible_vruntime.load(Relaxed) > self.vruntime {
                    return true;
                }
                false
            }
            //睡眠前走这里
            UpdateFlags::Wait => {
                true
            }
        }
    }
}
