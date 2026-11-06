// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Licensed Arm FVP configuration, guest bootstrap, and checked VM lifecycle.

#[cfg(target_os = "linux")]
mod backend;
mod cca_init;
#[cfg(target_os = "linux")]
pub mod lifecycle;
#[cfg(target_os = "linux")]
pub mod platform;
#[cfg(target_os = "linux")]
pub mod process;
mod profile;
#[cfg(target_os = "linux")]
pub mod runtime;
#[cfg(target_os = "linux")]
pub mod staging;

#[cfg(target_os = "linux")]
pub use backend::*;
pub use cca_init::CcaHostNetwork;
pub use cca_init::CcaInitConfig;
pub use cca_init::prepare_cca_initrd;
pub use profile::FvpCcaConfig;
pub use profile::FvpConsole;
pub use profile::FvpDeadlines;
pub use profile::FvpPlatform;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Guest path where the FVP's sole shared filesystem is mounted.
pub const GUEST_SHARE_ROOT: &str = "/share";

/// Explicit, immutable inputs for one checked FVP session.
pub struct FvpRunConfig {
    /// Validated FVP platform configuration.
    pub profile: FvpCcaConfig,
    /// Qualified host kernel.
    pub kernel: PathBuf,
    /// Qualified base initrd, left unchanged by guest bootstrap preparation.
    pub initrd: PathBuf,
    /// Guest input tree to snapshot before launch.
    pub share_dir: PathBuf,
    /// Durable host-side results directory.
    pub output_dir: PathBuf,
    /// Optional fresh result file under the output directory.
    pub result_file: Option<PathBuf>,
    /// Pipette location under the guest share.
    pub guest_pipette_path: String,
    /// Guest command and arguments.
    pub guest_command: Vec<String>,
    /// Per-command guest environment.
    pub guest_env: BTreeMap<String, String>,
    /// Guest working directory.
    pub guest_current_dir: Option<String>,
    /// Outer test identity used to merge nested results with its `l1_` logs.
    pub nested_test_parent: Option<String>,
    /// Approved runtime toolchain root.
    pub platform_root: PathBuf,
    /// Verified prebuilt firmware package root.
    pub shrinkwrap_package_root: PathBuf,
}

/// A checked guest command outcome, published only after model cleanup.
pub struct FvpRunOutput {
    /// Exit code, or `128 + signal` for signal termination.
    pub exit_code: Option<i32>,
    /// Total wall time, including teardown.
    pub elapsed: Duration,
}
