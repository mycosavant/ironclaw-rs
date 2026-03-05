//! stereOS VM sandbox backend.
//!
//! Runs commands in hardened NixOS VMs via QEMU instead of Docker containers.
//! stereOS provides full VM-boundary isolation with:
//! - Kernel-level separation (no shared kernel with host)
//! - Restricted agent shell (`stereos-agent-shell`)
//! - Immutable base image (QEMU `snapshot=on` for copy-on-write overlays)
//! - Fast boot (~3 s with KVM on a warm host; up to 10 s timeout without KVM)
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    StereOsRunner                              │
//! │                                                              │
//! │  execute()                                                   │
//! │    1. Allocate SSH port                                      │
//! │    2. Spawn QEMU with port forwarding                       │
//! │    3. Wait for SSH readiness                                │
//! │    4. Execute command via SSH                               │
//! │    5. Collect output                                         │
//! │    6. Kill QEMU + release port                              │
//! │                                                              │
//! │  create_instance()                                           │
//! │    1. Allocate SSH port                                      │
//! │    2. Spawn QEMU (persistent)                               │
//! │    3. Wait for SSH readiness                                │
//! │    4. Inject env vars via SSH                               │
//! │    5. Start worker binary via SSH                           │
//! │                                                              │
//! │  ┌──────────────┐  ┌──────────────┐  ┌──────────────────┐  │
//! │  │ PortAllocator│  │  SshClient   │  │ QEMU Process     │  │
//! │  └──────────────┘  └──────────────┘  └──────────────────┘  │
//! └─────────────────────────────────────────────────────────────┘
//! ```

pub mod ports;
pub mod runner;
pub mod ssh;

pub use runner::StereOsRunner;
