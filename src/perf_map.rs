use rand::seq::SliceRandom;
use rand::thread_rng;
use rand::Rng;
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Barrier,
};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use crate::perf::calc_av_nanos;
use crate::perf::Measurement;

/// A collection that can be benchmarked by bustle.
///
/// Any thread that performs operations on the collection will first call `pin` and then perform
/// collection operations on the `Handle` that is returned. `pin` will not be called in the hot
/// loop of the benchmark.
pub trait Collection: Send + Sync + 'static {
    type Handle: CollectionHandle;
    fn pin(&self) -> Self::Handle;
    fn prefill_complete(&self);
}

/// A handle to a key-value collection.
///
/// Note that for all these methods, the benchmarker does not dictate what the values are. Feel
/// free to use the same value for all operations, or use distinct ones and check that your
/// retrievals indeed return the right results.
pub trait CollectionHandle {
    type Key: Clone + Send + Sync + FromU64;

    fn get(&self, key: &Self::Key) -> bool;
    fn insert(&self, key: Self::Key) -> bool;
    fn remove(&self, key: &Self::Key) -> bool;
    fn update(&self, key: &Self::Key) -> bool;
}

pub trait ValueModifier {
    fn modify(&mut self);
}

impl ValueModifier for u64 {
    fn modify(&mut self) {
        *self += 1;
    }
}

impl ValueModifier for String {
    fn modify(&mut self) {
        self.push_str("X");
    }
}

// Define the FromU64 trait
pub trait FromU64 {
    fn from_u64(value: u64) -> Self;
}

// Implement FromU64 for u64 (trivial conversion)
impl FromU64 for u64 {
    fn from_u64(value: u64) -> Self {
        value
    }
}

// Implement FromU64 for String (convert u64 to hexadecimal string)
impl FromU64 for String {
    fn from_u64(value: u64) -> Self {
        format!("{:x}", value)
    }
}

#[derive(Clone)] // Allow cloning if needed
pub struct Keys<TK: Clone + Send + Sync + FromU64> {
    allocated: Arc<AtomicUsize>,
    keys: Vec<TK>,
}

impl<TK> Keys<TK>
where
    TK: Send + Sync + Clone + FromU64,
{
    pub fn new(total_keys: usize) -> Self {
        let mut rng = rand::thread_rng();
        let mut unique_set = HashSet::new();

        while unique_set.len() < total_keys {
            unique_set.insert(rng.gen::<u64>());
        }

        Self {
            allocated: Arc::new(AtomicUsize::new(0)),
            keys: unique_set.into_iter().map(TK::from_u64).collect(),
        }
    }

    pub fn reset(&self) {
        self.allocated.store(0, Ordering::Relaxed);
    }

    pub fn random(&self, i: usize) -> TK {
        let allocated = self.allocated.load(Ordering::Relaxed);
        self.keys[i % allocated].clone()
    }

    // too slow
    // pub fn alloc(&self) -> TK {
    //     let i = self.allocated.fetch_add(1, Ordering::Relaxed);
    //     self.keys[i]
    // }

    pub fn alloc_n(&self, count: usize) -> &[TK] {
        let i = self.allocated.fetch_add(count, Ordering::Relaxed);
        &self.keys[i..(i + count)]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Read,
    Insert,
    Remove,
    Update,
    Upsert,
}

#[derive(Clone, Copy, Debug)]
pub struct Mix {
    pub read: usize,
    pub insert: usize,
    pub remove: usize,
    pub update: usize,
    pub upsert: usize,
}

impl Mix {
    /// Constructs a very read-heavy workload (~95%), with limited concurrent modifications.
    pub fn read_heavy() -> Self {
        Self {
            read: 95,
            insert: 2,
            update: 1,
            remove: 1,
            upsert: 1,
        }
    }

    pub fn read_99() -> Self {
        Self {
            read: 99,
            insert: 1,
            update: 0,
            remove: 0,
            upsert: 0,
        }
    }

    /// Constructs a read-only workload.
    pub fn read_100() -> Self {
        Self {
            read: 100,
            insert: 0,
            update: 0,
            remove: 0,
            upsert: 0,
        }
    }

    // Assuming 'Operation' enum is defined similarly to the previous examples
    pub fn to_ops(&self) -> Vec<Operation> {
        let mut list = Vec::with_capacity(100);
        list.extend(std::iter::repeat(Operation::Read).take(self.read as usize));
        list.extend(std::iter::repeat(Operation::Insert).take(self.insert as usize));
        list.extend(std::iter::repeat(Operation::Remove).take(self.remove as usize));
        list.extend(std::iter::repeat(Operation::Update).take(self.update as usize));
        list.extend(std::iter::repeat(Operation::Upsert).take(self.upsert as usize));
        list.shuffle(&mut rand::thread_rng());
        list
    }
}

#[derive(Debug, Clone)] // Add these derives for convenience if needed
pub struct SharedMapTestConfig<'a> {
    pub thread_count: usize,
    pub total_ops: usize,
    pub prefill: usize,
    pub operations: &'a Vec<Operation>,
    pub keys_needed_per_thread: usize,
}
fn run_ops<H: CollectionHandle>(
    dict: &H, // Assuming you have a ConcurrentDictionary type
    keys: &Arc<Keys<H::Key>>,
    op_mix: &[Operation],
    ops_per_thread: usize,
    keys_needed_per_thread: usize,
) -> usize {
    let mut rng = thread_rng();
    let op_mix_count = op_mix.len();
    let mut total_success = 0;
    let mut new_keys = keys.alloc_n(keys_needed_per_thread).iter().cycle();

    for i in 0..ops_per_thread {
        let op = op_mix[i % op_mix_count];
        let r = rng.gen::<usize>(); // Generate a random usize
        let success = match op {
            Operation::Read => dict.get(&keys.random(r)),
            Operation::Insert => dict.insert(new_keys.next().unwrap().clone()),
            Operation::Remove => dict.remove(&keys.random(r)),
            Operation::Update => {
                dict.update(&keys.random(r))
                // if let Some(existing_value) = dict.get(&keys.random(r)) {
                //     dict.insert(keys.random(r), existing_value + 1).is_some()
                // } else {
                //     false
                // }
            }
            Operation::Upsert => {
                // Note: Rust's `insert` always returns the old value, even if the key didn't exist before
                //let old_value = dict.insert(keys.random(r), 1);
                //old_value.is_none() || old_value.unwrap() == 0
                dict.update(&keys.random(r))
            }
        };

        total_success += if success { 0 } else { 1 };
    }

    total_success
}

pub fn run_shared_map_test<'a, H: Collection>(
    name: &'a str,
    collection: Arc<H>,
    config: &SharedMapTestConfig,
    keys: &'a Arc<Keys<<<H as Collection>::Handle as CollectionHandle>::Key>>,
) -> Measurement<'a> {
    let num_threads = config.thread_count;

    print!("Map {name:8} (threads {num_threads:>3}) ... ");

    let barrier = Arc::new(Barrier::new(num_threads + 1));
    let mut thread_handles = Vec::with_capacity(num_threads);
    let ops_per_thread = config.total_ops / num_threads;
    let results = Arc::new(Mutex::new(Vec::<Duration>::new()));

    keys.reset();
    let mut new_keys = keys.alloc_n(config.prefill).iter().cycle();
    let inserter = collection.pin();
    for _ in 0..config.prefill {
        inserter.insert(new_keys.next().unwrap().clone());
    }

    collection.prefill_complete();

    // uncomment for core affinity
    // affinity: let core_ids = get_core_ids().expect("Failed to get core IDs");

    for _ in 0..num_threads {
        let operations = config.operations.clone();
        let keys_needed_per_thread = config.keys_needed_per_thread;
        let barrier = barrier.clone();
        let results_clone = results.clone();
        let collection = collection.clone();
        let keys = keys.clone();
        // affinity: let core_id = core_ids[n % core_ids.len()];
        // affinity: let core_id_usize = core_id.id as usize;

        let handle = thread::spawn(move || {
            // affinity: set_thread_affinity(&[core_id_usize]).expect("Failed to set thread affinity");
            let dict = collection.pin();
            barrier.wait();
            let start_time = Instant::now();
            run_ops(
                &dict,
                &keys,
                &operations,
                ops_per_thread,
                keys_needed_per_thread,
            );

            let elapsed = start_time.elapsed();
            let mut results = results_clone.lock().unwrap();
            results.push(elapsed);
        });

        thread_handles.push(handle);
    }

    barrier.wait();
    for handle in thread_handles {
        handle.join().unwrap();
    }

    let real_total_ops = ops_per_thread as u64 * num_threads as u64;
    let average_duration = calc_av_nanos(results, real_total_ops);

    println!("avg: {:8.2} ns", average_duration);

    Measurement {
        name,
        latency: average_duration,
        thread_count: num_threads as u64,
    }
}

pub(crate) trait MapAdapter<K, V> {
    fn insert(&mut self, key: K, value: V);
    fn get(&self, key: &K) -> Option<V>;
}

pub(crate) fn run_map_test<'a, M>(
    name: &'a str,
    op_count: usize,
    prefill: usize,
    map: &mut M,
) -> Measurement<'a>
where
    M: MapAdapter<u64, u64>,
{
    print!("map test {name} ... ");
    let mut rng = rand::thread_rng();    

    // Prefill the map with random values
    for i in 0..prefill {
        let value = 1; //rng.gen::<u64>();
        map.insert(i as u64, value);
    }

    let start_time = Instant::now();

    // Perform read operations and measure the time
    for i in 0..op_count {
        let key = (i % prefill) as u64; //rng.gen_range(0..prefill as u64); // Generate keys within the prefill range
        if let Some(value) = map.get(&key) {
            std::hint::black_box(value.clone()); // Use clone to avoid ownership issues
        }
    }

    let elapsed = start_time.elapsed();
    let average_duration = elapsed.as_nanos() as f64 / op_count as f64;

    println!("avg: {:8.2} ns", average_duration);

    Measurement {
        name,
        latency: average_duration,
        thread_count: (prefill / 1000) as u64,
    }
}
