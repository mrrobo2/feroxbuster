use super::FeroxScanner;
use crate::{
    atomic_load, atomic_store,
    config::RequesterPolicy,
    event_handlers::{
        Command::{self, AddError, SubtractFromUsizeField},
        Handles,
    },
    extractor::{ExtractionTarget::ResponseBody, ExtractorBuilder},
    response::FeroxResponse,
    scan_manager::ScanStatus,
    statistics::{StatError::Other, StatField::TotalExpected},
    url::FeroxUrl,
    utils::logged_request,
    HIGH_ERROR_RATIO,
};
use anyhow::Result;

use crate::scan_manager::FeroxScan;
use leaky_bucket::LeakyBucket;
use std::{
    cmp::max,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::{
    sync::{oneshot, RwLock},
    time::{sleep, Duration},
};

#[derive(Copy, Clone, PartialEq, Debug)]
/// represents different situations where different criteria can trigger auto-tune/bail behavior
pub enum PolicyTrigger {
    /// excessive 403 trigger
    Status403,

    /// excessive 429 trigger
    Status429,

    /// excessive general errors
    Errors,
}

/// data regarding policy and metadata about last enforced trigger etc...
#[derive(Default, Debug)]
pub struct PolicyData {
    /// how to handle exceptional cases such as too many errors / 403s / 429s etc
    policy: RequesterPolicy,

    /// whether or not we're in the middle of a cooldown period
    cooling_down: AtomicBool,

    /// length of time to pause tuning after making an adjustment
    wait_time: u64,

    /// rate limit (at last interval)
    limit: AtomicUsize,

    /// number of errors (at last interval)
    errors: AtomicUsize,

    /// whether or not the owning Requester should remove the rate_limiter, happens when a scan
    /// has been limited and moves back up to the point of its original scan speed
    remove_limit: AtomicBool,

    /// heap of values used for adjusting # of requests/second
    heap: std::sync::RwLock<LimitHeap>,
}

/// implementation of PolicyData
impl PolicyData {
    /// given a RequesterPolicy, create a new PolicyData
    fn new(policy: RequesterPolicy, timeout: u64) -> Self {
        // can use this as a tweak for how aggressively adjustments should be made when tuning
        let wait_time = ((timeout as f64 / 2.0) * 1000.0) as u64;

        Self {
            policy,
            wait_time,
            ..Default::default()
        }
    }

    /// setter for requests / second; populates the underlying heap with values from req/sec seed
    fn set_reqs_sec(&self, reqs_sec: usize) {
        if let Ok(mut guard) = self.heap.write() {
            guard.original = reqs_sec as i32;
            guard.build();
            self.set_limit(guard.inner[0] as usize); // set limit to 1/2 of current request rate
        }
    }

    /// setter for errors
    fn set_errors(&self, errors: usize) {
        atomic_store!(self.errors, errors);
    }

    /// setter for limit
    fn set_limit(&self, limit: usize) {
        atomic_store!(self.limit, limit);
    }

    /// getter for limit
    fn get_limit(&self) -> usize {
        atomic_load!(self.limit)
    }

    /// adjust the rate of requests per second up (increase rate)
    fn adjust_up(&self, streak_counter: &usize) {
        if let Ok(mut heap) = self.heap.try_write() {
            if *streak_counter > 2 {
                // streak of 3 upward moves in a row, traverse the tree upward instead of to a
                // higher-valued branch lower in the tree
                let current = heap.value();
                heap.move_up();
                heap.move_up();
                if current > heap.value() {
                    // the tree's structure makes it so that sometimes 2 moves up results in a
                    // value greater than the current node's and other times we need to move 3 up
                    // to arrive at a greater value
                    if heap.has_parent() && heap.parent_value() > current {
                        // all nodes except 0th node (root)
                        heap.move_up();
                    } else if !heap.has_parent() {
                        // been here enough that we can try resuming the scan to its original
                        // speed (no limiting at all)
                        atomic_store!(self.remove_limit, true);
                    }
                }
                self.set_limit(heap.value() as usize);
            } else if heap.has_children() {
                // streak not at 3, just check that we can move down, and do so
                heap.move_left();
                self.set_limit(heap.value() as usize);
            } else {
                // tree bottomed out, need to move back up the tree a bit
                let current = heap.value();
                heap.move_up();
                heap.move_up();

                if current > heap.value() {
                    heap.move_up();
                }

                self.set_limit(heap.value() as usize);
            }
        }
    }

    /// adjust the rate of requests per second down (decrease rate)
    fn adjust_down(&self) {
        if let Ok(mut heap) = self.heap.try_write() {
            if heap.has_children() {
                heap.move_right();
                self.set_limit(heap.value() as usize);
            }
        }
    }
}

/// bespoke variation on an array-backed max-heap
///
/// 255 possible values generated from the initial requests/second
///
/// when no additional errors are encountered, the left child is taken (increasing req/sec)
/// if errors have increased since the last interval, the right child is taken (decreasing req/sec)
///
/// formula for each child:
/// - left: (|parent - current|) / 2 + current
/// - right: current - ((|parent - current|) / 2)
#[derive(Debug)]
struct LimitHeap {
    /// backing array, 255 nodes == height of 7 ( 2^(h+1) -1 nodes )
    inner: [i32; 255],

    /// original # of requests / second
    original: i32,

    /// current position w/in the backing array
    current: usize,
}

/// default implementation of a LimitHeap
impl Default for LimitHeap {
    /// zero-initialize the backing array
    fn default() -> Self {
        Self {
            inner: [0; 255],
            original: 0,
            current: 0,
        }
    }
}

/// implementation of a LimitHeap
impl LimitHeap {
    /// move to right child, return node's index from which the move was requested
    fn move_right(&mut self) -> usize {
        if self.has_children() {
            let tmp = self.current;
            self.current = self.current * 2 + 2;
            return tmp;
        }
        self.current
    }

    /// move to left child, return node's index from which the move was requested
    fn move_left(&mut self) -> usize {
        if self.has_children() {
            let tmp = self.current;
            self.current = self.current * 2 + 1;
            return tmp;
        }
        self.current
    }

    /// move to parent, return node's index from which the move was requested
    fn move_up(&mut self) -> usize {
        if self.has_parent() {
            let tmp = self.current;
            self.current = (self.current - 1) / 2;
            return tmp;
        }
        self.current
    }

    /// move directly to the given index
    fn move_to(&mut self, index: usize) {
        self.current = index;
    }

    /// get the current node's value
    fn value(&self) -> i32 {
        self.inner[self.current]
    }

    /// set the current node's value
    fn set_value(&mut self, value: i32) {
        self.inner[self.current] = value;
    }

    /// check that this node has a parent (true for all except root)
    fn has_parent(&self) -> bool {
        self.current > 0
    }

    /// get node's parent's value or self.original if at the root
    fn parent_value(&mut self) -> i32 {
        if self.has_parent() {
            let current = self.move_up();
            let val = self.value();
            self.move_to(current);
            return val;
        }
        self.original
    }

    /// check if the current node has children
    fn has_children(&self) -> bool {
        // inner structure is a complete tree, just check for the right child
        self.current * 2 + 2 <= self.inner.len()
    }

    /// get current node's right child's value
    fn right_child_value(&mut self) -> i32 {
        let tmp = self.move_right();
        let val = self.value();
        self.move_to(tmp);
        val
    }

    /// set current node's left child's value
    fn set_left_child(&mut self) {
        let parent = self.parent_value();
        let current = self.value();
        let value = ((parent - current).abs() / 2) + current;

        self.move_left();
        self.set_value(value);
        self.move_up();
    }

    /// set current node's right child's value
    fn set_right_child(&mut self) {
        let parent = self.parent_value();
        let current = self.value();
        let value = current - ((parent - current).abs() / 2);

        self.move_right();
        self.set_value(value);
        self.move_up();
    }

    /// iterate over the backing array, filling in each child's value based on the original value
    fn build(&mut self) {
        // ex: original is 400
        // arr[0] == 200
        // arr[1] (left child) == 300
        // arr[2] (right child) == 100
        let root = self.original / 2;

        self.inner[0] = root; // set root node to half of the original value
        self.inner[1] = ((self.original - root).abs() / 2) + root;
        self.inner[2] = root - ((self.original - root).abs() / 2);

        // start with index 1 and fill in each child below that node
        for i in 1..self.inner.len() {
            self.move_to(i);

            if self.has_children() && self.right_child_value() == 0 {
                // this node has an unset child since the rchild is 0
                self.set_left_child();
                self.set_right_child();
            }
        }
        self.move_to(0); // reset current index to the root of the tree
    }
}

/// Makes multiple requests based on the presence of extensions
pub(super) struct Requester {
    /// handles to handlers and config
    handles: Arc<Handles>,

    /// url that will be scanned
    target_url: String,

    /// limits requests per second if present
    rate_limiter: RwLock<Option<LeakyBucket>>,

    /// data regarding policy and metadata about last enforced trigger etc...
    policy_data: PolicyData,

    /// FeroxScan associated with the creation of this Requester
    ferox_scan: Arc<FeroxScan>,

    /// simple lock to control access to tuning to a single thread (per-scan)
    ///
    /// need a usize to determine the number of consecutive non-error calls that a requester has
    /// seen; this will satisfy the non-mut self constraint (due to us being behind an Arc, and
    /// the need for a counter
    tuning_lock: Mutex<usize>,
}

/// Requester implementation
impl Requester {
    /// given a FeroxScanner, create a Requester
    pub fn from(scanner: &FeroxScanner, ferox_scan: Arc<FeroxScan>) -> Result<Self> {
        let limit = scanner.handles.config.rate_limit;

        let rate_limiter = if limit > 0 {
            Some(Self::build_a_bucket(limit)?)
        } else {
            None
        };

        let policy_data = PolicyData::new(
            scanner.handles.config.requester_policy,
            scanner.handles.config.timeout,
        );

        Ok(Self {
            ferox_scan,
            policy_data,
            rate_limiter: RwLock::new(rate_limiter),
            handles: scanner.handles.clone(),
            target_url: scanner.target_url.to_owned(),
            tuning_lock: Mutex::new(0),
        })
    }

    /// build a LeakyBucket, given a rate limit (as requests per second)
    fn build_a_bucket(limit: usize) -> Result<LeakyBucket> {
        let refill = max((limit as f64 / 10.0).round() as usize, 1); // minimum of 1 per second
        let tokens = max((limit as f64 / 2.0).round() as usize, 1);
        let interval = if refill == 1 { 1000 } else { 100 }; // 1 second if refill is 1

        Ok(LeakyBucket::builder()
            .refill_interval(Duration::from_millis(interval)) // add tokens every 0.1s
            .refill_amount(refill) // ex: 100 req/s -> 10 tokens per 0.1s
            .tokens(tokens) // reduce initial burst, 2 is arbitrary, but felt good
            .max(limit)
            .build()?)
    }

    /// sleep and set a flag that can be checked by other threads
    async fn cool_down(&self) {
        if atomic_load!(self.policy_data.cooling_down, Ordering::SeqCst) {
            // prevents a few racy threads making it in here and doubling the wait time erroneously
            return;
        }

        atomic_store!(self.policy_data.cooling_down, true, Ordering::SeqCst);

        sleep(Duration::from_millis(self.policy_data.wait_time)).await;

        atomic_store!(self.policy_data.cooling_down, false, Ordering::SeqCst);
    }

    /// limit the number of requests per second
    pub async fn limit(&self) -> Result<()> {
        self.rate_limiter
            .read()
            .await
            .as_ref()
            .unwrap()
            .acquire_one()
            .await?;
        Ok(())
    }

    /// small function to break out different error checking mechanisms
    fn too_many_errors(&self) -> bool {
        let total = self.ferox_scan.num_errors(PolicyTrigger::Errors);

        // at least 25 errors
        let threshold = max(self.handles.config.threads / 2, 25);

        total >= threshold
    }

    /// small function to break out different error checking mechanisms
    fn too_many_status_errors(&self, trigger: PolicyTrigger) -> bool {
        let total = self.ferox_scan.num_errors(trigger);
        let requests = self.ferox_scan.requests();

        let ratio = total as f64 / requests as f64;

        match trigger {
            PolicyTrigger::Status403 => ratio >= HIGH_ERROR_RATIO,
            PolicyTrigger::Status429 => ratio >= HIGH_ERROR_RATIO / 3.0,
            _ => false,
        }
    }

    /// determine whether or not a policy needs to be enforced
    ///
    /// criteria:
    /// - number of threads (50 default) for general errors (timeouts etc)
    /// - 90% of requests are 403
    /// - 30% of requests are 429
    fn should_enforce_policy(&self) -> Option<PolicyTrigger> {
        if atomic_load!(self.policy_data.cooling_down, Ordering::SeqCst) {
            // prevents a few racy threads making it in here and doubling the wait time erroneously
            return None;
        }

        let requests = atomic_load!(self.handles.stats.data.requests);

        if requests < max(self.handles.config.threads, 50) {
            // check whether at least a full round of threads has made requests or 50 (default # of
            // threads), whichever is higher
            return None;
        }

        if self.too_many_errors() {
            return Some(PolicyTrigger::Errors);
        }

        if self.too_many_status_errors(PolicyTrigger::Status403) {
            return Some(PolicyTrigger::Status403);
        }

        if self.too_many_status_errors(PolicyTrigger::Status429) {
            return Some(PolicyTrigger::Status429);
        }

        None
    }

    /// wrapper for adjust_[up,down] functions, checks error levels to determine adjustment direction
    async fn adjust_limit(&self, trigger: PolicyTrigger, create_limiter: bool) -> Result<()> {
        let scan_errors = self.ferox_scan.num_errors(trigger);
        let policy_errors = atomic_load!(self.policy_data.errors, Ordering::SeqCst);

        if let Ok(mut guard) = self.tuning_lock.try_lock() {
            if scan_errors > policy_errors {
                // errors have increased, need to reduce the requests/sec limit
                *guard = 0; // reset streak counter to 0
                if atomic_load!(self.policy_data.errors) != 0 {
                    self.policy_data.adjust_down();
                }
                self.policy_data.set_errors(scan_errors);
            } else {
                // errors can only be incremented, so an else is sufficient
                *guard += 1;
                self.policy_data.adjust_up(&*guard);
            }
        }

        if atomic_load!(self.policy_data.remove_limit) {
            self.set_rate_limiter(None).await?;
            atomic_store!(self.policy_data.remove_limit, false);
        } else if create_limiter {
            // create_limiter is really just used for unit testing situations, it's true anytime
            // during actual execution
            let new_limit = self.policy_data.get_limit(); // limit is set from within the lock
            self.set_rate_limiter(Some(new_limit)).await?;
        }

        Ok(())
    }

    /// lock the rate limiter and set its value to ta new leaky_bucket
    async fn set_rate_limiter(&self, new_limit: Option<usize>) -> Result<()> {
        let mut guard = self.rate_limiter.write().await;

        let new_bucket = if new_limit.is_none() {
            // got None, need to remove the rate_limiter
            None
        } else if guard.is_some() && guard.as_ref().unwrap().max() == new_limit.unwrap() {
            // new_limit is checked for None in first branch, should be fine to unwrap

            // this function is called more often than i'd prefer due to Send requirements of
            // mutex/rwlock primitives and awaits, this will minimize the cost of the extra calls
            return Ok(());
        } else {
            Some(Self::build_a_bucket(new_limit.unwrap())?)
        };

        let _ = std::mem::replace(&mut *guard, new_bucket);
        Ok(())
    }

    /// enforce auto-tune policy
    async fn tune(&self, trigger: PolicyTrigger) -> Result<()> {
        if atomic_load!(self.policy_data.errors) == 0 {
            // set original number of reqs/second the first time tune is called, skip otherwise
            let reqs_sec = self.ferox_scan.requests_per_second() as usize;
            self.policy_data.set_reqs_sec(reqs_sec);

            let new_limit = self.policy_data.get_limit();
            self.set_rate_limiter(Some(new_limit)).await?;
        }

        self.adjust_limit(trigger, true).await?;
        self.cool_down().await;

        Ok(())
    }

    /// enforce auto-bail policy
    async fn bail(&self, trigger: PolicyTrigger) -> Result<()> {
        if self.ferox_scan.is_active() {
            log::warn!(
                "too many {:?} ({}) triggered {:?} Policy on {}",
                trigger,
                self.ferox_scan.num_errors(trigger),
                self.handles.config.requester_policy,
                self.ferox_scan
            );

            // if allowed to be called within .abort, the inner .await makes it so other
            // in-flight requests don't see the Cancelled status, doing it here ensures a
            // minimum number of requests entering this block
            self.ferox_scan
                .set_status(ScanStatus::Cancelled)
                .unwrap_or_else(|e| log::warn!("Could not set scan status: {}", e));

            // kill the scan
            self.ferox_scan
                .abort()
                .await
                .unwrap_or_else(|e| log::warn!("Could not bail on scan: {}", e));

            // figure out how many requests are skipped as a result
            let pb = self.ferox_scan.progress_bar();
            let num_skipped = pb.length().saturating_sub(pb.position()) as usize;

            // update the overall scan bar by subtracting the number of skipped requests from
            // the total
            self.handles
                .stats
                .send(SubtractFromUsizeField(TotalExpected, num_skipped))
                .unwrap_or_else(|e| log::warn!("Could not update overall scan bar: {}", e));
        }

        Ok(())
    }

    /// Wrapper for make_request
    ///
    /// Attempts recursion when appropriate and sends Responses to the output handler for processing
    pub async fn request(&self, word: &str) -> Result<()> {
        log::trace!("enter: request({})", word);

        let urls =
            FeroxUrl::from_string(&self.target_url, self.handles.clone()).formatted_urls(word)?;

        for url in urls {
            // auto_tune is true, or rate_limit was set (mutually exclusive to user)
            // and a rate_limiter has been created
            // short-circuiting the lock access behind the first boolean check
            let should_tune = self.handles.config.auto_tune || self.handles.config.rate_limit > 0;
            let should_limit = should_tune && self.rate_limiter.read().await.is_some();

            if should_limit {
                // found a rate limiter, limit that junk!
                if let Err(e) = self.limit().await {
                    log::warn!("Could not rate limit scan: {}", e);
                    self.handles.stats.send(AddError(Other)).unwrap_or_default();
                }
            }

            let response = logged_request(&url, self.handles.clone()).await?;

            if (should_tune || self.handles.config.auto_bail)
                && !atomic_load!(self.policy_data.cooling_down, Ordering::SeqCst)
            {
                // only check for policy enforcement when the trigger isn't on cooldown and tuning
                // or bailing is in place (should_tune used here because when auto-tune is on, we'll
                // reach this without a rate_limiter in place)
                match self.policy_data.policy {
                    RequesterPolicy::AutoTune => {
                        if let Some(trigger) = self.should_enforce_policy() {
                            self.tune(trigger).await?;
                        }
                    }
                    RequesterPolicy::AutoBail => {
                        if let Some(trigger) = self.should_enforce_policy() {
                            self.bail(trigger).await?;
                        }
                    }
                    RequesterPolicy::Default => {}
                }
            }

            // response came back without error, convert it to FeroxResponse
            let ferox_response =
                FeroxResponse::from(response, true, self.handles.config.output_level).await;

            // do recursion if appropriate
            if !self.handles.config.no_recursion {
                self.handles
                    .send_scan_command(Command::TryRecursion(Box::new(ferox_response.clone())))?;
                let (tx, rx) = oneshot::channel::<bool>();
                self.handles.send_scan_command(Command::Sync(tx))?;
                rx.await?;
            }

            // purposefully doing recursion before filtering. the thought process is that
            // even though this particular url is filtered, subsequent urls may not
            if self
                .handles
                .filters
                .data
                .should_filter_response(&ferox_response, self.handles.stats.tx.clone())
            {
                continue;
            }

            if self.handles.config.extract_links && !ferox_response.status().is_redirection() {
                let extractor = ExtractorBuilder::default()
                    .target(ResponseBody)
                    .response(&ferox_response)
                    .handles(self.handles.clone())
                    .build()?;

                extractor.extract().await?;
            }

            // everything else should be reported
            if let Err(e) = ferox_response.send_report(self.handles.output.tx.clone()) {
                log::warn!("Could not send FeroxResponse to output handler: {}", e);
            }
        }

        log::trace!("exit: request");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputLevel;
    use crate::scan_manager::ScanStatus;
    use crate::statistics::StatError;
    use crate::{
        config::Configuration,
        event_handlers::{FiltersHandler, ScanHandler, StatsHandler, Tasks, TermOutHandler},
        filters,
    };
    use crate::{
        scan_manager::FeroxScan,
        scan_manager::{ScanOrder, ScanType},
    };
    use reqwest::StatusCode;
    use std::time::Instant;

    /// helper to setup a realistic requester test
    async fn setup_requester_test(config: Option<Arc<Configuration>>) -> (Arc<Handles>, Tasks) {
        // basically C&P from main::wrapped_main, can look there for comments etc if needed
        let configuration = config.unwrap_or_else(|| Arc::new(Configuration::new().unwrap()));

        let (stats_task, stats_handle) = StatsHandler::initialize(configuration.clone());
        let (filters_task, filters_handle) = FiltersHandler::initialize();
        let (out_task, out_handle) =
            TermOutHandler::initialize(configuration.clone(), stats_handle.tx.clone());

        let handles = Arc::new(Handles::new(
            stats_handle,
            filters_handle,
            out_handle,
            configuration.clone(),
        ));

        let (scan_task, scan_handle) = ScanHandler::initialize(handles.clone());

        handles.set_scan_handle(scan_handle);
        filters::initialize(handles.clone()).await.unwrap();

        let tasks = Tasks::new(out_task, stats_task, filters_task, scan_task);

        (handles, tasks)
    }

    /// helper to stay DRY
    async fn increment_errors(handles: Arc<Handles>, scan: Arc<FeroxScan>, num_errors: usize) {
        for _ in 0..num_errors {
            handles
                .stats
                .send(Command::AddError(StatError::Other))
                .unwrap();
            scan.add_error();
        }

        handles.stats.sync().await.unwrap();
    }

    /// helper to stay DRY
    async fn increment_scan_errors(handles: Arc<Handles>, url: &str, num_errors: usize) {
        let scans = handles.ferox_scans().unwrap();

        for _ in 0..num_errors {
            scans.increment_error(format!("{}/", url).as_str());
        }
    }

    /// helper to stay DRY
    async fn increment_scan_status_codes(
        handles: Arc<Handles>,
        url: &str,
        code: StatusCode,
        num_errors: usize,
    ) {
        let scans = handles.ferox_scans().unwrap();
        for _ in 0..num_errors {
            scans.increment_status_code(format!("{}/", url).as_str(), code);
        }
    }

    /// helper to stay DRY
    async fn increment_status_codes(
        handles: Arc<Handles>,
        scan: Arc<FeroxScan>,
        num_codes: usize,
        code: StatusCode,
    ) {
        for _ in 0..num_codes {
            handles.stats.send(Command::AddStatus(code)).unwrap();
            if code == StatusCode::FORBIDDEN {
                scan.add_403();
            } else {
                scan.add_429();
            }
        }

        handles.stats.sync().await.unwrap();
    }

    async fn create_scan(
        handles: Arc<Handles>,
        url: &str,
        num_errors: usize,
        trigger: PolicyTrigger,
    ) -> Arc<FeroxScan> {
        let scan = FeroxScan::new(
            url,
            ScanType::Directory,
            ScanOrder::Initial,
            1000,
            OutputLevel::Default,
            None,
        );

        scan.set_status(ScanStatus::Running).unwrap();
        scan.progress_bar(); // create a new pb

        let scans = handles.ferox_scans().unwrap();
        scans.insert(scan.clone());

        match trigger {
            PolicyTrigger::Status403 => {
                increment_scan_status_codes(
                    handles.clone(),
                    url,
                    StatusCode::FORBIDDEN,
                    num_errors,
                )
                .await;
            }
            PolicyTrigger::Status429 => {
                increment_scan_status_codes(
                    handles.clone(),
                    url,
                    StatusCode::TOO_MANY_REQUESTS,
                    num_errors,
                )
                .await;
            }
            PolicyTrigger::Errors => {
                increment_scan_errors(handles.clone(), url, num_errors).await;
            }
        }

        assert_eq!(scan.num_errors(trigger), num_errors);

        scan
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// should_enforce_policy should return false when # of requests is < threads; also when < 50
    async fn should_enforce_policy_returns_false_on_not_enough_requests_seen() {
        let (handles, _) = setup_requester_test(None).await;

        let requester = Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: Arc::new(FeroxScan::default()),
            target_url: "http://localhost".to_string(),
            rate_limiter: RwLock::new(None),
            policy_data: Default::default(),
        };

        let ferox_scan = Arc::new(FeroxScan::default());

        increment_errors(requester.handles.clone(), ferox_scan.clone(), 49).await;
        // 49 errors is false because we haven't hit the min threshold
        assert_eq!(atomic_load!(requester.handles.stats.data.requests), 49);
        assert_eq!(requester.should_enforce_policy(), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// should_enforce_policy should return true when # of requests is >= 50 and errors >= threads * 2
    async fn should_enforce_policy_returns_true_on_error_times_threads() {
        let mut config = Configuration::new().unwrap_or_default();
        config.threads = 50;

        let (handles, _) = setup_requester_test(Some(Arc::new(config))).await;

        let ferox_scan = Arc::new(FeroxScan::default());

        let requester = Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: ferox_scan.clone(),
            target_url: "http://localhost".to_string(),
            rate_limiter: RwLock::new(None),
            policy_data: Default::default(),
        };

        increment_errors(requester.handles.clone(), ferox_scan.clone(), 25).await;
        assert_eq!(requester.should_enforce_policy(), None);
        increment_errors(requester.handles.clone(), ferox_scan, 25).await;
        assert_eq!(
            requester.should_enforce_policy(),
            Some(PolicyTrigger::Errors)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// should_enforce_policy should return true when # of requests is >= 50 and 403s >= 45 (90%)
    async fn should_enforce_policy_returns_true_on_excessive_403s() {
        let (handles, _) = setup_requester_test(None).await;
        let ferox_scan = Arc::new(FeroxScan::default());

        let requester = Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: ferox_scan.clone(),
            target_url: "http://localhost".to_string(),
            rate_limiter: RwLock::new(None),
            policy_data: Default::default(),
        };

        increment_status_codes(
            requester.handles.clone(),
            ferox_scan.clone(),
            45,
            StatusCode::FORBIDDEN,
        )
        .await;
        assert_eq!(requester.should_enforce_policy(), None);
        increment_status_codes(
            requester.handles.clone(),
            ferox_scan.clone(),
            5,
            StatusCode::OK,
        )
        .await;
        assert_eq!(
            requester.should_enforce_policy(),
            Some(PolicyTrigger::Status403)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// should_enforce_policy should return true when # of requests is >= 50 and errors >= 45 (90%)
    async fn should_enforce_policy_returns_true_on_excessive_429s() {
        let mut config = Configuration::new().unwrap_or_default();
        config.threads = 50;

        let (handles, _) = setup_requester_test(Some(Arc::new(config))).await;
        let ferox_scan = Arc::new(FeroxScan::default());

        let requester = Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: ferox_scan.clone(),
            target_url: "http://localhost".to_string(),
            rate_limiter: RwLock::new(None),
            policy_data: Default::default(),
        };

        increment_status_codes(
            requester.handles.clone(),
            ferox_scan.clone(),
            15,
            StatusCode::TOO_MANY_REQUESTS,
        )
        .await;
        assert_eq!(requester.should_enforce_policy(), None);
        increment_status_codes(
            requester.handles.clone(),
            ferox_scan.clone(),
            35,
            StatusCode::OK,
        )
        .await;
        assert_eq!(
            requester.should_enforce_policy(),
            Some(PolicyTrigger::Status429)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// bail should call abort on the scan with the most errors
    async fn bail_calls_abort_on_highest_errored_feroxscan() {
        let (handles, _) = setup_requester_test(None).await;

        let scan_one = create_scan(handles.clone(), "http://one", 10, PolicyTrigger::Errors).await;
        let scan_two = create_scan(handles.clone(), "http://two", 14, PolicyTrigger::Errors).await;
        let scan_three =
            create_scan(handles.clone(), "http://three", 4, PolicyTrigger::Errors).await;
        let scan_four = create_scan(handles.clone(), "http://four", 7, PolicyTrigger::Errors).await;

        // set up a fake JoinHandle for the scan that's expected to have .abort called on it
        // the reason being if there's no task, the status is never updated, so can't be checked
        let dummy_task =
            tokio::spawn(async move { tokio::time::sleep(Duration::new(15, 0)).await });
        scan_two.set_task(dummy_task).await.unwrap();

        assert!(scan_one.is_active());
        assert!(scan_two.is_active());

        let scans = handles.ferox_scans().unwrap();
        assert_eq!(scans.get_active_scans().len(), 4);

        let req_clone = scan_two.clone();
        let requester = Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: req_clone,
            target_url: "http://one/one/stuff.php".to_string(),
            rate_limiter: RwLock::new(None),
            policy_data: Default::default(),
        };

        requester.bail(PolicyTrigger::Errors).await.unwrap();
        assert_eq!(scans.get_active_scans().len(), 3);
        assert!(scan_one.is_active());
        assert!(scan_three.is_active());
        assert!(scan_four.is_active());
        assert!(!scan_two.is_active());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// bail is ok when no active scans are found
    async fn bail_returns_ok_on_no_active_scans() {
        let (handles, _) = setup_requester_test(None).await;

        let scan_one =
            create_scan(handles.clone(), "http://one", 10, PolicyTrigger::Status403).await;
        let scan_two =
            create_scan(handles.clone(), "http://two", 10, PolicyTrigger::Status429).await;

        scan_one.set_status(ScanStatus::Complete).unwrap();
        scan_two.set_status(ScanStatus::Cancelled).unwrap();

        let scans = handles.ferox_scans().unwrap();
        assert_eq!(scans.get_active_scans().len(), 0);

        let requester = Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: Arc::new(FeroxScan::default()),
            target_url: "http://one/one/stuff.php".to_string(),
            rate_limiter: RwLock::new(None),
            policy_data: Default::default(),
        };

        let result = requester.bail(PolicyTrigger::Status403).await;
        assert!(result.is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// should_enforce should early exit when cooldown flag is set
    async fn should_enforce_policy_returns_none_on_cooldown() {
        let mut config = Configuration::new().unwrap_or_default();
        config.threads = 50;

        let (handles, _) = setup_requester_test(Some(Arc::new(config))).await;

        let requester = Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: Arc::new(FeroxScan::default()),
            target_url: "http://localhost".to_string(),
            rate_limiter: RwLock::new(None),
            policy_data: Default::default(),
        };

        requester
            .policy_data
            .cooling_down
            .store(true, Ordering::Relaxed);

        assert_eq!(requester.should_enforce_policy(), None);
    }

    #[test]
    /// PolicyData builds and sets correct values for the inner heap when set_reqs_sec is called
    fn set_reqs_sec_builds_heap_and_sets_initial_value() {
        let pd = PolicyData::new(RequesterPolicy::AutoBail, 7);
        assert_eq!(pd.wait_time, 3500);
        pd.set_reqs_sec(400);
        assert_eq!(pd.get_limit(), 200);
        assert_eq!(pd.heap.read().unwrap().original, 400);
        assert_eq!(pd.heap.read().unwrap().current, 0);
        assert_eq!(pd.heap.read().unwrap().inner[0], 200);
        assert_eq!(pd.heap.read().unwrap().inner[1], 300);
        assert_eq!(pd.heap.read().unwrap().inner[2], 100);
    }

    #[test]
    /// PolicyData setters/getters tests for code coverage / sanity
    fn policy_data_getters_and_setters() {
        let pd = PolicyData::new(RequesterPolicy::AutoBail, 7);
        pd.set_errors(20);
        assert_eq!(pd.errors.load(Ordering::Relaxed), 20);
        pd.set_limit(200);
        assert_eq!(pd.get_limit(), 200);
    }

    #[test]
    /// PolicyData adjust_down sets the limit to the correct value
    fn policy_data_adjust_down_simple() {
        let pd = PolicyData::new(RequesterPolicy::AutoBail, 7);
        pd.set_reqs_sec(400);
        assert_eq!(pd.get_limit(), 200);
        pd.adjust_down();
        assert_eq!(pd.get_limit(), 100);
    }

    #[test]
    /// PolicyData adjust_down sets the limit to the correct value when no child nodes are present
    fn policy_data_adjust_down_no_children() {
        let pd = PolicyData::new(RequesterPolicy::AutoBail, 7);
        pd.set_reqs_sec(400);
        assert_eq!(pd.get_limit(), 200);
        let mut guard = pd.heap.write().unwrap();
        guard.move_to(250);
        guard.set_value(27);
        pd.set_limit(guard.value() as usize);
        drop(guard);

        pd.adjust_down();
        assert_eq!(pd.get_limit(), 27);
    }

    #[test]
    /// PolicyData adjust_up sets the limit to the correct value
    fn policy_data_adjust_up_simple() {
        let pd = PolicyData::new(RequesterPolicy::AutoBail, 7);
        pd.set_reqs_sec(400);
        assert_eq!(pd.get_limit(), 200);
        pd.adjust_up(&0);
        assert_eq!(pd.get_limit(), 300);
    }

    #[test]
    /// PolicyData adjust_up sets the limit to the correct value
    fn policy_data_adjust_up_with_streak_and_2_moves() {
        // original: 400
        // [200, 300, 100, 350, 250, 150, 50, 375, 325, 275, 225, 175, 125, 75, 25, ...]
        let pd = PolicyData::new(RequesterPolicy::AutoBail, 7);
        pd.set_reqs_sec(400);
        assert_eq!(pd.get_limit(), 200);

        // 2 moves
        pd.heap.write().unwrap().move_to(9);
        assert_eq!(pd.heap.read().unwrap().value(), 275);
        pd.adjust_up(&3);
        assert_eq!(pd.heap.read().unwrap().value(), 300);
        assert_eq!(pd.limit.load(Ordering::Relaxed), 300);
        assert_eq!(pd.remove_limit.load(Ordering::Relaxed), false);
    }

    #[test]
    /// PolicyData adjust_up sets the limit to the correct value
    fn policy_data_adjust_up_with_streak_and_2_moves_to_arrive_at_root() {
        // original: 400
        // [200, 300, 100, 350, 250, 150, 50, 375, 325, 275, 225, 175, 125, 75, 25, ...]
        let pd = PolicyData::new(RequesterPolicy::AutoBail, 7);
        pd.set_reqs_sec(400);
        assert_eq!(pd.get_limit(), 200);

        pd.heap.write().unwrap().move_to(4);
        assert_eq!(pd.heap.read().unwrap().value(), 250);
        pd.adjust_up(&3);
        assert_eq!(pd.heap.read().unwrap().value(), 200);
        assert_eq!(pd.limit.load(Ordering::Relaxed), 200);
        assert_eq!(pd.remove_limit.load(Ordering::Relaxed), true);
    }

    #[test]
    /// PolicyData adjust_up sets the limit to the correct value
    fn policy_data_adjust_up_with_streak_and_2_moves_to_find_less_than_current() {
        // original: 400
        // [200, 300, 100, 350, 250, 150, 50, 375, 325, 275, 225, 175, 125, 75, 25, ...]
        let pd = PolicyData::new(RequesterPolicy::AutoBail, 7);
        pd.set_reqs_sec(400);
        assert_eq!(pd.get_limit(), 200);

        pd.heap.write().unwrap().move_to(15);
        assert_eq!(pd.heap.read().unwrap().value(), 387);
        pd.adjust_up(&3);
        assert_eq!(pd.heap.read().unwrap().value(), 350);
        assert_eq!(pd.limit.load(Ordering::Relaxed), 350);
        assert_eq!(pd.remove_limit.load(Ordering::Relaxed), false);
    }

    #[test]
    /// PolicyData adjust_up sets the limit to the correct value
    fn policy_data_adjust_up_with_streak_and_3_moves() {
        // original: 400
        // [200, 300, 100, 350, 250, 150, 50, 375, 325, 275, 225, 175, 125, 75, 25, ...]
        let pd = PolicyData::new(RequesterPolicy::AutoBail, 7);
        pd.set_reqs_sec(400);
        assert_eq!(pd.get_limit(), 200);

        pd.heap.write().unwrap().move_to(19);
        assert_eq!(pd.heap.read().unwrap().value(), 287);
        pd.adjust_up(&3);
        assert_eq!(pd.heap.read().unwrap().value(), 300);
        assert_eq!(pd.limit.load(Ordering::Relaxed), 300);
        assert_eq!(pd.remove_limit.load(Ordering::Relaxed), false);
    }

    #[test]
    /// PolicyData adjust_up sets the limit to the correct value
    fn policy_data_adjust_up_with_no_children_2_moves() {
        // original: 400
        // [200, 300, 100, 350, 250, 150, 50, 375, 325, 275, 225, 175, 125, 75, 25, ...]
        let pd = PolicyData::new(RequesterPolicy::AutoBail, 7);
        pd.set_reqs_sec(400);
        assert_eq!(pd.get_limit(), 200);

        pd.heap.write().unwrap().move_to(241);

        assert_eq!(pd.heap.read().unwrap().value(), 41);
        pd.adjust_up(&0);
        assert_eq!(pd.heap.read().unwrap().value(), 43);
        assert_eq!(pd.limit.load(Ordering::Relaxed), 43);
        assert_eq!(pd.remove_limit.load(Ordering::Relaxed), false);
    }

    #[test]
    /// PolicyData adjust_up sets the limit to the correct value
    fn policy_data_adjust_up_with_no_children_3_moves() {
        // original: 400
        // [200, 300, 100, 350, 250, 150, 50, 375, 325, 275, 225, 175, 125, 75, 25, ...]
        let pd = PolicyData::new(RequesterPolicy::AutoBail, 7);
        pd.set_reqs_sec(400);
        assert_eq!(pd.get_limit(), 200);

        pd.heap.write().unwrap().move_to(240);

        assert_eq!(pd.heap.read().unwrap().value(), 45);
        pd.adjust_up(&0);
        assert_eq!(pd.heap.read().unwrap().value(), 37);
        assert_eq!(pd.limit.load(Ordering::Relaxed), 37);
        assert_eq!(pd.remove_limit.load(Ordering::Relaxed), false);
    }

    #[test]
    /// hit some of the out of the way corners of limitheap for coverage
    fn increase_limit_heap_coverage_by_hitting_edge_cases() {
        let pd = PolicyData::new(RequesterPolicy::AutoBail, 7);
        pd.set_reqs_sec(400);

        println!("{:?}", pd.heap.read().unwrap()); // debug derivation

        pd.heap.write().unwrap().move_to(240);
        assert_eq!(pd.heap.write().unwrap().move_right(), 240);
        assert_eq!(pd.heap.write().unwrap().move_left(), 240);

        pd.heap.write().unwrap().move_to(0);
        assert_eq!(pd.heap.write().unwrap().move_up(), 0);
        assert_eq!(pd.heap.write().unwrap().parent_value(), 400);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// cooldown should pause execution and prevent others calling it by setting cooling_down flag
    async fn cooldown_pauses_and_sets_flag() {
        let (handles, _) = setup_requester_test(None).await;

        let requester = Arc::new(Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: Arc::new(FeroxScan::default()),
            target_url: "http://localhost".to_string(),
            rate_limiter: RwLock::new(None),
            policy_data: PolicyData::new(RequesterPolicy::AutoBail, 7),
        });

        let start = Instant::now();
        let clone = requester.clone();
        let resp = tokio::task::spawn(async move {
            sleep(Duration::new(1, 0)).await;
            clone.policy_data.cooling_down.load(Ordering::Relaxed)
        });

        requester.cool_down().await;

        assert_eq!(resp.await.unwrap(), true);
        println!("{}", start.elapsed().as_millis());
        assert!(start.elapsed().as_millis() >= 3500);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// adjust_limit should add one to the streak counter when errors from scan equal policy and
    /// increase the scan rate
    async fn adjust_limit_increments_streak_counter_on_upward_movement() {
        let (handles, _) = setup_requester_test(None).await;

        let requester = Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: Arc::new(FeroxScan::default()),
            target_url: "http://localhost".to_string(),
            rate_limiter: RwLock::new(None),
            policy_data: PolicyData::new(RequesterPolicy::AutoBail, 7),
        };

        requester.policy_data.set_reqs_sec(400);
        requester
            .adjust_limit(PolicyTrigger::Errors, true)
            .await
            .unwrap();

        assert_eq!(*requester.tuning_lock.lock().unwrap(), 1);
        assert_eq!(requester.policy_data.get_limit(), 300);
        assert_eq!(
            requester.rate_limiter.read().await.as_ref().unwrap().max(),
            300
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// adjust_limit should reset the streak counter when errors from scan are > policy and
    /// decrease the scan rate
    async fn adjust_limit_resets_streak_counter_on_downward_movement() {
        let (handles, _) = setup_requester_test(None).await;
        let mut buckets = leaky_bucket::LeakyBuckets::new();
        let coordinator = buckets.coordinate().unwrap();
        tokio::spawn(async move { coordinator.await.expect("coordinator errored") });
        let limiter = buckets.rate_limiter().max(200).build().unwrap();

        let scan = FeroxScan::default();
        scan.add_error();
        scan.add_error();

        let requester = Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: Arc::new(scan),
            target_url: "http://localhost".to_string(),
            rate_limiter: RwLock::new(Some(limiter)),
            policy_data: PolicyData::new(RequesterPolicy::AutoBail, 7),
        };

        requester.policy_data.set_reqs_sec(400);
        requester.policy_data.set_errors(1);

        let mut guard = requester.tuning_lock.lock().unwrap();
        *guard = 2;
        drop(guard);

        requester
            .adjust_limit(PolicyTrigger::Errors, false)
            .await
            .unwrap();

        assert_eq!(*requester.tuning_lock.lock().unwrap(), 0);
        assert_eq!(requester.policy_data.get_limit(), 100);
        assert_eq!(requester.policy_data.errors.load(Ordering::Relaxed), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// adjust_limit should remove the rate limiter when remove_limit is set
    async fn adjust_limit_removes_rate_limiter() {
        let (handles, _) = setup_requester_test(None).await;

        let scan = FeroxScan::default();
        scan.add_error();
        scan.add_error();

        let requester = Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: Arc::new(scan),
            target_url: "http://localhost".to_string(),
            rate_limiter: RwLock::new(None),
            policy_data: PolicyData::new(RequesterPolicy::AutoBail, 7),
        };

        requester.policy_data.set_reqs_sec(400);
        requester
            .policy_data
            .remove_limit
            .store(true, Ordering::Relaxed);

        requester
            .adjust_limit(PolicyTrigger::Errors, true)
            .await
            .unwrap();
        assert!(requester.rate_limiter.read().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// errors policytrigger should always be false, 403 is high ratio, and 429 is high ratio / 3
    async fn too_many_status_errors_returns_correct_values() {
        let (handles, _) = setup_requester_test(None).await;

        let mut requester = Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: Arc::new(FeroxScan::default()),
            target_url: "http://localhost".to_string(),
            rate_limiter: RwLock::new(None),
            policy_data: PolicyData::new(RequesterPolicy::AutoBail, 7),
        };

        assert_eq!(
            requester.too_many_status_errors(PolicyTrigger::Errors),
            false
        );

        assert_eq!(
            requester.too_many_status_errors(PolicyTrigger::Status429),
            false
        );
        requester.ferox_scan.progress_bar().set_position(10);
        requester.ferox_scan.add_429();
        requester.ferox_scan.add_429();
        requester.ferox_scan.add_429();
        assert_eq!(
            requester.too_many_status_errors(PolicyTrigger::Status429),
            true
        );

        assert_eq!(
            requester.too_many_status_errors(PolicyTrigger::Status403),
            false
        );
        requester.ferox_scan = Arc::new(FeroxScan::default());
        requester.ferox_scan.progress_bar().set_position(10);
        requester.ferox_scan.add_403();
        requester.ferox_scan.add_403();
        requester.ferox_scan.add_403();
        requester.ferox_scan.add_403();
        requester.ferox_scan.add_403();
        requester.ferox_scan.add_403();
        requester.ferox_scan.add_403();
        requester.ferox_scan.add_403();
        requester.ferox_scan.add_403();
        assert_eq!(
            requester.too_many_status_errors(PolicyTrigger::Status403),
            true
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// set_rate_limiter should exit early when new limit equals the current bucket's max
    async fn set_rate_limiter_early_exit() {
        let (handles, _) = setup_requester_test(None).await;
        let mut buckets = leaky_bucket::LeakyBuckets::new();
        let coordinator = buckets.coordinate().unwrap();
        tokio::spawn(async move { coordinator.await.expect("coordinator errored") });
        let limiter = buckets.rate_limiter().max(200).build().unwrap();

        let requester = Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: Arc::new(FeroxScan::default()),
            target_url: "http://localhost".to_string(),
            rate_limiter: RwLock::new(Some(limiter)),
            policy_data: PolicyData::new(RequesterPolicy::AutoBail, 7),
        };

        requester.set_rate_limiter(Some(200)).await.unwrap();
        assert_eq!(
            requester.rate_limiter.read().await.as_ref().unwrap().max(),
            200
        );
        requester.set_rate_limiter(Some(200)).await.unwrap();
        assert_eq!(
            requester.rate_limiter.read().await.as_ref().unwrap().max(),
            200
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    /// tune should set req/sec and rate_limiter, adjust the limit and cooldown
    async fn tune_sets_expected_values_and_then_waits() {
        let (handles, _) = setup_requester_test(None).await;

        let mut buckets = leaky_bucket::LeakyBuckets::new();
        let coordinator = buckets.coordinate().unwrap();
        tokio::spawn(async move { coordinator.await.expect("coordinator errored") });
        let limiter = buckets.rate_limiter().max(200).build().unwrap();

        let scan = FeroxScan::new(
            "http://localhost",
            ScanType::Directory,
            ScanOrder::Initial,
            1000,
            OutputLevel::Default,
            None,
        );
        scan.set_status(ScanStatus::Running).unwrap();
        scan.add_429();

        let requester = Requester {
            handles,
            tuning_lock: Mutex::new(0),
            ferox_scan: scan.clone(),
            target_url: "http://localhost".to_string(),
            rate_limiter: RwLock::new(Some(limiter)),
            policy_data: PolicyData::new(RequesterPolicy::AutoTune, 4),
        };

        let start = Instant::now();

        let pb = scan.progress_bar();
        pb.set_length(1000);
        pb.set_position(400);
        sleep(Duration::new(1, 0)).await; // used to get req/sec up to 400

        assert_eq!(requester.policy_data.errors.load(Ordering::Relaxed), 0);

        requester.tune(PolicyTrigger::Status429).await.unwrap();

        assert_eq!(requester.policy_data.heap.read().unwrap().original, 400);
        assert_eq!(requester.policy_data.get_limit(), 200);
        assert_eq!(
            requester.rate_limiter.read().await.as_ref().unwrap().max(),
            200
        );

        scan.finish().unwrap();
        assert!(start.elapsed().as_millis() >= 2000);
    }
}
