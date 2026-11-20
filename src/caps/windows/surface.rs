//! Surface maker module: the Surface UEFI "Battery Limit" caps charging at
//! 80%, and newer models add Smart Charging. State read-back is not
//! generically available, so detection stays behavioral (roadmap FR-1).

use super::{CapOptions, Maker, MakerModule, SettingGuidance};

pub fn module() -> MakerModule {
    MakerModule {
        maker: Maker::Surface,
        cap_options: CapOptions {
            fixed: &[80],
            range: None,
            adaptive: true,
        },
        cap_guidance: Some(SettingGuidance {
            tool: "Surface UEFI",
            steps: "Restart into Surface UEFI (hold Power + Volume Up) and enable Battery Limit, \
                    which caps charging at 80%",
            url: None,
        }),
        adaptive_guidance: Some(SettingGuidance {
            tool: "Windows Smart Charging",
            steps: "Smart Charging is automatic on supported Surface models — no setting to \
                    change",
            url: None,
        }),
    }
}
