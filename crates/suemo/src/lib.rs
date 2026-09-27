//! suemo core: domain model, engine wrapper, IPC protocol, config, daemon,
//! and CLI verbs. No GPUI here — the front-end lives in `suemo-gpui`,
//! which is the only crate allowed to depend on both this lib and gpui
//! (cargo forbids suemo(bin) → suemo-gpui → suemo(lib) cycles).

pub mod cli;
pub mod config;
pub mod daemon;
pub mod domain;
pub mod engine;
pub mod http;
pub mod ipc;
pub mod sync;
