//! K8s Lease-based leader election for HA spurctld deployments.
//!
//! When --enable-leader-election is set, spurctld acquires a Lease object
//! before starting the gRPC server and scheduler loop. Only the leader
//! processes requests; standby replicas block in acquire_lease() until the
//! leader fails to renew.

use anyhow::Context;
use chrono::Utc;
use k8s_openapi::api::coordination::v1::Lease;
use kube::api::{Api, ObjectMeta, Patch, PatchParams, PostParams};
use kube::Client;
use tracing::{debug, info, warn};

const LEASE_NAME: &str = "spurctld-leader";
const LEASE_DURATION_SECS: i32 = 15;
const RENEW_INTERVAL_SECS: u64 = 5;

/// Block until this instance acquires the leader Lease.
/// Once acquired, spawns a background task to renew the Lease every 5s.
/// If renewal fails, the process exits so K8s can restart it.
pub async fn acquire_lease(namespace: &str) -> anyhow::Result<()> {
    let client = Client::try_default()
        .await
        .context("failed to create K8s client for leader election")?;
    let leases: Api<Lease> = Api::namespaced(client.clone(), namespace);

    let identity = get_identity();
    info!(identity = %identity, "leader election identity");

    // Try to acquire the Lease in a loop
    loop {
        match try_acquire(&leases, namespace, &identity).await {
            Ok(true) => {
                info!("acquired leader Lease");
                break;
            }
            Ok(false) => {
                debug!("Lease held by another instance, retrying in 5s");
                tokio::time::sleep(std::time::Duration::from_secs(RENEW_INTERVAL_SECS)).await;
            }
            Err(e) => {
                warn!("leader election error: {e}, retrying in 5s");
                tokio::time::sleep(std::time::Duration::from_secs(RENEW_INTERVAL_SECS)).await;
            }
        }
    }

    // Spawn background renewal task
    let ns = namespace.to_string();
    tokio::spawn(async move {
        renewal_loop(client, &ns, &identity).await;
    });

    Ok(())
}

/// Try to create or update the Lease. Returns true if we became the leader.
async fn try_acquire(leases: &Api<Lease>, namespace: &str, identity: &str) -> anyhow::Result<bool> {
    let now = Utc::now();
    let now_micro = k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(now);

    match leases.get(LEASE_NAME).await {
        Ok(existing) => {
            let spec = existing.spec.as_ref();
            let holder = spec.and_then(|s| s.holder_identity.as_deref());
            let renew_time = spec.and_then(|s| s.renew_time.as_ref());
            let duration = spec
                .and_then(|s| s.lease_duration_seconds)
                .unwrap_or(LEASE_DURATION_SECS);

            // Check if the existing Lease has expired
            let expired = renew_time
                .map(|t| {
                    let elapsed = now.signed_duration_since(t.0);
                    elapsed.num_seconds() > duration as i64
                })
                .unwrap_or(true);

            if holder == Some(identity) || expired {
                // We can take/renew the Lease
                let patch = serde_json::json!({
                    "spec": {
                        "holderIdentity": identity,
                        "leaseDurationSeconds": LEASE_DURATION_SECS,
                        "acquireTime": now_micro,
                        "renewTime": now_micro,
                    }
                });
                leases
                    .patch(
                        LEASE_NAME,
                        &PatchParams::apply("spurctld"),
                        &Patch::Merge(&patch),
                    )
                    .await?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {
            // Lease doesn't exist yet — create it
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(LEASE_NAME.into()),
                    namespace: Some(namespace.into()),
                    ..Default::default()
                },
                spec: Some(k8s_openapi::api::coordination::v1::LeaseSpec {
                    holder_identity: Some(identity.into()),
                    lease_duration_seconds: Some(LEASE_DURATION_SECS),
                    acquire_time: Some(now_micro.clone()),
                    renew_time: Some(now_micro),
                    ..Default::default()
                }),
            };
            match leases.create(&PostParams::default(), &lease).await {
                Ok(_) => Ok(true),
                Err(kube::Error::Api(e)) if e.code == 409 => Ok(false), // lost race
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Renew the Lease periodically. If renewal fails, exit the process.
async fn renewal_loop(client: Client, namespace: &str, identity: &str) {
    let leases: Api<Lease> = Api::namespaced(client, namespace);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(RENEW_INTERVAL_SECS));

    loop {
        interval.tick().await;

        let now = Utc::now();
        let now_micro = k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(now);

        let patch = serde_json::json!({
            "spec": {
                "holderIdentity": identity,
                "renewTime": now_micro,
            }
        });

        match leases
            .patch(
                LEASE_NAME,
                &PatchParams::apply("spurctld"),
                &Patch::Merge(&patch),
            )
            .await
        {
            Ok(_) => {
                debug!("leader Lease renewed");
            }
            Err(e) => {
                // Lost the Lease — exit so K8s can restart us
                tracing::error!("leader Lease renewal failed: {e} — exiting");
                std::process::exit(1);
            }
        }
    }
}

/// Generate a unique identity for this instance (hostname + PID).
fn get_identity() -> String {
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".into());
    format!("{}-{}", hostname, std::process::id())
}
