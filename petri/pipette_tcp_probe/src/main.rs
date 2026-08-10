// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Execute one command through an already-running pipette TCP agent.

#![forbid(unsafe_code)]

use anyhow::Context;
use clap::Parser;
use pal_async::DefaultPool;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    /// Loopback TCP port forwarded to the pipette agent.
    #[clap(long)]
    port: u16,

    /// Directory for pipette command output.
    #[clap(long)]
    output_dir: PathBuf,

    /// Command to run through pipette.
    #[clap(trailing_var_arg = true, required = true)]
    command: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let exit_code = DefaultPool::run_with(async |driver| {
        std::fs::create_dir_all(&args.output_dir)
            .context("failed to create pipette probe output directory")?;
        let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, args.port));
        let socket = pal_async::socket::PolledSocket::connect_tcp(&driver, address)
            .await
            .with_context(|| format!("failed to connect to pipette at {address}"))?;
        socket
            .get()
            .set_nodelay(true)
            .context("failed to set TCP_NODELAY")?;

        let client = pipette_client::PipetteClient::new(&driver, socket, &args.output_dir)
            .await
            .context("failed to initialize pipette client")?;

        let command_result = async {
            let (program, command_args) = args.command.split_first().unwrap();
            let output = client
                .command(program)
                .args(command_args)
                .output()
                .await
                .context("failed to execute pipette probe command")?;
            std::fs::write(args.output_dir.join("command.stdout"), output.stdout)
                .context("failed to write pipette probe stdout")?;
            std::fs::write(args.output_dir.join("command.stderr"), output.stderr)
                .context("failed to write pipette probe stderr")?;
            Ok::<_, anyhow::Error>(output.status.code().unwrap_or(1))
        }
        .await;
        let poweroff_error = client.power_off().await.err();

        match command_result {
            Ok(exit_code) => {
                if let Some(error) = poweroff_error {
                    eprintln!("warning: failed to power off pipette host: {error:#}");
                }
                Ok(exit_code)
            }
            Err(error) => {
                if let Some(poweroff_error) = poweroff_error {
                    Err(error.context(format!(
                        "also failed to power off pipette host: {poweroff_error:#}"
                    )))
                } else {
                    Err(error)
                }
            }
        }
    })?;

    std::process::exit(exit_code);
}
