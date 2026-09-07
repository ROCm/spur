// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The address MetalLB hands to the gateway.

use anyhow::Result;

use super::kube;

const METALLB_NAMESPACE: &str = "metallb-system";

/// MetalLB hands out nothing until a pool exists, so the `https` Service keeps `<pending>` and the
/// Gateway is never programmed. cluster-bloom builds the same single-address pool, but writes it to
/// the RKE2 manifest directory, which k0s does not read.
fn render_lb_pool(address: &str) -> String {
    format!(
        "---\napiVersion: metallb.io/v1beta1\nkind: IPAddressPool\nmetadata:\n  name: \
         spur-node-pool\n  namespace: {METALLB_NAMESPACE}\nspec:\n  addresses:\n  - \
         {address}/32\n---\napiVersion: metallb.io/v1beta1\nkind: L2Advertisement\nmetadata:\n  \
         name: spur-node-l2\n  namespace: {METALLB_NAMESPACE}\nspec:\n  ipAddressPools:\n  - \
         spur-node-pool\n"
    )
}

/// Read the address the node reaches the network from. This becomes the load balancer address, so
/// it must be the node's own: MetalLB answers ARP for it in L2 mode.
fn parse_route_src(route: &str) -> Option<String> {
    let mut fields = route.split_whitespace();
    while let Some(field) = fields.next() {
        if field == "src" {
            return fields.next().map(str::to_string);
        }
    }
    None
}

/// Give MetalLB an address to hand out. This runs after the deployer, because the CRD arrives with
/// the platform stack. A cluster that never installs MetalLB is not an error, so this warns.
pub async fn ensure_load_balancer_pool(node: &str) -> Result<()> {
    // ArgoCD applies the platform stack after the deployer returns, so nothing of MetalLB exists
    // yet: measured on a fresh cluster, its ArgoCD application appears 67 seconds later and its CRD
    // 70. Both were absent at this point, so there is no early signal to gate on. Poll for the CRD.
    if !kube::wait_for_crd("ipaddresspools.metallb.io").await {
        eprintln!("MetalLB installed no CRD, so no address pool was created");
        return Ok(());
    }
    let listed = kube::kubectl()
        .args([
            "get",
            "ipaddresspool",
            "-n",
            METALLB_NAMESPACE,
            "-o",
            "name",
        ])
        .output()
        .await?;
    if !String::from_utf8_lossy(&listed.stdout).trim().is_empty() {
        return Ok(());
    }

    let address = load_balancer_address(node).await?;
    // The CRD lands before MetalLB serves its validating webhook, and the API server rejects the
    // pool until it can call that webhook. Retrying is name-independent, where waiting on a named
    // deployment would break whenever the chart renames it.
    let mut last = None;
    for attempt in 0..30 {
        match kube::apply_echoing(
            render_lb_pool(&address).as_bytes(),
            "the MetalLB address pool",
        )
        .await
        {
            Ok(()) => {
                eprintln!("Created MetalLB address pool {address}/32");
                return Ok(());
            }
            Err(e) => {
                if attempt == 0 {
                    eprintln!("Waiting for MetalLB to accept the address pool ...");
                }
                last = Some(e);
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("could not apply the MetalLB address pool")))
}

/// The address MetalLB hands to the gateway.
///
/// It must belong to a Kubernetes node, because only a node runs a MetalLB speaker to answer ARP
/// for it. The node this command runs on is the k0s control plane, which carries no kubelet and is
/// therefore no Kubernetes node at all, so its own address would reach nothing. Read the address
/// off the node that runs the gateway instead, and fall back to the local one for a cluster where
/// the two are the same machine.
async fn load_balancer_address(node: &str) -> Result<String> {
    let out = kube::kubectl()
        .args([
            "get",
            "node",
            node,
            "-o",
            "jsonpath={.status.addresses[?(@.type=='InternalIP')].address}",
        ])
        .output()
        .await?;
    if let Some(address) = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .filter(|address| !address.is_empty())
    {
        return Ok(address.to_string());
    }
    let route = local_route().await?;
    parse_route_src(&route).ok_or_else(|| {
        anyhow::anyhow!(
            "node {node} reports no InternalIP, and this node's address is not in `ip route get \
             1.1.1.1`: {route}"
        )
    })
}

async fn local_route() -> Result<String> {
    let out = tokio::process::Command::new("ip")
        .args(["route", "get", "1.1.1.1"])
        .output()
        .await?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // MetalLB with no pool leaves the gateway Service on `<pending>` forever, and the Gateway then
    // reports "No addresses have been assigned".
    #[test]
    fn the_pool_offers_the_node_its_own_address() {
        let rendered = render_lb_pool("10.0.0.5");
        assert!(rendered.contains("- 10.0.0.5/32"));
        assert!(rendered.contains("kind: IPAddressPool"));
        // Without an L2Advertisement the pool exists and still advertises nothing.
        assert!(rendered.contains("kind: L2Advertisement"));
        assert!(rendered.contains("- spur-node-pool"));
    }

    #[test]
    fn the_node_address_comes_from_the_route() {
        let route = "1.1.1.1 via 10.0.0.1 dev enp0s3 src 10.0.0.5 uid 0 \n    cache";
        assert_eq!(parse_route_src(route), Some("10.0.0.5".to_string()));
    }

    #[test]
    fn a_route_without_a_source_is_not_guessed() {
        assert_eq!(parse_route_src("1.1.1.1 dev enp0s3 uid 0"), None);
        assert_eq!(parse_route_src(""), None);
        // `src` as the last field promises an address that is not there.
        assert_eq!(parse_route_src("1.1.1.1 dev enp0s3 src"), None);
    }
}
