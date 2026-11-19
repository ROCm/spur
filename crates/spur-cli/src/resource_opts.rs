// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Slurm's option-source conflict rule for per-GPU/memory resource flags:
//! whether a merged option came from the command line (or a directive), an
//! input environment variable, or neither, and which of several mutually
//! exclusive options survive.

use anyhow::{bail, Context, Result};

use crate::env_defaults::env_first;

/// Bounds a `Ping` used only to check a capability before deciding whether to
/// refuse — not worth the user's full patience if the controller is wedged.
const CAPABILITY_PING_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(spur_core::config::CAPABILITY_PING_TIMEOUT_SECS);

/// Confirm the controller advertises `cap` before sending a request it would
/// otherwise silently weaken or drop; refuses with `refusal` when absent.
pub(crate) async fn require_controller_capability(
    client: &mut spur_proto::proto::slurm_controller_client::SlurmControllerClient<
        crate::authclient::AuthChannel,
    >,
    cap: &str,
    refusal: &str,
) -> Result<()> {
    let ping = tokio::time::timeout(CAPABILITY_PING_TIMEOUT, client.ping(()))
        .await
        .context("ping to controller timed out")?
        .context("ping to controller failed")?
        .into_inner();
    if !ping.capabilities.iter().any(|c| c == cap) {
        bail!("{refusal}");
    }
    Ok(())
}

/// Refuses `--cpus-per-gpu`/`--mem-per-gpu` against a controller that would
/// drop them (and run the job with 1 CPU per task) rather than enforce them.
/// Shared by sbatch, salloc and standalone srun, which all submit a `JobSpec`.
pub(crate) async fn require_per_gpu_options_capability(
    client: &mut spur_proto::proto::slurm_controller_client::SlurmControllerClient<
        crate::authclient::AuthChannel,
    >,
    cpus_per_gpu: Option<u32>,
    memory_per_gpu_mb: u64,
) -> Result<()> {
    if cpus_per_gpu.is_none() && memory_per_gpu_mb == 0 {
        return Ok(());
    }
    require_controller_capability(
        client,
        spur_core::node::CAP_PER_GPU_OPTIONS,
        "the controller does not support --cpus-per-gpu/--mem-per-gpu; upgrade spurctld or submit without them",
    )
    .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OptionSource {
    Unset,
    Explicit,
    Env,
}

/// Source of a merged option: the command line or a directive is explicit;
/// a set input variable that was not overridden by the command line is env.
pub(crate) fn option_source(cli_set: bool, env_names: &[&str], is_set: bool) -> OptionSource {
    if cli_set {
        return OptionSource::Explicit;
    }
    if env_first(env_names).is_some() {
        return OptionSource::Env;
    }
    if is_set {
        return OptionSource::Explicit;
    }
    OptionSource::Unset
}

/// Slurm's rule for one conflict group: two explicit, or two from env, is an
/// error; an explicit one drops the env ones. Returns which members survive.
/// `explicit_conflict` and `env_conflict` are separate messages so each names
/// what the user actually set (a flag, or the env vars behind it).
pub(crate) fn surviving(
    sources: &[OptionSource],
    explicit_conflict: &str,
    env_conflict: &str,
) -> Result<Vec<bool>> {
    let count = |s| sources.iter().filter(|x| **x == s).count();
    let (explicit, env) = (count(OptionSource::Explicit), count(OptionSource::Env));
    if explicit > 1 {
        bail!("{explicit_conflict}");
    }
    if explicit == 0 && env > 1 {
        bail!("{env_conflict}");
    }
    Ok(sources
        .iter()
        .map(|s| match s {
            OptionSource::Unset => false,
            OptionSource::Explicit => true,
            OptionSource::Env => explicit == 0,
        })
        .collect())
}

pub(crate) const CPU_CONFLICT: &str = "--cpus-per-gpu cannot be combined with --cpus-per-task (-c)";
pub(crate) const MEM_CONFLICT: &str =
    "--mem, --mem-per-cpu, and --mem-per-gpu are mutually exclusive";

/// Per-GPU options need a GPU request; refused before the controller sees them.
pub(crate) fn require_gpus(cpus_per_gpu: bool, mem_per_gpu: bool, has_gpus: bool) -> Result<()> {
    if has_gpus {
        return Ok(());
    }
    if cpus_per_gpu {
        bail!("--cpus-per-gpu requires a GPU request");
    }
    if mem_per_gpu {
        bail!("--mem-per-gpu requires a GPU request");
    }
    Ok(())
}

/// A job's GPU request from any of its forms (`--gpus*`, or `gpu` in `--gres`).
pub(crate) fn requests_gpus(
    gpus: bool,
    gpus_per_node: bool,
    gpus_per_task: bool,
    gres: &[String],
) -> bool {
    gpus || gpus_per_node
        || gpus_per_task
        || gres.iter().any(
            |g| matches!(spur_core::resource::parse_gres(g), Some((name, _, _)) if name == "gpu"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use OptionSource::*;

    const ENV_CONFLICT: &str = "env conflict";

    #[test]
    fn both_explicit_is_an_error() {
        assert_eq!(
            surviving(&[Explicit, Explicit], CPU_CONFLICT, ENV_CONFLICT)
                .unwrap_err()
                .to_string(),
            CPU_CONFLICT
        );
    }
    #[test]
    fn both_from_env_is_an_error() {
        assert_eq!(
            surviving(&[Unset, Env, Env], MEM_CONFLICT, ENV_CONFLICT)
                .unwrap_err()
                .to_string(),
            ENV_CONFLICT
        );
    }
    #[test]
    fn explicit_drops_env() {
        assert_eq!(
            surviving(&[Env, Explicit], CPU_CONFLICT, ENV_CONFLICT).unwrap(),
            [false, true]
        );
        assert_eq!(
            surviving(&[Explicit, Unset, Env], MEM_CONFLICT, ENV_CONFLICT).unwrap(),
            [true, false, false]
        );
    }
    #[test]
    fn a_single_option_survives_whatever_its_source() {
        assert_eq!(
            surviving(&[Env, Unset], CPU_CONFLICT, ENV_CONFLICT).unwrap(),
            [true, false]
        );
    }
    #[test]
    #[serial(env_injection)]
    fn source_prefers_cli_then_env_then_directive() {
        let env = crate::env_defaults::EnvGuard::new();
        env.set("SBATCH_CPUS_PER_GPU", "4");
        assert_eq!(
            option_source(true, &["SBATCH_CPUS_PER_GPU"], true),
            Explicit
        );
        assert_eq!(option_source(false, &["SBATCH_CPUS_PER_GPU"], true), Env);
        assert_eq!(
            option_source(false, &["SBATCH_MEM_PER_GPU"], true),
            Explicit
        );
        assert_eq!(option_source(false, &["SBATCH_MEM_PER_GPU"], false), Unset);
    }
    #[test]
    fn per_gpu_options_need_gpus() {
        assert_eq!(
            require_gpus(true, false, false).unwrap_err().to_string(),
            "--cpus-per-gpu requires a GPU request"
        );
        assert_eq!(
            require_gpus(false, true, false).unwrap_err().to_string(),
            "--mem-per-gpu requires a GPU request"
        );
        assert!(require_gpus(true, true, true).is_ok());
    }
    #[test]
    fn gres_gpu_counts_as_a_gpu_request() {
        assert!(requests_gpus(false, false, false, &["gpu:mi300x:2".into()]));
        assert!(!requests_gpus(false, false, false, &["fpga:1".into()]));
    }

    /// Refused without the capability (an old controller would drop the
    /// options silently); accepted once the controller advertises it.
    #[tokio::test]
    async fn per_gpu_options_are_refused_without_the_capability_and_accepted_with_it() {
        let (addr, capture) = crate::mock_controller::spawn().await;
        let mut client = crate::mock_controller::client(addr).await;

        let err = require_per_gpu_options_capability(&mut client, Some(4), 0)
            .await
            .expect_err("an old controller must refuse --cpus-per-gpu");
        assert!(err.to_string().contains("does not support --cpus-per-gpu"));

        let err = require_per_gpu_options_capability(&mut client, None, 1024)
            .await
            .expect_err("an old controller must refuse --mem-per-gpu too");
        assert!(err.to_string().contains("does not support --cpus-per-gpu"));

        capture.set_ping_capabilities(vec![spur_core::node::CAP_PER_GPU_OPTIONS.to_string()]);
        require_per_gpu_options_capability(&mut client, Some(4), 0)
            .await
            .expect("a controller advertising the capability is accepted");
    }

    /// A controller that cannot even answer `Ping` must not be treated as one
    /// that silently supports the capability.
    #[tokio::test]
    async fn a_failed_ping_refuses_per_gpu_options() {
        let (addr, capture) = crate::mock_controller::spawn().await;
        let mut client = crate::mock_controller::client(addr).await;
        capture.set_ping_error(tonic::Code::Unavailable);

        require_per_gpu_options_capability(&mut client, Some(4), 0)
            .await
            .expect_err("a failed ping must refuse rather than proceed");
    }

    #[tokio::test]
    async fn a_job_with_neither_per_gpu_option_is_always_accepted() {
        let (addr, _capture) = crate::mock_controller::spawn().await;
        let mut client = crate::mock_controller::client(addr).await;
        require_per_gpu_options_capability(&mut client, None, 0)
            .await
            .expect("no per-GPU option means nothing to check");
    }
}
