// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checks the SNP Linux firmware handoff without a low-ECAM workaround.

use anyhow::Context;
use pipette_client::PipetteClient;
use pipette_client::cmd;

pub(crate) async fn verify(agent: &PipetteClient) -> anyhow::Result<()> {
    let shell = agent.unix_shell();
    let dmesg = cmd!(shell, "dmesg").read().await?;
    anyhow::ensure!(
        dmesg.contains("SEV: Status: SEV SEV-ES SEV-SNP"),
        "guest did not report active SNP"
    );
    let tables = cmd!(shell, "cat /sys/firmware/efi/systab").read().await?;
    let command_line = cmd!(shell, "cat /proc/cmdline").read().await?;
    anyhow::ensure!(
        command_line
            .split_whitespace()
            .take_while(|arg| *arg != "--")
            .any(|arg| arg == "efi=noruntime"),
        "synthetic EFI handoff did not disable runtime services"
    );
    anyhow::ensure!(
        tables.lines().any(|line| line.starts_with("SMBIOS3=")),
        "EFI did not publish SMBIOS3: {tables}"
    );
    let vendor = cmd!(shell, "cat /sys/class/dmi/id/bios_vendor")
        .read()
        .await?;
    anyhow::ensure!(!vendor.trim().is_empty(), "SMBIOS BIOS vendor is empty");
    let date = cmd!(shell, "cat /sys/class/dmi/id/bios_date")
        .read()
        .await?;
    let year: u16 = date
        .trim()
        .rsplit('/')
        .next()
        .context("SMBIOS BIOS date has no year")?
        .parse()
        .context("parse SMBIOS BIOS year")?;
    anyhow::ensure!(year >= 2016, "SMBIOS BIOS date is too old: {date}");
    let mcfg = agent
        .read_file("/sys/firmware/acpi/tables/MCFG")
        .await
        .context("read guest MCFG")?;
    let ecam = u64::from_le_bytes(
        mcfg.get(44..52)
            .context("MCFG has no host bridge")?
            .try_into()
            .context("MCFG ECAM address is not 64 bits")?,
    );
    anyhow::ensure!(
        ecam > u64::from(u32::MAX),
        "test did not exercise above-4-GiB ECAM: {ecam:#x}"
    );
    tracing::info!(
        ecam,
        bios_date = date.trim(),
        "verified SNP EFI and SMBIOS handoff"
    );
    Ok(())
}
