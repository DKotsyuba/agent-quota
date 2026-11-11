//! Single integration-test binary: each module is a former `tests/*.rs` crate.
//! One binary links dependencies once; nextest still runs every test in its own process.

#[path = "../assets.rs"]
mod assets;
#[path = "../collectors.rs"]
mod collectors;
#[path = "../install.rs"]
mod install;
#[path = "../owner_launcher.rs"]
mod owner_launcher;
#[path = "../protocol.rs"]
mod protocol;
#[path = "../service.rs"]
mod service;
