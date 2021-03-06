//! A thread pool for running blocking functions asynchronously.
//!
//! Blocking thread pool consists of four elements:
//! * Frequency Detector
//! * Trend Estimator
//! * Predictive Upscaler
//! * Time-based Downscaler
//!
//! ## Frequency Detector
//! Detects how many tasks are submitted from scheduler to thread pool in a given time frame.
//! Pool manager thread does this sampling every 200 milliseconds.
//! This value is going to be used for trend estimation phase.
//!
//! ## Trend Estimator
//! Hold up to the given number of frequencies to create an estimation.
//! Trend estimator holds 10 frequencies at a time.
//! This value is stored as constant in [FREQUENCY_QUEUE_SIZE](constant.FREQUENCY_QUEUE_SIZE.html).
//! Estimation algorithm and prediction uses Exponentially Weighted Moving Average algorithm.
//!
//! This algorithm is adapted from [A Novel Predictive and Self–Adaptive Dynamic Thread Pool Management](https://doi.org/10.1109/ISPA.2011.61)
//! and altered to:
//! * use instead of heavy calculation of trend, utilize thread redundancy which is the sum of the differences between the predicted and observed value.
//! * use instead of linear trend estimation, it uses exponential trend estimation where formula is:
//! ```text
//! LOW_WATERMARK * (predicted - observed) + LOW_WATERMARK
//! ```
//! *NOTE:* If this algorithm wants to be tweaked increasing [LOW_WATERMARK](constant.LOW_WATERMARK.html) will automatically adapt the additional dynamic thread spawn count
//! * operate without watermarking by timestamps (in paper which is used to measure algorithms own performance during the execution)
//! * operate extensive subsampling. Extensive subsampling congests the pool manager thread.
//! * operate without keeping track of idle time of threads or job out queue like TEMA and FOPS implementations.
//!
//! ## Predictive Upscaler
//! Upscaler has three cases (also can be seen in paper):
//! * The rate slightly increases and there are many idle threads.
//! * The number of worker threads tends to be reduced since the workload of the system is descending.
//! * The system has no request or stalled. (Our case here is when the current tasks block further tasks from being processed – throughput hogs)
//!
//! For the first two EMA calculation and exponential trend estimation gives good performance.
//! For the last case, upscaler selects upscaling amount by amount of tasks mapped when throughput hogs happen.
//!
//! **example scenario:** Let's say we have 10_000 tasks where every one of them is blocking for 1 second. Scheduler will map plenty of tasks but will got rejected.
//! This makes estimation calculation nearly 0 for both entering and exiting parts. When this happens and we still see tasks mapped from scheduler.
//! We start to slowly increase threads by amount of frequency linearly. High increase of this value either make us hit to the thread threshold on
//! some OS or make congestion on the other thread utilizations of the program, because of context switch.
//!
//! Throughput hogs determined by a combination of job in / job out frequency and current scheduler task assignment frequency.
//! Threshold of EMA difference is eluded by machine epsilon for floating point arithmetic errors.
//!
//! ## Time-based Downscaler
//! When threads becomes idle, they will not shut down immediately.
//! Instead, they wait a random amount between 1 and 11 seconds
//! to even out the load.

use std::collections::VecDeque;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::{env, thread};

use crossbeam_channel::{bounded, Receiver, Sender};
use lazy_static::lazy_static;

use crate::{load_balancer, placement, utils};
use lightproc::lightproc::LightProc;
use lightproc::proc_stack::ProcStack;
use lightproc::recoverable_handle::RecoverableHandle;
use std::future::Future;
use std::io::ErrorKind;
use std::iter::Iterator;

use crate::placement::CoreId;

use std::sync::Mutex;

/// If low watermark isn't configured this is the default scaler value.
/// This value is used for the heuristics of the scaler
const DEFAULT_LOW_WATERMARK: u64 = 2;

/// Pool managers interval time (milliseconds).
/// This is the actual interval which makes adaptation calculation.
const MANAGER_POLL_INTERVAL: u64 = 200;

/// Frequency histogram's sliding window size.
/// Defines how many frequencies will be considered for adaptation.
const FREQUENCY_QUEUE_SIZE: usize = 10;

/// Exponential moving average smoothing coefficient for limited window.
/// Smoothing factor is estimated with: 2 / (N + 1) where N is sample size.
const EMA_COEFFICIENT: f64 = 2_f64 / (FREQUENCY_QUEUE_SIZE as f64 + 1_f64);

/// Pool task frequency variable.
/// Holds scheduled tasks onto the thread pool for the calculation time window.
static FREQUENCY: AtomicU64 = AtomicU64::new(0);

/// Possible max threads (without OS contract).
static MAX_THREADS: AtomicU64 = AtomicU64::new(10_000);

/// Pool interface between the scheduler and thread pool
struct Pool {
    sender: Sender<LightProc>,
    receiver: Receiver<LightProc>,
}

lazy_static! {
    /// Blocking pool with static starting thread count.
    static ref POOL: Pool = {
        for _ in 0..*low_watermark() {
            thread::Builder::new()
                .name("bastion-blocking-driver".to_string())
                .spawn(|| {
                    self::affinity_pinner();

                    for task in &POOL.receiver {
                        task.run();
                    }
                })
                .expect("cannot start a thread driving blocking tasks");
        }

        // Pool manager to check frequency of task rates
        // and take action by scaling the pool accordingly.
        thread::Builder::new()
            .name("bastion-pool-manager".to_string())
            .spawn(|| {
                let poll_interval = Duration::from_millis(MANAGER_POLL_INTERVAL);
                loop {
                    scale_pool();
                    thread::sleep(poll_interval);
                }
            })
            .expect("thread pool manager cannot be started");

        // We want to use an unbuffered channel here to help
        // us drive our dynamic control. In effect, the
        // kernel's scheduler becomes the queue, reducing
        // the number of buffers that work must flow through
        // before being acted on by a core. This helps keep
        // latency snappy in the overall async system by
        // reducing bufferbloat.
        let (sender, receiver) = bounded(0);
        Pool { sender, receiver }
    };

    static ref ROUND_ROBIN_PIN: Mutex<CoreId> = Mutex::new(CoreId { id: 0 });

    /// Sliding window for pool task frequency calculation
    static ref FREQ_QUEUE: Mutex<VecDeque<u64>> = {
        Mutex::new(VecDeque::with_capacity(FREQUENCY_QUEUE_SIZE.saturating_add(1)))
    };

    /// Dynamic pool thread count variable
    static ref POOL_SIZE: Mutex<u64> = Mutex::new(*low_watermark());
}

/// Exponentially Weighted Moving Average calculation
///
/// This allows us to find the EMA value.
/// This value represents the trend of tasks mapped onto the thread pool.
/// Calculation is following:
/// ```text
/// +--------+-----------------+----------------------------------+
/// | Symbol |   Identifier    |           Explanation            |
/// +--------+-----------------+----------------------------------+
/// | α      | EMA_COEFFICIENT | smoothing factor between 0 and 1 |
/// | Yt     | freq            | frequency sample at time t       |
/// | St     | acc             | EMA at time t                    |
/// +--------+-----------------+----------------------------------+
/// ```
/// Under these definitions formula is following:
/// ```text
/// EMA = α * [ Yt + (1 - α)*Yt-1 + ((1 - α)^2)*Yt-2 + ((1 - α)^3)*Yt-3 ... ] + St
/// ```
/// # Arguments
///
/// * `freq_queue` - Sliding window of frequency samples
#[inline]
fn calculate_ema(freq_queue: &VecDeque<u64>) -> f64 {
    freq_queue.iter().enumerate().fold(0_f64, |acc, (i, freq)| {
        acc + ((*freq as f64) * ((1_f64 - EMA_COEFFICIENT).powf(i as f64) as f64))
    }) * EMA_COEFFICIENT as f64
}

/// Adaptive pool scaling function
///
/// This allows to spawn new threads to make room for incoming task pressure.
/// Works in the background detached from the pool system and scales up the pool based
/// on the request rate.
///
/// It uses frequency based calculation to define work. Utilizing average processing rate.
fn scale_pool() {
    // Fetch current frequency, it does matter that operations are ordered in this approach.
    let current_frequency = FREQUENCY.swap(0, Ordering::SeqCst);
    let mut freq_queue = FREQ_QUEUE.lock().unwrap();

    // Make it safe to start for calculations by adding initial frequency scale
    if freq_queue.len() == 0 {
        freq_queue.push_back(0);
    }

    // Calculate message rate for the given time window
    let frequency = (current_frequency as f64 / MANAGER_POLL_INTERVAL as f64) as u64;

    // Calculates current time window's EMA value (including last sample)
    let prev_ema_frequency = calculate_ema(&freq_queue);

    // Add seen frequency data to the frequency histogram.
    freq_queue.push_back(frequency);
    if freq_queue.len() == FREQUENCY_QUEUE_SIZE.saturating_add(1) {
        freq_queue.pop_front();
    }

    // Calculates current time window's EMA value (including last sample)
    let curr_ema_frequency = calculate_ema(&freq_queue);

    // Adapts the thread count of pool
    //
    // Sliding window of frequencies visited by the pool manager.
    // Pool manager creates EMA value for previous window and current window.
    // Compare them to determine scaling amount based on the trends.
    // If current EMA value is bigger, we will scale up.
    if curr_ema_frequency > prev_ema_frequency {
        // "Scale by" amount can be seen as "how much load is coming".
        // "Scale" amount is "how many threads we should spawn".
        let scale_by: f64 = curr_ema_frequency - prev_ema_frequency;
        let scale = num_cpus::get().min(
            ((DEFAULT_LOW_WATERMARK as f64 * scale_by) + DEFAULT_LOW_WATERMARK as f64) as usize,
        );

        // It is time to scale the pool!
        (0..scale).for_each(|_| {
            create_blocking_thread();
        });
    } else if (curr_ema_frequency - prev_ema_frequency).abs() < std::f64::EPSILON
        && current_frequency != 0
    {
        // Throughput is low. Allocate more threads to unblock flow.
        // If we fall to this case, scheduler is congested by longhauling tasks.
        // For unblock the flow we should add up some threads to the pool, but not that many to
        // stagger the program's operation.
        (0..DEFAULT_LOW_WATERMARK).for_each(|_| {
            create_blocking_thread();
        });
    }
}

/// Creates blocking thread to receive tasks
/// Dynamic threads will terminate themselves if they don't
/// receive any work after between one and ten seconds.
fn create_blocking_thread() {
    // Check that thread is spawnable.
    // If it hits to the OS limits don't spawn it.
    {
        let pool_size = *POOL_SIZE.lock().unwrap();
        if pool_size >= MAX_THREADS.load(Ordering::SeqCst) {
            MAX_THREADS.store(10_000, Ordering::SeqCst);
            return;
        }
    }
    // We want to avoid having all threads terminate at
    // exactly the same time, causing thundering herd
    // effects. We want to stagger their destruction over
    // 10 seconds or so to make the costs fade into
    // background noise.
    //
    // Generate a simple random number of milliseconds
    let rand_sleep_ms = 1000_u64
        .checked_add(u64::from(utils::random(10_000)))
        .expect("shouldn't overflow");

    let _ = thread::Builder::new()
        .name("bastion-blocking-driver-dynamic".to_string())
        .spawn(move || {
            self::affinity_pinner();

            let wait_limit = Duration::from_millis(rand_sleep_ms);

            // Adjust the pool size counter before and after spawn
            *POOL_SIZE.lock().unwrap() += 1;
            while let Ok(task) = POOL.receiver.recv_timeout(wait_limit) {
                task.run();
            }
            *POOL_SIZE.lock().unwrap() -= 1;
        })
        .map_err(|err| {
            match err.kind() {
                ErrorKind::WouldBlock => {
                    // Maximum allowed threads per process is varying from system to system.
                    // Also, some systems have it(like macOS), and some don't(Linux).
                    // This case expected not to happen.
                    // But when happened this shouldn't throw a panic.
                    let guarded_count = POOL_SIZE
                        .lock()
                        .unwrap()
                        .checked_sub(1)
                        .expect("shouldn't underflow");
                    MAX_THREADS.store(guarded_count, Ordering::SeqCst);
                }
                _ => eprintln!(
                    "cannot start a dynamic thread driving blocking tasks: {}",
                    err
                ),
            }
        });
}

/// Enqueues work, attempting to send to the thread pool in a
/// nonblocking way and spinning up needed amount of threads
/// based on the previous statistics without relying on
/// if there is not a thread ready to accept the work or not.
fn schedule(t: LightProc) {
    // Add up for every incoming scheduled task
    FREQUENCY.fetch_add(1, Ordering::Acquire);

    if let Err(err) = POOL.sender.try_send(t) {
        // We were not able to send to the channel without
        // blocking.
        POOL.sender.send(err.into_inner()).unwrap();
    }
}

/// Spawns a blocking task.
///
/// The task will be spawned onto a thread pool specifically dedicated to blocking tasks.
pub fn spawn_blocking<F, R>(future: F, stack: ProcStack) -> RecoverableHandle<R>
where
    F: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    let (task, handle) = LightProc::recoverable(future, schedule, stack);
    task.schedule();
    handle
}

///
/// Low watermark value, defines the bare minimum of the pool.
/// Spawns initial thread set.
/// Can be configurable with env var `BASTION_BLOCKING_THREADS` at runtime.
#[inline]
pub fn low_watermark() -> &'static u64 {
    lazy_static! {
        static ref LOW_WATERMARK: u64 = {
            env::var_os("BASTION_BLOCKING_THREADS")
                .map(|x| x.to_str().unwrap().parse::<u64>().unwrap())
                .unwrap_or(DEFAULT_LOW_WATERMARK)
        };
    }

    &*LOW_WATERMARK
}

///
/// Affinity pinner for blocking pool
/// Pinning isn't going to be enabled for single core systems.
#[inline]
pub fn affinity_pinner() {
    if 1 != *load_balancer::core_retrieval() {
        let mut core = ROUND_ROBIN_PIN.lock().unwrap();
        placement::set_for_current(*core);
        core.id = (core.id + 1) % *load_balancer::core_retrieval();
    }
}
