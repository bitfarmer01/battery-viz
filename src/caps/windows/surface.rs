//! Surface maker module: the Surface UEFI "Battery Limit" caps charging at
//! 80%, and newer models add Smart Charging. State read-back is not
//! generically available, so detection stays behavioral (roadmap FR-1).

use super::{CapOptions, Maker, MakerModule, SettingGuidance};

pub fn module() -> MakerModule {
    MakerModule {
        maker: Maker::Surface,
        cap_options: CapOptions {
            // Surface UEFI's Battery Limit is a fixed 50% cap (Microsoft
            // Learn, "Surface Battery Limit setting"): charging stops at
            // 50% of maximum capacity and only resumes below 50%. The
            // 80% figure belongs to Smart Charging, which is automatic.
            fixed: &[50],
            range: None,
            adaptive: true,
        },
        cap_guidance: Some(SettingGuidance {
            tool: "Surface UEFI",
            steps: "Restart into Surface UEFI (press and hold Volume Up while pressing and \
                    releasing Power), then under Boot configuration → Advanced options enable \
                    Battery Limit, which caps charging at 50%",
            url: Some("https://learn.microsoft.com/en-us/surface/battery-limit"),
        }),
        adaptive_guidance: Some(SettingGuidance {
            tool: "Windows Smart Charging",
            steps: "Smart Charging is automatic on supported Surface models and limits \
                    charging to about 80% during prolonged AC use — no setting to change",
            url: None,
        }),
    }
}
