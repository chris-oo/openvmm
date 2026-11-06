// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Nested vm host definitions

use petri::PetriVmBuilder;
#[cfg(target_os = "linux")]
use petri::PetriVmmBackend;
use petri::openvmm::OpenVmmPetriBackend;

pub(crate) async fn vm_host(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_nested_virt()
        .with_no_vmbus()
        .modify_backend(|b| b.with_pcie_root_topology(1, 1, 1))
        .run()
        .await?;

    vm.run_nested_test(&agent, Default::default()).await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[vmm_test_macros::vmm_test(
    qemu_linux_direct_aarch64,
    openvmm_linux_direct_x64,
    openvmm_linux_direct_aarch64
)]
async fn shared_directory<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs_err::write(
        directory.path().join("marker"),
        b"shared after agent startup",
    )?;
    let cache = tempfile::tempdir()?;
    fs_err::write(cache.path().join("marker"), b"child share")?;
    let (vm, agent) = config
        .with_shared_directory("a-cache", cache.path(), "/shared directory/cache")
        .with_shared_directory("z-data", directory.path(), "/shared directory/././")
        .run()
        .await?;
    assert_eq!(
        agent.read_file("/shared directory/marker").await?,
        b"shared after agent startup",
    );
    agent
        .write_file("/shared directory/result", b"guest write".as_slice())
        .await?;
    assert_eq!(
        fs_err::read(directory.path().join("result"))?,
        b"guest write"
    );
    assert_eq!(
        agent.read_file("/shared directory/cache/marker").await?,
        b"child share",
    );
    agent
        .write_file("/shared directory/cache/result", b"child write".as_slice())
        .await?;
    assert_eq!(fs_err::read(cache.path().join("result"))?, b"child write");
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Keep the FVP fixture alive until the inner test exits and checked teardown.
#[cfg(target_os = "linux")]
pub(crate) async fn vm_host_fvp(
    config: PetriVmBuilder<petri::fvp::FvpPetriBackend>,
) -> anyhow::Result<()> {
    run_fvp_nested(config, false).await
}

/// Run a nested Realm test with the explicit AHCI assignment fixture.
#[cfg(target_os = "linux")]
pub(crate) async fn vm_host_fvp_realm_vfio(
    config: PetriVmBuilder<petri::fvp::FvpPetriBackend>,
) -> anyhow::Result<()> {
    run_fvp_nested(
        config.modify_backend(|backend| backend.with_realm_vfio()),
        true,
    )
    .await
}

#[cfg(target_os = "linux")]
async fn run_fvp_nested(
    config: PetriVmBuilder<petri::fvp::FvpPetriBackend>,
    realm_vfio: bool,
) -> anyhow::Result<()> {
    let payload_mode = match std::env::var("PETRI_FVP_PAYLOAD") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error.into()),
    };
    let (vm, agent) = config.with_nested_virt().run().await?;
    let mut env = std::collections::BTreeMap::from([(
        "PETRI_CAPABILITIES".into(),
        if realm_vfio {
            "cca,guest_memfd_in_place,cca_realm_vfio"
        } else {
            "cca,guest_memfd_in_place"
        }
        .into(),
    )]);
    if let Some(mode) = payload_mode {
        env.insert("PETRI_FVP_PAYLOAD".into(), mode);
    }
    if realm_vfio {
        env.insert(
            "PETRI_VFIO_BDF_CCA_REALM_VFIO".into(),
            "0000:01:00.0".into(),
        );
    }
    let nested = vm.run_nested_test(&agent, env).await;
    let completed = nested.is_ok()
        || nested
            .as_ref()
            .is_err_and(|error| error.is::<petri::NestedTestFailure>());
    let shutdown = if completed {
        vm.wait_for_clean_teardown().await
    } else {
        vm.teardown().await
    };
    match (nested, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(shutdown)) => {
            Err(error.context(format!("FVP L1 cleanup also failed: {shutdown:#}")))
        }
    }
}
