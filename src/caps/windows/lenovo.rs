//! Lenovo maker module: Lenovo Vantage exposes charge thresholds (custom
//! start/stop, plus Conservation Mode). Per the roadmap's open question,
//! how to read Vantage's threshold state programmatically is unverified —
//! detection stays behavioral (FR-1).

use super::{Maker, MakerModule, SettingGuidance};
use crate::caps::{CapOptions, CapRange};

pub fn module() -> MakerModule {
    MakerModule {
        maker: Maker::Lenovo,
        cap_options: CapOptions {
            fixed: &[],
            // Vantage custom charge threshold spans 60–100%.
            range: Some(CapRange {
                min: 60,
                max: 100,
                step: 5,
            }),
            adaptive: false,
        },
        cap_guidance: Some(SettingGuidance {
            tool: "Lenovo Vantage",
            steps: "Open Lenovo Vantage → Power and enable Conservation Mode or set a custom \
                    charge threshold",
            url: None,
        }),
        adaptive_guidance: None,
    }
}
