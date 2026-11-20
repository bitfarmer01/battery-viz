//! ASUS maker module: MyASUS Battery Health Charging offers fixed modes —
//! "Best for battery" (60%) and "Balanced" (80%) — against "Maximum" (full).
//! State read-back is unverified (roadmap open question), so detection
//! stays behavioral (FR-1).

use super::{CapOptions, Maker, MakerModule, SettingGuidance};

pub fn module() -> MakerModule {
    MakerModule {
        maker: Maker::Asus,
        cap_options: CapOptions {
            fixed: &[60, 80],
            range: None,
            adaptive: false,
        },
        cap_guidance: Some(SettingGuidance {
            tool: "MyASUS",
            steps: "Open MyASUS → Hardware settings → Battery Health Charging and pick Best for \
                    battery (60%) or Balanced (80%)",
            url: None,
        }),
        adaptive_guidance: None,
    }
}
