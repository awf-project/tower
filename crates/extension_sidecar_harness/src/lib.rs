#![forbid(unsafe_code)]

pub mod jsonrpc;

pub use jsonrpc::{
    HostCallIdAllocator, QueuedFrame, frame_from_envelope, host_call, read_host_response,
    send_error, send_response, write_envelope,
};
