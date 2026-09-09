// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared CCA host startup for the official test initrd.

use anyhow::Context;
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub(crate) const CCA_INIT_SCRIPT_NAME: &str = "cca-init.sh";

/// Networking provided by the outer CCA platform.
#[derive(Clone, Debug)]
pub enum CcaHostNetwork {
    /// QEMU user networking at 10.0.2.0/24.
    QemuStatic,
    /// Userspace DHCP on eth0, including an external bound on client hooks.
    Dhcp {
        /// Guest monotonic bound for obtaining and configuring a lease.
        /// The outer launcher must also bound host elapsed time, since an
        /// emulator can stop advancing the guest clock.
        timeout: Duration,
    },
}

/// Platform-specific inputs to the shared CCA host init script.
#[derive(Clone, Debug)]
pub struct CcaInitConfig {
    /// Virtio-9P tag exported by the outer platform.
    pub mount_tag: String,
    /// Mutually exclusive static or DHCP configuration.
    pub network: CcaHostNetwork,
}

impl CcaInitConfig {
    /// Existing QEMU CCA startup contract.
    pub fn qemu() -> Self {
        Self {
            mount_tag: "host".into(),
            network: CcaHostNetwork::QemuStatic,
        }
    }

    /// FVP startup contract with a 30-second DHCP deadline.
    pub fn fvp() -> Self {
        Self {
            mount_tag: "FM".into(),
            network: CcaHostNetwork::Dhcp {
                timeout: Duration::from_secs(30),
            },
        }
    }
}

/// Inject startup and host CA certificates into an ephemeral base-initrd copy.
pub fn prepare_cca_initrd(
    base_initrd: &Path,
    scratch_dir: &Path,
    guest_pipette_path: &str,
    config: &CcaInitConfig,
) -> anyhow::Result<tempfile::TempPath> {
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("host clock is before the Unix epoch")?
        .as_secs();
    crate::qemu::prepare_initrd_with_script(
        base_initrd,
        scratch_dir,
        CCA_INIT_SCRIPT_NAME,
        build_init_script(guest_pipette_path, config, epoch)?,
    )
}

fn network_script(network: &CcaHostNetwork) -> anyhow::Result<String> {
    match network {
        CcaHostNetwork::QemuStatic => Ok("ip addr replace 10.0.2.15/24 dev eth0\n\
             ip route replace default via 10.0.2.2\n\
             echo 'nameserver 10.0.2.3' > /etc/resolv.conf\n"
            .into()),
        CcaHostNetwork::Dhcp { timeout } => {
            anyhow::ensure!(
                (Duration::from_secs(10)..=Duration::from_secs(120)).contains(timeout)
                    && timeout.subsec_nanos() == 0,
                "CCA DHCP timeout must be a whole number of seconds in 10..=120"
            );
            Ok(dhcp_script(timeout.as_secs()))
        }
    }
}

fn dhcp_script(seconds: u64) -> String {
    format!(
        "\
        dhcp_dir=$(mktemp -d /tmp/incubator-dhcp.XXXXXX)\n\
        dhcp_log=\"$log_dir/incubator-dhcp.log\"\n\
        : > \"$dhcp_dir/client.log\"\n\
        read dhcp_started dhcp_unused < /proc/uptime\n\
        dhcp_deadline=$((${{dhcp_started%.*}} * 100 + 1${{dhcp_started#*.}} - 100 + {seconds} * 100))\n\
        setsid sh -c '\n\
            set +e\n\
            udhcpc -f -q -n -t 5 -T 3 -i eth0 > \"$1/client.log\" 2>&1\n\
            status=$?\n\
            if [ \"$status\" -eq 0 ]; then\n\
                address=$(ip -4 -o address show dev eth0 scope global) || status=1\n\
                route=$(ip -4 route show default dev eth0) || status=1\n\
                if [ -z \"$address\" ] || [ -z \"$route\" ]; then\n\
                    echo \"CCA DHCP did not configure an address and default route\" >> \"$1/client.log\"\n\
                    status=1\n\
                fi\n\
                if ! grep -q \"^nameserver \" /etc/resolv.conf; then\n\
                    echo \"CCA DHCP supplied no DNS server; name resolution may be unavailable\" >> \"$1/client.log\"\n\
                fi\n\
            fi\n\
            read completed unused < /proc/uptime\n\
            completed=$((${{completed%.*}} * 100 + 1${{completed#*.}} - 100))\n\
            echo \"$status $completed\" > \"$1/result.new\"\n\
            mv \"$1/result.new\" \"$1/result\"\n\
            while :; do sleep 1; done\n\
        ' sh \"$dhcp_dir\" > \"$dhcp_dir/worker.log\" 2>&1 &\n\
        dhcp_pid=$!\n\
        dhcp_status=125\n\
        while kill -0 \"$dhcp_pid\" 2>/dev/null; do\n\
            {poll}\
            sleep 1\n\
        done\n\
        kill -KILL \"$dhcp_pid\" 2>/dev/null || :\n\
        kill -KILL \"-$dhcp_pid\" 2>/dev/null || :\n\
        wait \"$dhcp_pid\" 2>/dev/null || :\n\
        dhcp_pid=\n\
        if [ \"$dhcp_status\" -eq 124 ]; then\n\
            echo 'CCA DHCP deadline exceeded' >> \"$dhcp_dir/client.log\"\n\
        elif [ \"$dhcp_status\" -eq 125 ]; then\n\
            echo 'CCA DHCP supervisor process exited unexpectedly' >> \"$dhcp_dir/client.log\"\n\
        fi\n\
        cat \"$dhcp_dir/client.log\" \"$dhcp_dir/worker.log\" > \"$dhcp_log\"\n\
        if ! rm -r \"$dhcp_dir\"; then echo \"Could not remove CCA DHCP scratch directory: $dhcp_dir\" >&2; fi\n\
        dhcp_dir=\n\
        if [ \"$dhcp_status\" -ne 0 ]; then\n\
            echo \"CCA DHCP failed with status $dhcp_status\" >&2\n\
            cat \"$dhcp_log\" >&2\n\
            exit \"$dhcp_status\"\n\
        fi\n",
        poll = dhcp_poll(),
    )
}

fn dhcp_poll() -> String {
    format!(
        "\
        read dhcp_now dhcp_unused < /proc/uptime\n\
        dhcp_now=$((${{dhcp_now%.*}} * 100 + 1${{dhcp_now#*.}} - 100))\n\
        {}\
        if [ \"$dhcp_now\" -ge \"$dhcp_deadline\" ]; then\n\
            dhcp_status=124\n\
            break\n\
        fi\n",
        dhcp_completion_check()
    )
}

fn dhcp_completion_check() -> &'static str {
    "\
    if [ -f \"$dhcp_dir/result\" ]; then\n\
        read dhcp_status dhcp_completed < \"$dhcp_dir/result\"\n\
        if [ \"$dhcp_completed\" -gt \"$dhcp_deadline\" ]; then dhcp_status=124; fi\n\
        break\n\
    fi\n"
}

fn build_init_script(
    guest_pipette_path: &str,
    config: &CcaInitConfig,
    epoch: u64,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        !config.mount_tag.is_empty() && !config.mount_tag.contains('\0'),
        "CCA 9P mount tag must be nonempty and contain no NUL"
    );
    anyhow::ensure!(
        guest_pipette_path.starts_with("/share/")
            && !guest_pipette_path.contains('\0')
            && !Path::new(guest_pipette_path)
                .components()
                .any(|component| component == std::path::Component::ParentDir),
        "CCA pipette path must be under /share and contain no NUL"
    );
    let pipette = crate::qemu::shell_single_quote(guest_pipette_path);
    let tag = crate::qemu::shell_single_quote(&config.mount_tag);
    let share = crate::qemu::shell_single_quote(crate::GUEST_SHARE_ROOT);
    let network = network_script(&config.network)?;
    Ok(format!(
        "\
        #!/bin/sh\n\
        set -eu\n\
        dhcp_pid=\n\
        dhcp_dir=\n\
        log_dir=\n\
        shutdown() {{\n\
            status=$?\n\
            trap - EXIT\n\
            trap '' TERM INT\n\
            if [ -n \"$dhcp_pid\" ]; then\n\
                kill -KILL \"$dhcp_pid\" 2>/dev/null || :\n\
                kill -KILL \"-$dhcp_pid\" 2>/dev/null || :\n\
            fi\n\
            if [ \"$status\" -ne 0 ]; then\n\
                echo \"CCA host initialization failed: $status\" >&2\n\
                if [ -n \"$log_dir\" ]; then\n\
                    if ! echo \"CCA host initialization failed: $status\" > \"$log_dir/incubator-init-error.log\"; then\n\
                        echo 'Could not write CCA initialization failure log' >&2\n\
                    fi\n\
                fi\n\
            fi\n\
            if ! sync; then echo 'CCA shutdown sync failed' >&2; fi\n\
            poweroff -f\n\
            exit \"$status\"\n\
        }}\n\
        trap shutdown EXIT\n\
        trap 'exit 143' TERM\n\
        trap 'exit 130' INT\n\
        /bin/busybox --install /bin 2>/dev/null\n\
        mountpoint -q /dev || mount -t devtmpfs none /dev\n\
        mountpoint -q /proc || mount -t proc none /proc\n\
        mountpoint -q /sys || mount -t sysfs none /sys\n\
        mkdir -p /dev/pts {share} /root /tmp /etc\n\
        mountpoint -q /dev/pts || mount -t devpts devpts /dev/pts\n\
        date -u -s @{epoch}\n\
        mount -t 9p -o trans=virtio,version=9p2000.L,msize=512000 {tag} {share}\n\
        log_dir=\"$(dirname {pipette})/cca-logs\"\n\
        mkdir -p \"$log_dir\"\n\
        ip link set lo up\n\
        ip link set eth0 up\n\
        {network}\
        {{\n\
            ip address show\n\
            ip route show\n\
        }} > \"$log_dir/incubator-network.log\" 2>&1\n\
        export HOME=/root\n\
        export SSL_CERT_FILE=/{certificates}\n\
        cd {share}\n\
        {pipette} --transport tcp\n",
        certificates = crate::qemu::CA_CERTIFICATES_NAME,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn platform_networks_are_exclusive() {
        let qemu = build_init_script("/share/pipette", &CcaInitConfig::qemu(), 0).unwrap();
        let fvp = build_init_script("/share/pipette", &CcaInitConfig::fvp(), 0).unwrap();
        assert!(qemu.contains("msize=512000 'host' '/share'"));
        assert!(qemu.contains("ip addr replace 10.0.2.15/24 dev eth0"));
        assert!(!qemu.contains("udhcpc"));
        assert!(fvp.contains("msize=512000 'FM' '/share'"));
        assert!(fvp.contains("udhcpc -f -q -n -t 5 -T 3 -i eth0"));
        assert!(fvp.contains("- 100 + 30 * 100"));
        assert!(!fvp.contains("10.0.2."));
        for script in [&qemu, &fvp] {
            assert!(script.contains("trap shutdown EXIT"));
            assert!(script.contains("SSL_CERT_FILE="));
            assert!(!script.contains("kvm_cca_preflight"));
        }
    }

    #[test]
    fn rejects_invalid_dhcp_deadlines() {
        for timeout in [
            Duration::ZERO,
            Duration::from_secs(9),
            Duration::from_millis(10500),
            Duration::from_secs(121),
        ] {
            assert!(network_script(&CcaHostNetwork::Dhcp { timeout }).is_err());
        }
        for seconds in [10, 30, 120] {
            assert!(
                network_script(&CcaHostNetwork::Dhcp {
                    timeout: Duration::from_secs(seconds)
                })
                .is_ok()
            );
        }
    }

    #[test]
    fn quotes_platform_inputs_and_rejects_nul() {
        let mut config = CcaInitConfig::qemu();
        config.mount_tag = "tag'$(false)".into();
        let script = build_init_script("/share/a'b/pipette", &config, 42).unwrap();
        assert!(script.contains("'tag'\\''$(false)'"));
        assert!(script.contains("'/share/a'\\''b/pipette' --transport tcp"));
        config.mount_tag = "\0".into();
        assert!(build_init_script("/share/pipette", &config, 0).is_err());
        assert!(build_init_script("/other/pipette", &CcaInitConfig::qemu(), 0).is_err());
        assert!(build_init_script("/share/../outside/pipette", &CcaInitConfig::qemu(), 0).is_err());
    }

    #[cfg(target_os = "linux")]
    fn execute_dhcp(client: &str, seconds: u64) -> (std::process::Output, String) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in [
            ("udhcpc", client),
            ("ip", "echo configured"),
            ("grep", "exit 0"),
        ] {
            let executable = dir.path().join(name);
            std::fs::write(&executable, format!("#!/bin/sh\n{content}\n")).unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let script = format!(
            "set -eu\nlog_dir={}\n{}",
            crate::qemu::shell_single_quote(dir.path().to_str().unwrap()),
            dhcp_script(seconds),
        );
        let path = std::env::var_os("PATH").unwrap_or_default();
        let output = std::process::Command::new("timeout")
            .args(["--kill-after=1", "5", "sh", "-c", &script])
            .env(
                "PATH",
                format!("{}:{}", dir.path().display(), path.to_string_lossy()),
            )
            .env("HOOK_PID_FILE", dir.path().join("hook.pid"))
            .output()
            .unwrap();
        let log = std::fs::read_to_string(dir.path().join("incubator-dhcp.log")).unwrap();
        if client.contains("HOOK_PID_FILE") {
            let pid = std::fs::read_to_string(dir.path().join("hook.pid"))
                .expect("hook test must record its descendant PID");
            let pid: u32 = pid.trim().parse().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while let Ok(process) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                let state = process
                    .rsplit_once(") ")
                    .unwrap()
                    .1
                    .split_whitespace()
                    .next()
                    .unwrap();
                if state == "Z" {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = std::process::Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .status();
                    panic!("DHCP hook child {pid} survived cleanup in state {state}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        (output, log)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn dhcp_uses_completion_time_not_poll_time() {
        let dir = tempfile::tempdir().unwrap();
        for (completed, expected) in [(2999, 0), (3000, 0), (3001, 124)] {
            std::fs::write(dir.path().join("result"), format!("0 {completed}\n")).unwrap();
            let script = format!(
                "set -eu\ndhcp_dir={}\ndhcp_deadline=3000\n\
                 while :; do {} break; done\nexit \"$dhcp_status\"\n",
                crate::qemu::shell_single_quote(dir.path().to_str().unwrap()),
                dhcp_completion_check(),
            );
            let status = std::process::Command::new("sh")
                .args(["-c", &script])
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(expected));
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn dhcp_observes_result_published_while_sampling_clock() {
        let dir = tempfile::tempdir().unwrap();
        let script = format!(
            "set -eu\ndhcp_dir={}\ndhcp_deadline=3000\n\
             read() {{\n\
                 if [ \"$1\" = dhcp_now ]; then\n\
                     test ! -f \"$dhcp_dir/result\"\n\
                     echo '0 2999' > \"$dhcp_dir/result\"\n\
                     dhcp_now=30.50\n\
                 else\n\
                     command read \"$@\"\n\
                 fi\n\
             }}\n\
             while :; do {} break; done\nexit \"$dhcp_status\"\n",
            crate::qemu::shell_single_quote(dir.path().to_str().unwrap()),
            dhcp_poll(),
        );
        let output = std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn dhcp_success_and_failure_preserve_status_and_logs() {
        let (success, log) = execute_dhcp("echo lease-configured; exit 0", 2);
        assert!(success.status.success(), "{success:?}");
        assert!(log.contains("lease-configured"));
        let (failure, log) = execute_dhcp("echo no-lease; exit 7", 2);
        assert_eq!(failure.status.code(), Some(7));
        assert!(log.contains("no-lease"));
        assert!(String::from_utf8_lossy(&failure.stderr).contains("CCA DHCP failed with status 7"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn dhcp_deadline_kills_hung_hook_group() {
        let start = std::time::Instant::now();
        let (output, log) = execute_dhcp("sleep 30 & echo $! > \"$HOOK_PID_FILE\"; wait", 1);
        assert_eq!(output.status.code(), Some(124), "{output:?}");
        assert!(log.contains("CCA DHCP deadline exceeded"));
        assert!(start.elapsed() < Duration::from_secs(4));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn dhcp_cleans_descendants_when_client_exits_first() {
        let (output, _) = execute_dhcp("sleep 30 & echo $! > \"$HOOK_PID_FILE\"; exit 7", 2);
        assert_eq!(output.status.code(), Some(7));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn dhcp_rejects_success_without_network_configuration() {
        let (output, log) = execute_dhcp(
            "printf '#!/bin/sh\\nexit 0\\n' > \"$(dirname \"$0\")/ip\"; exit 0",
            2,
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(log.contains("did not configure"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn dhcp_allows_lease_without_dns() {
        let (output, log) = execute_dhcp(
            "printf '#!/bin/sh\\nexit 1\\n' > \"$(dirname \"$0\")/grep\"; exit 0",
            2,
        );
        assert!(output.status.success(), "{output:?}");
        assert!(log.contains("no DNS server"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn shutdown_ignores_signals_and_preserves_failure() {
        let script = build_init_script("/share/pipette", &CcaInitConfig::fvp(), 0).unwrap();
        let startup = script.split_once("/bin/busybox --install").unwrap().0;
        let output = std::process::Command::new("sh")
            .args([
                "-c",
                &format!(
                    "{startup}\n\
                     sync() {{ kill -TERM $$; kill -INT $$; }}\n\
                     poweroff() {{ echo POWEROFF_REACHED; }}\n\
                     exit 7\n"
                ),
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert!(String::from_utf8_lossy(&output.stdout).contains("POWEROFF_REACHED"));
    }
}
