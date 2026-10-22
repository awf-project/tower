//! Per-extension FIFO owner. Commit producers only enqueue; child I/O belongs
//! to the joined worker, outside publication and authoritative state locks.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use extension_protocol::{Event, ExtensionFault, ExtensionManifest};
use serde_json::Value;

use super::catch_up::{CatchUpError, CatchUpFailure};
use super::publication::{
    CommittedEventBatch, CommittedEventPublisher, CommittedFileEvent, SnapshotEntry,
};
use crate::domain::extension_host::ExtensionId;
use crate::domain::extension_host::{ExtensionCallError, ExtensionInstance};

const MAX_PENDING_EVENTS: usize = 65_536;

enum Command {
    Batch(CommittedEventBatch),
    Cut(mpsc::Sender<Result<u64, (u64, CatchUpFailure)>>),
    Activate,
    Call(
        String,
        Value,
        mpsc::Sender<Result<Value, ExtensionCallError>>,
    ),
    Event(Event, mpsc::Sender<Result<(), ExtensionFault>>),
    Shutdown,
}

pub(crate) struct ExtensionDelivery {
    sender: mpsc::Sender<Command>,
    extension_id: ExtensionId,
    cut_started: AtomicBool,
    cancelled: Arc<AtomicBool>,
    queued_events: Arc<AtomicUsize>,
    failure: Arc<Mutex<Option<CatchUpFailure>>>,
}

fn protocol_failure() -> ExtensionFault {
    ExtensionFault::ProtocolError {
        message: "extension delivery ordering failed".to_owned(),
    }
}

impl ExtensionDelivery {
    pub(crate) fn enqueue(&self, batch: CommittedEventBatch) {
        let event_count = batch.events.len();
        if self
            .queued_events
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count
                    .checked_add(event_count)
                    .filter(|count| *count <= MAX_PENDING_EVENTS)
            })
            .is_err()
        {
            *self.failure.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(CatchUpFailure::protocol(
                    self.extension_id.clone(),
                    CatchUpError::Overflow {
                        extension_id: self.extension_id.clone(),
                    },
                ));
            return;
        }
        if self.sender.send(Command::Batch(batch)).is_err() {
            *self.failure.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(CatchUpFailure::protocol(
                    self.extension_id.clone(),
                    CatchUpError::BarrierClosed {
                        extension_id: self.extension_id.clone(),
                    },
                ));
        }
    }

    /// Called under the publication gate: this marker follows every batch
    /// committed through C and precedes every batch after C.
    pub(crate) fn cut(&self) -> mpsc::Receiver<Result<u64, (u64, CatchUpFailure)>> {
        let (sender, receiver) = mpsc::channel();
        let error = if self.cancelled.load(Ordering::Acquire) {
            Some(CatchUpError::Cancelled {
                extension_id: self.extension_id.clone(),
            })
        } else if self.cut_started.swap(true, Ordering::AcqRel) {
            Some(CatchUpError::ClosedEpoch {
                extension_id: self.extension_id.clone(),
            })
        } else {
            None
        };
        if let Some(error) = error {
            let _ = sender.send(Err((
                0,
                CatchUpFailure::protocol(self.extension_id.clone(), error),
            )));
        } else if let Err(error) = self.sender.send(Command::Cut(sender))
            && let Command::Cut(sender) = error.0
        {
            let _ = sender.send(Err((
                0,
                CatchUpFailure::protocol(
                    self.extension_id.clone(),
                    CatchUpError::BarrierClosed {
                        extension_id: self.extension_id.clone(),
                    },
                ),
            )));
        }
        receiver
    }

    pub(crate) fn activate(&self) {
        let _ = self.sender.send(Command::Activate);
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.sender.send(Command::Shutdown);
    }
}

pub(crate) struct OrderedExtension {
    manifest: ExtensionManifest,
    delivery: Arc<ExtensionDelivery>,
    worker: Option<JoinHandle<()>>,
}

impl OrderedExtension {
    pub(crate) fn spawn(
        mut instance: Box<dyn ExtensionInstance>,
        publisher: Arc<CommittedEventPublisher>,
        snapshot: Vec<SnapshotEntry>,
        preparing: bool,
    ) -> std::io::Result<(Box<dyn ExtensionInstance>, Arc<ExtensionDelivery>)> {
        let watermark = publisher
            .capture_cut()
            .map_err(|_| std::io::Error::other("publication is closed"))?
            .sequence;
        let manifest = instance.manifest().clone();
        let extension_id = ExtensionId::new(&manifest.name);
        let subscriptions = manifest.events.subscribe.clone();
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let queued_events = Arc::new(AtomicUsize::new(0));
        let delivery = Arc::new(ExtensionDelivery {
            sender,
            extension_id: extension_id.clone(),
            cut_started: AtomicBool::new(false),
            cancelled: Arc::clone(&cancelled),
            queued_events: Arc::clone(&queued_events),
            failure: Arc::clone(&failure),
        });
        let worker = std::thread::Builder::new()
            .name("extension-delivery".to_owned())
            .spawn(move || {
                // Spawn and subscription installation happen under this gate. Do
                // not run a child callback until the snapshot cut is released.
                drop(publisher.commit_lock());
                let mut last_sequence = watermark;
                let mut processed = 0u64;
                let mut accepted = if subscriptions
                    .iter()
                    .any(|subscription| subscription == "event/fileIndexed")
                {
                    snapshot.len() as u64
                } else {
                    0
                };
                let mut pending = VecDeque::new();
                let mut active = !preparing;
                for entry in snapshot {
                    if cancelled.load(Ordering::Acquire) {
                        *failure.lock().unwrap_or_else(|p| p.into_inner()) =
                            Some(CatchUpFailure::protocol(
                                extension_id.clone(),
                                CatchUpError::Cancelled {
                                    extension_id: extension_id.clone(),
                                },
                            ));
                        break;
                    }
                    deliver(
                        &mut *instance,
                        &subscriptions,
                        Event::FileIndexed {
                            file_id: entry.file_id.index() as u64,
                            path: entry.path.as_str().to_owned(),
                        },
                        &failure,
                        &mut processed,
                    );
                }
                while let Ok(command) = receiver.recv() {
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    // Producers enqueue before releasing their commit guards.
                    drop(publisher.commit_lock());
                    match command {
                        Command::Batch(batch) => {
                            queued_events.fetch_sub(batch.events.len(), Ordering::AcqRel);
                            if last_sequence.checked_add(1) != Some(batch.sequence) {
                                *failure.lock().unwrap_or_else(|p| p.into_inner()) =
                                    Some(CatchUpFailure::protocol(
                                        extension_id.clone(),
                                        CatchUpError::InvalidSequence,
                                    ));
                            }
                            last_sequence = batch.sequence;
                            for event in batch.events {
                                if cancelled.load(Ordering::Acquire) {
                                    *failure.lock().unwrap_or_else(|p| p.into_inner()) =
                                        Some(CatchUpFailure::protocol(
                                            extension_id.clone(),
                                            CatchUpError::Cancelled {
                                                extension_id: extension_id.clone(),
                                            },
                                        ));
                                    break;
                                }
                                if active {
                                    deliver(
                                        &mut *instance,
                                        &subscriptions,
                                        wire_event(event),
                                        &failure,
                                        &mut processed,
                                    );
                                } else if pending.len() < MAX_PENDING_EVENTS {
                                    pending.push_back(event);
                                } else {
                                    *failure.lock().unwrap_or_else(|p| p.into_inner()) =
                                        Some(CatchUpFailure::protocol(
                                            extension_id.clone(),
                                            CatchUpError::Overflow {
                                                extension_id: extension_id.clone(),
                                            },
                                        ));
                                }
                            }
                        }
                        Command::Cut(response) => {
                            let additional = pending
                                .iter()
                                .filter(|event| {
                                    let method = match event {
                                        CommittedFileEvent::Indexed { .. } => "event/fileIndexed",
                                        CommittedFileEvent::Changed { .. } => "event/fileChanged",
                                        CommittedFileEvent::Deleted { .. } => "event/fileDeleted",
                                    };
                                    subscriptions
                                        .iter()
                                        .any(|subscription| subscription == method)
                                })
                                .count() as u64;
                            if let Some(total) = accepted.checked_add(additional) {
                                accepted = total;
                            } else {
                                *failure.lock().unwrap_or_else(|p| p.into_inner()) =
                                    Some(CatchUpFailure::protocol(
                                        extension_id.clone(),
                                        CatchUpError::Overflow {
                                            extension_id: extension_id.clone(),
                                        },
                                    ));
                            }
                            drain(
                                &mut *instance,
                                &subscriptions,
                                &mut pending,
                                &failure,
                                &mut processed,
                                &cancelled,
                            );
                            let result = failure
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .clone()
                                .map_or(Ok(accepted), |fault| Err((accepted, fault)));
                            let _ = response.send(result);
                        }
                        Command::Activate => {
                            drain(
                                &mut *instance,
                                &subscriptions,
                                &mut pending,
                                &failure,
                                &mut processed,
                                &cancelled,
                            );
                            active = true;
                        }
                        Command::Call(name, params, response) => {
                            let failed = failure.lock().unwrap_or_else(|p| p.into_inner()).clone();
                            let result = match failed {
                                Some(failure) => {
                                    Err(ExtensionCallError::Fault(failure.wire_fault()))
                                }
                                None => instance.call_tool(&name, params),
                            };
                            let _ = response.send(result);
                        }
                        Command::Event(event, response) => {
                            let _ = response.send(instance.deliver_event(event));
                        }
                        Command::Shutdown => break,
                    }
                }
                instance.shutdown();
            })?;
        Ok((
            Box::new(Self {
                manifest,
                delivery: Arc::clone(&delivery),
                worker: Some(worker),
            }),
            delivery,
        ))
    }
}

fn wire_event(event: CommittedFileEvent) -> Event {
    match event {
        CommittedFileEvent::Indexed { file_id, path } => Event::FileIndexed {
            file_id: file_id.index() as u64,
            path: path.as_str().to_owned(),
        },
        CommittedFileEvent::Changed { file_id, path } => Event::FileChanged {
            file_id: file_id.index() as u64,
            path: path.as_str().to_owned(),
        },
        CommittedFileEvent::Deleted { path } => Event::FileDeleted {
            path: path.as_str().to_owned(),
        },
    }
}

fn deliver(
    instance: &mut dyn ExtensionInstance,
    subscriptions: &[String],
    event: Event,
    failure: &Mutex<Option<CatchUpFailure>>,
    processed: &mut u64,
) {
    let method = match &event {
        Event::FileIndexed { .. } => "event/fileIndexed",
        Event::FileChanged { .. } => "event/fileChanged",
        Event::FileDeleted { .. } => "event/fileDeleted",
    };
    if !subscriptions
        .iter()
        .any(|subscription| subscription == method)
    {
        return;
    }
    if failure.lock().unwrap_or_else(|p| p.into_inner()).is_some() {
        return;
    }
    match instance.deliver_event(event) {
        Ok(()) => match processed.checked_add(1) {
            Some(count) => *processed = count,
            None => {
                let extension_id = ExtensionId::new(&instance.manifest().name);
                *failure.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(CatchUpFailure::protocol(
                        extension_id.clone(),
                        CatchUpError::Overflow { extension_id },
                    ));
            }
        },
        Err(fault) => {
            *failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(CatchUpFailure::delivery(
                ExtensionId::new(&instance.manifest().name),
                &fault,
            ))
        }
    }
}

fn drain(
    instance: &mut dyn ExtensionInstance,
    subscriptions: &[String],
    pending: &mut VecDeque<CommittedFileEvent>,
    failure: &Mutex<Option<CatchUpFailure>>,
    processed: &mut u64,
    cancelled: &AtomicBool,
) {
    while let Some(event) = pending.pop_front() {
        if cancelled.load(Ordering::Acquire) {
            let extension_id = ExtensionId::new(&instance.manifest().name);
            *failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(CatchUpFailure::protocol(
                extension_id.clone(),
                CatchUpError::Cancelled { extension_id },
            ));
            break;
        }
        deliver(
            instance,
            subscriptions,
            wire_event(event),
            failure,
            processed,
        );
    }
}

impl ExtensionInstance for OrderedExtension {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }
    fn call_tool(&mut self, name: &str, params: Value) -> Result<Value, ExtensionCallError> {
        let (sender, receiver) = mpsc::channel();
        self.delivery
            .sender
            .send(Command::Call(name.to_owned(), params, sender))
            .map_err(|_| ExtensionCallError::Fault(protocol_failure()))?;
        receiver
            .recv()
            .unwrap_or_else(|_| Err(ExtensionCallError::Fault(protocol_failure())))
    }
    fn deliver_event(&mut self, event: Event) -> Result<(), ExtensionFault> {
        let (sender, receiver) = mpsc::channel();
        self.delivery
            .sender
            .send(Command::Event(event, sender))
            .map_err(|_| protocol_failure())?;
        receiver.recv().unwrap_or_else(|_| Err(protocol_failure()))
    }
    fn shutdown(&mut self) {
        self.delivery.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for OrderedExtension {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::adapters::mcp::native_tools::{EngineState, NativeToolRegistry};
    use crate::adapters::mcp::registry::ToolRegistry;
    use crate::adapters::{InMemoryFs, InMemoryStorage};
    use crate::domain::index::InvertedIndex;
    use crate::domain::{ExtensionId, FileId, ProjectWorkspace, RelativePath};
    use serde_json::json;
    use std::sync::RwLock;

    pub(crate) struct Recorder {
        manifest: ExtensionManifest,
        events: Vec<Event>,
        released: bool,
        fail_delivery: bool,
    }
    impl Recorder {
        pub(crate) fn new(fail_delivery: bool) -> Self {
            Self {
                manifest: ExtensionManifest {
                    name: "recorder".into(),
                    version: "0.1.0".into(),
                    command: vec!["unused".into()],
                    activation: extension_protocol::Activation::Eager,
                    tools: Vec::new(),
                    events: extension_protocol::manifest::EventsSection {
                        subscribe: vec![
                            "event/fileIndexed".into(),
                            "event/fileChanged".into(),
                            "event/fileDeleted".into(),
                        ],
                    },
                    capabilities: extension_protocol::manifest::CapabilitiesSection::default(),
                },
                events: Vec::new(),
                released: false,
                fail_delivery,
            }
        }
    }
    impl ExtensionInstance for Recorder {
        fn manifest(&self) -> &ExtensionManifest {
            &self.manifest
        }
        fn call_tool(&mut self, name: &str, _: Value) -> Result<Value, ExtensionCallError> {
            match name {
                "release" => {
                    self.released = true;
                    Ok(json!({"released":true}))
                }
                "fence" => Ok(json!({"ready": self.released, "count": self.events.len()})),
                _ => Ok(serde_json::to_value(&self.events).unwrap()),
            }
        }
        fn deliver_event(&mut self, event: Event) -> Result<(), ExtensionFault> {
            if self.fail_delivery {
                return Err(ExtensionFault::Crashed { code: Some(17) });
            }
            self.events.push(event);
            Ok(())
        }
        fn shutdown(&mut self) {}
    }
    fn changed(path: &str) -> CommittedFileEvent {
        CommittedFileEvent::Changed {
            file_id: FileId::new_for_testing(0, 0),
            path: RelativePath::new(path),
        }
    }

    #[test]
    fn actual_delivery_reports_typed_sequence_closed_cancel_and_duplicate_errors() {
        let publisher = Arc::new(CommittedEventPublisher::new(1));
        let id = ExtensionId::new("recorder");
        assert!(matches!(
            publisher.require_delivery(&id),
            Err(CatchUpError::UnknownExtension { .. })
        ));
        let (mut extension, delivery) = OrderedExtension::spawn(
            Box::new(Recorder::new(false)),
            Arc::clone(&publisher),
            vec![],
            true,
        )
        .unwrap();
        publisher
            .subscribe(id.clone(), Arc::clone(&delivery))
            .unwrap();
        assert!(matches!(
            publisher.subscribe(id.clone(), Arc::clone(&delivery)),
            Err(CatchUpError::DuplicateEpoch { .. })
        ));
        delivery.enqueue(CommittedEventBatch {
            sequence: 9,
            events: vec![changed("out-of-order.rs")],
        });
        let (_, failure) = delivery
            .cut()
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert_eq!(failure.error, CatchUpError::InvalidSequence);
        assert_eq!(failure.extension_id, id);
        let (_, closed) = delivery
            .cut()
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert!(matches!(closed.error, CatchUpError::ClosedEpoch { .. }));
        delivery.cancel();
        let (_, cancelled) = delivery.cut().recv().unwrap().unwrap_err();
        assert!(matches!(cancelled.error, CatchUpError::Cancelled { .. }));
        assert_eq!(
            cancelled.failure_class,
            crate::adapters::daemon::StartupFailureClass::Cancelled
        );
        extension.shutdown();
        publisher.cancel_deliveries();
        assert!(matches!(
            publisher.subscribe(id, delivery),
            Err(CatchUpError::BarrierClosed { .. })
        ));
    }

    #[test]
    fn actual_delivery_queue_overflow_fails_only_its_epoch() {
        let publisher = Arc::new(CommittedEventPublisher::new(1));
        let gate = publisher.commit_lock();
        let (mut extension, delivery) = OrderedExtension::spawn(
            Box::new(Recorder::new(false)),
            Arc::clone(&publisher),
            vec![],
            true,
        )
        .unwrap();
        delivery.enqueue(CommittedEventBatch {
            sequence: 1,
            events: vec![changed("oversized.rs"); MAX_PENDING_EVENTS + 1],
        });
        let cut = delivery.cut();
        drop(gate);
        let (_, failure) = cut
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert!(matches!(failure.error, CatchUpError::Overflow { .. }));
        assert!(
            publisher.reserve_batch(1).is_ok(),
            "one failed subscriber does not close publication"
        );
        extension.shutdown();
    }

    #[test]
    fn real_delivery_owner_preserves_snapshot_cut_pending_controls_and_live_query_order() {
        let publisher = Arc::new(CommittedEventPublisher::new(7));
        let commit = publisher.commit_lock();
        let (mut extension, delivery) = OrderedExtension::spawn(
            Box::new(Recorder::new(false)),
            Arc::clone(&publisher),
            vec![SnapshotEntry {
                file_id: FileId::new_for_testing(0, 0),
                path: RelativePath::new("snapshot.rs"),
            }],
            true,
        )
        .unwrap();
        publisher
            .subscribe(ExtensionId::new("recorder"), Arc::clone(&delivery))
            .unwrap();
        publisher
            .commit_events(vec![changed("before-cut.rs")])
            .unwrap();
        let cut = delivery.cut();
        publisher
            .commit_events(vec![changed("after-cut.rs")])
            .unwrap();
        drop(commit);
        assert_eq!(
            cut.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
            Ok(2)
        );
        assert_eq!(
            extension.call_tool("fence", json!({})).unwrap(),
            json!({"ready":false,"count":2})
        );
        assert_eq!(
            extension.call_tool("release", json!({})).unwrap(),
            json!({"released":true})
        );
        assert_eq!(
            extension.call_tool("fence", json!({})).unwrap(),
            json!({"ready":true,"count":2})
        );
        {
            let _commit = publisher.commit_lock();
            delivery.activate();
            publisher.commit_events(vec![changed("live.rs")]).unwrap();
        }
        let events = extension.call_tool("query", json!({})).unwrap();
        let paths: Vec<_> = events
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["path"].as_str().unwrap())
            .collect();
        assert_eq!(
            paths,
            ["snapshot.rs", "before-cut.rs", "after-cut.rs", "live.rs"]
        );
        extension.shutdown();
    }

    #[test]
    fn actual_native_commits_publish_once_and_cas_failure_publishes_nothing() {
        let publisher = Arc::new(CommittedEventPublisher::new(8));
        let commit = publisher.commit_lock();
        let (mut extension, delivery) = OrderedExtension::spawn(
            Box::new(Recorder::new(false)),
            Arc::clone(&publisher),
            Vec::new(),
            true,
        )
        .unwrap();
        publisher
            .subscribe(ExtensionId::new("recorder"), Arc::clone(&delivery))
            .unwrap();
        drop(commit);
        let mut state = EngineState::new(
            ProjectWorkspace::new(),
            InvertedIndex::new(),
            Box::new(InMemoryStorage::new()),
            Box::new(InMemoryFs::new()),
        );
        state.publication = Some(Arc::clone(&publisher));
        let mut registry = NativeToolRegistry::new(Arc::new(RwLock::new(state)));
        registry
            .call(
                "tower_create_file",
                json!({"path":"mcp.rs","content":"first"}),
            )
            .unwrap();
        assert!(
            registry
                .call(
                    "tower_create_file",
                    json!({"path":"mcp.rs","content":"rejected","expected_version":"00".repeat(32)})
                )
                .is_err()
        );
        registry
            .call(
                "tower_edit_range",
                json!({"path":"mcp.rs","start_byte":0,"end_byte":5,"replacement":"second"}),
            )
            .unwrap();
        registry
            .call("tower_delete_file", json!({"path":"mcp.rs"}))
            .unwrap();
        let target = {
            let _commit = publisher.commit_lock();
            delivery.cut()
        };
        assert_eq!(
            target
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            Ok(3)
        );
        let events = extension.call_tool("query", json!({})).unwrap();
        assert_eq!(events.as_array().unwrap().len(), 3);
        assert_eq!(events[2], json!({"type":"FileDeleted","path":"mcp.rs"}));
        extension.shutdown();
    }

    #[test]
    fn completed_live_batches_release_queue_capacity() {
        let publisher = Arc::new(CommittedEventPublisher::new(10));
        let (mut extension, _delivery) = {
            let _commit = publisher.commit_lock();
            let (extension, delivery) = OrderedExtension::spawn(
                Box::new(Recorder::new(false)),
                Arc::clone(&publisher),
                Vec::new(),
                false,
            )
            .unwrap();
            publisher
                .subscribe(ExtensionId::new("recorder"), Arc::clone(&delivery))
                .unwrap();
            (extension, delivery)
        };
        {
            let _commit = publisher.commit_lock();
            publisher
                .commit_events(vec![changed("first.rs"); MAX_PENDING_EVENTS])
                .unwrap();
        }
        assert_eq!(
            extension.call_tool("fence", json!({})).unwrap()["count"],
            MAX_PENDING_EVENTS
        );
        {
            let _commit = publisher.commit_lock();
            publisher.commit_events(vec![changed("last.rs")]).unwrap();
        }
        assert_eq!(
            extension.call_tool("fence", json!({})).unwrap()["count"],
            MAX_PENDING_EVENTS + 1
        );
        extension.shutdown();
    }

    #[test]
    fn cancellation_joins_bounded_backend_work_and_discards_remaining_replay() {
        struct Delayed {
            recorder: Recorder,
            entered: mpsc::Sender<()>,
            backend_deadline: mpsc::Receiver<()>,
            calls: Arc<AtomicUsize>,
            stopped: Arc<AtomicBool>,
        }
        impl ExtensionInstance for Delayed {
            fn manifest(&self) -> &ExtensionManifest {
                self.recorder.manifest()
            }
            fn call_tool(
                &mut self,
                name: &str,
                params: Value,
            ) -> Result<Value, ExtensionCallError> {
                self.recorder.call_tool(name, params)
            }
            fn deliver_event(&mut self, _: Event) -> Result<(), ExtensionFault> {
                self.calls.fetch_add(1, Ordering::AcqRel);
                let _ = self.entered.send(());
                // Model a backend that cooperates only at its operation timeout.
                let _ = self
                    .backend_deadline
                    .recv_timeout(std::time::Duration::from_millis(100));
                Ok(())
            }
            fn shutdown(&mut self) {
                self.stopped.store(true, Ordering::Release);
            }
        }
        for live in [false, true] {
            let publisher = Arc::new(CommittedEventPublisher::new(11));
            let (entered_tx, entered_rx) = mpsc::channel();
            let (_backend_tx, backend_rx) = mpsc::channel();
            let calls = Arc::new(AtomicUsize::new(0));
            let stopped = Arc::new(AtomicBool::new(false));
            let (mut extension, delivery) = {
                let _commit = publisher.commit_lock();
                OrderedExtension::spawn(
                    Box::new(Delayed {
                        recorder: Recorder::new(false),
                        entered: entered_tx,
                        backend_deadline: backend_rx,
                        calls: Arc::clone(&calls),
                        stopped: Arc::clone(&stopped),
                    }),
                    Arc::clone(&publisher),
                    if live {
                        vec![]
                    } else {
                        vec![
                            SnapshotEntry {
                                file_id: FileId::new_for_testing(0, 0),
                                path: RelativePath::new("pending.rs")
                            };
                            3
                        ]
                    },
                    !live,
                )
                .unwrap()
            };
            if live {
                delivery.enqueue(CommittedEventBatch {
                    sequence: 1,
                    events: vec![changed("live.rs"); 3],
                });
            }
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            let started = std::time::Instant::now();
            delivery.cancel();
            extension.shutdown();
            assert!(started.elapsed() < std::time::Duration::from_secs(2));
            assert!(
                stopped.load(Ordering::Acquire),
                "shutdown joins the owner before returning"
            );
            assert_eq!(
                calls.load(Ordering::Acquire),
                1,
                "cancellation prevents later replay effects"
            );
        }
    }

    #[test]
    fn delivery_faults_fail_the_epoch_without_claiming_accepted_count() {
        let publisher = Arc::new(CommittedEventPublisher::new(9));
        let commit = publisher.commit_lock();
        let (mut extension, delivery) = OrderedExtension::spawn(
            Box::new(Recorder::new(true)),
            Arc::clone(&publisher),
            Vec::new(),
            true,
        )
        .unwrap();
        publisher
            .subscribe(ExtensionId::new("recorder"), Arc::clone(&delivery))
            .unwrap();
        publisher.commit_events(vec![changed("failed.rs")]).unwrap();
        let target = delivery.cut();
        drop(commit);
        assert_eq!(
            target
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            Err((
                1,
                CatchUpFailure::delivery(
                    ExtensionId::new("recorder"),
                    &ExtensionFault::Crashed { code: Some(17) }
                )
            ))
        );
        extension.shutdown();
    }
}
