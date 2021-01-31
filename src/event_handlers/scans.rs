use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{bail, Result};
use tokio::sync::{mpsc, Semaphore};

use crate::ferox_response::FeroxResponse;
use crate::ferox_url::FeroxUrl;
use crate::{
    scan_manager::{FeroxScan, FeroxScans, ScanOrder},
    scanner::scan_url,
    statistics::StatField::TotalScans,
    CommandReceiver, CommandSender, FeroxChannel, Joiner,
};

use super::command::Command::UpdateUsizeField;
use super::*;

#[derive(Debug)]
/// Container for recursion transmitter and FeroxScans object
pub struct ScanHandle {
    /// FeroxScans object used across modules to track scans
    pub data: Arc<FeroxScans>,

    /// transmitter used to update `data`
    pub tx: CommandSender,
}

/// implementation of RecursionHandle
impl ScanHandle {
    /// Given an Arc-wrapped FeroxScans and CommandSender, create a new RecursionHandle
    pub fn new(data: Arc<FeroxScans>, tx: CommandSender) -> Self {
        Self { data, tx }
    }

    /// Send the given Command over `tx`
    pub fn send(&self, command: Command) -> Result<()> {
        self.tx.send(command)?;
        Ok(())
    }
}

/// event handler for updating a single data structure of all FeroxScans
#[derive(Debug)]
pub struct ScanHandler {
    /// collection of FeroxScans
    data: Arc<FeroxScans>,

    /// handles to other handlers needed to kick off a scan while already past main
    handles: Arc<Handles>,

    /// Receiver half of mpsc from which `Command`s are processed
    receiver: CommandReceiver,

    /// wordlist (re)used for each scan
    wordlist: std::sync::Mutex<Option<Arc<HashSet<String>>>>,

    /// group of scans that need to be joined
    tasks: Vec<Arc<FeroxScan>>,

    /// Maximum recursion depth, a depth of 0 is infinite recursion
    max_depth: usize,

    /// depths associated with the initial targets provided by the user
    depths: Vec<(String, usize)>,

    /// Bounded semaphore used as a barrier to limit concurrent scans
    limiter: Arc<Semaphore>,
}

/// implementation of event handler for filters
impl ScanHandler {
    /// create new event handler
    pub fn new(
        data: Arc<FeroxScans>,
        handles: Arc<Handles>,
        max_depth: usize,
        receiver: CommandReceiver,
    ) -> Self {
        let limit = handles.config.scan_limit;
        let limiter = Semaphore::new(limit);

        if limit == 0 {
            // scan_limit == 0 means no limit should be imposed... however, scoping the Semaphore
            // permit is tricky, so as a workaround, we'll add a ridiculous number of permits to
            // the semaphore (1,152,921,504,606,846,975 to be exact) and call that 'unlimited'

            // note to self: the docs say max is usize::MAX >> 3, however, threads will panic if
            // that value is used (says adding (1) will overflow the semaphore, even though none
            // are being added...)
            limiter.add_permits(usize::MAX >> 4);
        }

        Self {
            data,
            handles,
            receiver,
            max_depth,
            tasks: Vec::new(),
            depths: Vec::new(),
            limiter: Arc::new(limiter),
            wordlist: std::sync::Mutex::new(None),
        }
    }

    /// Set the wordlist
    fn wordlist(&self, wordlist: Arc<HashSet<String>>) {
        if let Ok(mut guard) = self.wordlist.lock() {
            if guard.is_none() {
                let _ = std::mem::replace(&mut *guard, Some(wordlist));
            }
        }
    }

    /// Initialize new `FeroxScans` and the sc side of an mpsc channel that is responsible for
    /// updates to the aforementioned object.
    pub fn initialize(handles: Arc<Handles>) -> (Joiner, ScanHandle) {
        log::trace!("enter: initialize");

        let data = Arc::new(FeroxScans::default());
        let (tx, rx): FeroxChannel<Command> = mpsc::unbounded_channel();

        let max_depth = handles.config.depth;

        let mut handler = Self::new(data.clone(), handles, max_depth, rx);

        let task = tokio::spawn(async move { handler.start().await });

        let event_handle = ScanHandle::new(data, tx);

        log::trace!("exit: initialize -> ({:?}, {:?})", task, event_handle);

        (task, event_handle)
    }

    /// Start a single consumer task (sc side of mpsc)
    ///
    /// The consumer simply receives `Command` and acts accordingly
    pub async fn start(&mut self) -> Result<()> {
        log::trace!("enter: start({:?})", self);

        while let Some(command) = self.receiver.recv().await {
            match command {
                Command::ScanInitialUrls(targets) => {
                    self.ordered_scan_url(targets, ScanOrder::Initial).await?;
                }
                Command::UpdateWordlist(wordlist) => {
                    self.wordlist(wordlist);
                }
                Command::JoinTasks(sender) => {
                    let ferox_scans = self.handles.ferox_scans().unwrap_or_default();
                    let limiter_clone = self.limiter.clone();

                    tokio::spawn(async move {
                        while ferox_scans.has_active_scans() {
                            for scan in ferox_scans.get_active_scans() {
                                log::debug!("FAFAFA joining {:?}", scan);
                                scan.join().await;
                                log::debug!("FAFAFA joined {:?}", scan);
                            }
                        }
                        limiter_clone.close();
                        sender.send(true).expect("oneshot channel failed");
                    });
                }
                Command::TryRecursion(response) => {
                    self.try_recursion(response).await?;
                }
                Command::Sync(sender) => {
                    sender.send(true).unwrap_or_default();
                }
                _ => {} // no other commands needed for RecursionHandler
            }
        }

        log::trace!("exit: start");
        Ok(())
    }

    /// Helper to easily get the (locked) underlying wordlist
    pub fn get_wordlist(&self) -> Result<Arc<HashSet<String>>> {
        if let Ok(guard) = self.wordlist.lock().as_ref() {
            if let Some(list) = guard.as_ref() {
                return Ok(list.clone());
            }
        }

        bail!("Could not get underlying wordlist")
    }

    /// wrapper around scanning a url to stay DRY
    async fn ordered_scan_url(&mut self, targets: Vec<String>, order: ScanOrder) -> Result<()> {
        log::trace!("enter: ordered_scan_url({:?}, {:?})", targets, order);

        for target in targets {
            if self.data.contains(&target) && matches!(order, ScanOrder::Latest) {
                // FeroxScans knows about this url and scan isn't an Initial scan
                // initial scans are skipped because when resuming from a .state file, the scans
                // will already be populated in FeroxScans, so we need to not skip kicking off
                // their scans
                continue;
            }

            let scan = if let Some(ferox_scan) = self.data.get_scan_by_url(&target) {
                ferox_scan // scan already known
            } else {
                self.data.add_directory_scan(&target, order).1 // add the new target; return FeroxScan
            };

            let list = self.get_wordlist()?;

            log::info!("scan handler received {} - beginning scan", target);

            if matches!(order, ScanOrder::Initial) {
                // keeps track of the initial targets' scan depths in order to enforce the
                // maximum recursion depth on any identified sub-directories
                let url = FeroxUrl::from_string(&target, self.handles.clone());
                let depth = url.depth().unwrap_or(0);
                self.depths.push((target.clone(), depth));
            }

            let handles_clone = self.handles.clone();
            let limiter_clone = self.limiter.clone();

            let task = tokio::spawn(async move {
                if let Err(e) = scan_url(&target, order, list, handles_clone, limiter_clone).await {
                    log::warn!("{}", e);
                }
            });

            self.handles.stats.send(UpdateUsizeField(TotalScans, 1))?;

            scan.set_task(task).await?;

            self.tasks.push(scan.clone());
        }

        log::trace!("exit: ordered_scan_url");
        Ok(())
    }

    async fn try_recursion(&mut self, response: Box<FeroxResponse>) -> Result<()> {
        log::trace!("enter: try_recursion({:?})", response,);

        let mut base_depth = 1_usize;

        for (base_url, base_url_depth) in &self.depths {
            if response.url().as_str().starts_with(base_url) {
                base_depth = *base_url_depth;
            }
        }

        if response.reached_max_depth(base_depth, self.max_depth, self.handles.clone()) {
            // at or past recursion depth
            return Ok(());
        }

        if !response.is_directory() {
            // not a directory
            return Ok(());
        }

        let targets = vec![response.url().to_string()];
        self.ordered_scan_url(targets, ScanOrder::Latest).await?;

        log::info!("Added new directory to recursive scan: {}", response.url());

        log::trace!("exit: try_recursion");
        Ok(())
    }
}
