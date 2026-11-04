//! Node hardware collection (crate `dotcore-node-hardware`): reads the local host and its
//! accelerators into hardware snapshots for the node, below the site manager and the runtimes.
//!
//! Host facts come from the operating system and per-device facts from per-vendor adapters
//! (NVIDIA through `dotcore-nvml`, AMD through sysfs, the Apple GPU through Metal on macOS). One
//! local capability serializes collection on a dedicated thread, and exact-process GPU memory
//! sampling serves development workload VRAM measurement. Collection is read-only: it never touches
//! workload state, site state or persistent files. Declared machines describe hardware as data
//! instead, and render the same snapshots without the hardware.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Dotcore supports only Linux and macOS targets");

#[cfg(target_os = "macos")]
pub mod apple;
mod cpu;
mod filesystem;
pub mod hardware;
mod host_memory;
mod local;
mod machine;
mod nvidia;
pub mod vram;

pub use local::{LocalCollectionError, LocalNodeCapability};
pub use machine::{MachineDescription, MachineError, MachineWorkload, SimulatedChoice};
