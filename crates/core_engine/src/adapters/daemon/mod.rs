//! Shared-daemon hub: one daemon per workspace owns the engine; `tower mcp`
//! clients relay stdio over a Unix socket. Pure infrastructure (sockets,
//! processes, framing) — the `domain/` crate is never imported here.
#![forbid(unsafe_code)]

pub mod catch_up;
pub mod client;
pub(crate) mod delivery;
pub mod engine;
pub mod instrumentation;
pub mod lifecycle;
pub mod publication;
pub mod server;
pub mod session;
pub mod socket;
pub mod wire;

pub use catch_up::{
    CatchUpEpoch, CatchUpError, ExtensionCatchUpCoordinator, ExtensionDependencyPolicy,
};
pub use lifecycle::{
    CapabilityDependency, CapabilityReadiness, DaemonLifecycle, DispatchPermit, ExtensionReadiness,
    LifecycleTransitionError, ReadinessController, ReadinessSnapshot, ReadinessWaitError,
    StartupFailureClass, StartupPhase,
};
pub use publication::{
    CommittedEventBatch, CommittedEventPublisher, CommittedFileEvent, PublicationCut,
    PublicationError, SnapshotEntry,
};
