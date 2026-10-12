// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Nested vm host definitions

use petri::PetriVmBuilder;
use petri::PetriVmmBackend;

pub(crate) async fn vm_host<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config.with_nested_virt().run().await?;

    vm.run_nested_test(&agent, Default::default()).await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Keep the FVP fixture alive until the inner test exits. The native runtime
/// performs checked fixture teardown before L1 poweroff, including on failure.
#[cfg(target_os = "linux")]
pub(crate) async fn vm_host_fvp(
    config: PetriVmBuilder<petri::fvp::FvpPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config.with_nested_virt().run().await?;
    let env = [
        (
            "PETRI_CAPABILITIES".into(),
            "cca,guest_memfd_in_place,cca_realm_vfio".into(),
        ),
        (
            "PETRI_VFIO_BDF_CCA_REALM_VFIO".into(),
            "0000:01:00.0".into(),
        ),
    ]
    .into();
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
