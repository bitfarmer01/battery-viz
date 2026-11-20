//! Dell maker module: Dell Power Manager (consumer) or the BIOS Battery
//! Health Manager (business) expose custom charge thresholds. Reading the
//! current threshold state back programmatically is not generically
//! available, so detection stays behavioral (roadmap FR-1).

use super::{Maker, MakerModule, SettingGuidance};
use crate::caps::{CapOptions, CapRange};

pub fn module() -> MakerModule {
    MakerModule {
        maker: Maker::Dell,
        cap_options: CapOptions {
            fixed: &[],
            // Dell Power Manager custom charging spans 50–100%.
            range: Some(CapRange {
                min: 50,
                max: 100,
                step: 5,
            }),
            adaptive: false,
        },
        cap_guidance: Some(SettingGuidance {
            tool: "Dell Power Manager",
            steps: "Open Dell Power Manager → Battery Information → Charging configuration and \
                    set a charge limit; on business machines, set Battery Health Manager in the \
                    BIOS (F10 at boot) instead",
            url: None,
        }),
        adaptive_guidance: None,
    }
}
