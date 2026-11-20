//! HP maker module: the BIOS Battery Health Manager ("Maximize my battery
//! health") caps charging at 80%, and "Let HP manage my battery charging"
//! is the adaptive mode. State read-back is not generically available, so
//! detection stays behavioral (roadmap FR-1).

use super::{CapOptions, Maker, MakerModule, SettingGuidance};

pub fn module() -> MakerModule {
    MakerModule {
        maker: Maker::Hp,
        cap_options: CapOptions {
            fixed: &[80],
            range: None,
            adaptive: true,
        },
        cap_guidance: Some(SettingGuidance {
            tool: "HP Battery Health Manager",
            steps: "Restart into the BIOS (F10 at boot), then Advanced → Power Management \
                    Options → Battery Health Manager and pick Maximize my battery health (80% \
                    cap); on consumer models use HP Support Assistant → Battery health",
            url: None,
        }),
        adaptive_guidance: Some(SettingGuidance {
            tool: "HP Battery Health Manager",
            steps: "In Battery Health Manager choose Let HP manage my battery charging",
            url: None,
        }),
    }
}
