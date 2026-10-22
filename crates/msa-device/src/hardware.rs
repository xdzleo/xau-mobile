//! Device components sent when provisioning the MSA device account.
//!
//! Upstream reads the SMBIOS type-1 table (system version, serial, UUID)
//! and feeds the raw table into the CLEP challenge. A phone has no SMBIOS,
//! so this is upstream's own fallback for a host where reading it fails
//! (xodus on Linux without `pkexec`): the three SMBIOS components are sent
//! as errors and the challenge is computed over zeroed buffers. The
//! device account Microsoft issues is the same kind either way.

use base64::prelude::*;

use crate::clep;
use crate::models::devicecredential::Component;

pub fn probe_provision_components() -> Vec<Component> {
    let mut components = Vec::with_capacity(16);
    let smbios_buf = [0; 256];
    // Upstream's drive serial is "AA==", a single zero byte, so its
    // buffer is all zeroes too.
    let drive_buf = [0; 64];

    let (clepv2, clepv4) = clep::challenge::get_license_challange(smbios_buf, drive_buf);

    components.push(Component::new(4113, "AA==".to_string()));
    components.push(Component::error(4101));
    components.push(Component::new(8196, BASE64_STANDARD.encode(clepv2)));
    components.push(Component::new(8197, BASE64_STANDARD.encode(clepv4)));

    components.push(Component::error(4100));
    components.push(Component::error(4101));
    components.push(Component::error(4102));

    components.push(Component::new(4145, "AQAAAA==".to_string()));
    components.push(Component::error(4160));
    components.push(Component::error(4161));

    components
}
