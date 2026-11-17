// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! PCI identity, and the stable device ids every device kind derives from it.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PciIdentity {
    pub(crate) domain: u32,
    pub(crate) bus: u8,
    pub(crate) device: u8,
    pub(crate) function: u8,
    pub(crate) partition_rank: u8,
}

pub(crate) fn bdf_from_identity(id: &PciIdentity) -> String {
    format!(
        "{:04x}:{:02x}:{:02x}.{}",
        id.domain, id.bus, id.device, id.function
    )
}

/// Low 8 bits of a stable_id are the partition index; the rest is the PCIe BDF
/// anchor (domain:bus:dev.func). Mask these off (`id & !STABLE_ID_PARTITION_MASK`)
/// to get the physical device identity — two stable_ids share silicon iff their
/// BDF anchors are equal.
pub const STABLE_ID_PARTITION_MASK: u64 = 0xFF;

/// Encode a reload-invariant device id from the PCIe BDF (domain:bus:dev.func
/// anchor) plus a per-partition rank. The domain occupies bits 24..39, so two
/// devices on different PCI domains with the same bus:dev.func never collide.
///
/// The exporter hooks in `examples/hooks/dme/common.sh` and
/// `examples/hooks/gpu-node/common-env.sh` decode this layout from KFD
/// topology to find render IDs; change them together with it.
pub(crate) fn encode_stable_id(id: &PciIdentity) -> u64 {
    (u64::from(id.domain & 0xffff) << 24)
        | (u64::from(id.bus) << 16)
        | (u64::from(id.device) << 11)
        | (u64::from(id.function) << 8)
        | u64::from(id.partition_rank)
}

/// Decode a stable_id's BDF anchor back to the k8s-DRA `dddd:bb:dd.f` form
/// (e.g. `0000:19:00.0`), the value the DRA driver publishes as
/// `resource.kubernetes.io/pciBusID`.
pub fn stable_id_to_bdf(stable_id: u64) -> String {
    let domain = (stable_id >> 24) & 0xffff;
    let bus = (stable_id >> 16) & 0xff;
    let dev = (stable_id >> 11) & 0x1f;
    let func = (stable_id >> 8) & 0x07;
    format!("{:04x}:{:02x}:{:02x}.{}", domain, bus, dev, func)
}

/// The stable id of a device named by its `dddd:bb:dd.f` sysfs address, rank 0.
pub fn stable_id_from_bdf(bdf: &str) -> Option<u64> {
    let (domain, rest) = bdf.split_once(':')?;
    let (bus, rest) = rest.split_once(':')?;
    let (device, function) = rest.split_once('.')?;
    if domain.len() != 4 || bus.len() != 2 || device.len() != 2 || function.len() != 1 {
        return None;
    }
    let identity = PciIdentity {
        domain: u32::from_str_radix(domain, 16).ok()?,
        bus: u8::from_str_radix(bus, 16).ok()?,
        device: u8::from_str_radix(device, 16).ok().filter(|d| *d <= 0x1f)?,
        function: function.parse().ok().filter(|f| *f <= 7)?,
        partition_rank: 0,
    };
    Some(encode_stable_id(&identity))
}

/// The node-local PCI host bridge (`pciDDDD:BB`) a sysfs device hangs off.
pub fn pci_root(device: &Path) -> Option<String> {
    let resolved = device.canonicalize().ok()?;
    resolved
        .ancestors()
        .filter_map(|path| path.file_name()?.to_str())
        .find(|name| {
            let bytes = name.as_bytes();
            bytes.len() == 10
                && name.starts_with("pci")
                && bytes[7] == b':'
                && bytes[3..7]
                    .iter()
                    .chain(&bytes[8..])
                    .all(u8::is_ascii_hexdigit)
        })
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(domain: u32, bus: u8, device: u8, function: u8) -> PciIdentity {
        PciIdentity {
            domain,
            bus,
            device,
            function,
            partition_rank: 0,
        }
    }

    #[test]
    fn a_bdf_string_encodes_like_the_identity_it_names() {
        for id in [identity(0, 0x05, 0, 0), identity(1, 0xc1, 0x1f, 7)] {
            let bdf = bdf_from_identity(&id);
            assert_eq!(
                stable_id_from_bdf(&bdf),
                Some(encode_stable_id(&id)),
                "{bdf}"
            );
        }
    }

    #[test]
    fn a_bdf_string_round_trips_through_its_stable_id() {
        let id = stable_id_from_bdf("0000:85:00.1").unwrap();
        assert_eq!(stable_id_to_bdf(id), "0000:85:00.1");
    }

    #[test]
    fn resolves_node_scoped_pci_host_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir
            .path()
            .join("devices/pci0001:00/0001:00:01.1/0001:04:00.0");
        std::fs::create_dir_all(&device).unwrap();
        assert_eq!(pci_root(&device).as_deref(), Some("pci0001:00"));
        assert_eq!(pci_root(dir.path()), None);
    }

    #[test]
    fn malformed_bdfs_have_no_stable_id() {
        for bad in [
            "",
            "85:00.0",
            "0000:85:00",
            "0000:85:20.0",
            "0000:85:00.8",
            "zzzz:85:00.0",
        ] {
            assert_eq!(stable_id_from_bdf(bad), None, "{bad:?}");
        }
    }
}
