//! Unknown-maker module: no recognized OEM. The app keeps working with
//! generic advice (roadmap FR-0 acceptance): no cap options are claimed and
//! nothing is invented about what the machine can do.

use super::{CapOptions, Maker, MakerModule, SettingGuidance};

pub fn module() -> MakerModule {
    MakerModule {
        maker: Maker::Unknown,
        cap_options: CapOptions::none(),
        cap_guidance: Some(SettingGuidance {
            tool: "Your laptop maker's tools",
            steps: "A charge limit is usually in the maker's companion app or in the BIOS/UEFI \
                    setup — check your maker's documentation for a battery charge limit option",
            url: None,
        }),
        // No known tool to point at; adaptive-charging advice stays silent.
        adaptive_guidance: None,
    }
}
