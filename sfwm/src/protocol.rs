//! Rust bindings for the river protocols, generated at compile time from the
//! vendored XML.
//!
//! Two protocols are generated here: `river-window-management-v1` (the WM core)
//! and `river-xkb-bindings-v1` (keyboard bindings). The latter references
//! `river_seat_v1` from the former, so — following the wayland-rs convention for
//! dependent protocols — each lives in its own module and the xkb module imports
//! the WM module's interfaces into scope.
#![allow(non_upper_case_globals, non_camel_case_types, clippy::all)]

/// river-window-management-v1.
pub mod wm {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/river-window-management-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/river-window-management-v1.xml");
}

/// river-xkb-bindings-v1 (depends on `wm::river_seat_v1`).
pub mod xkb {
    use wayland_client;
    // Bring the WM protocol's interface modules into scope so the generated code's
    // bare references to `river_seat_v1` resolve.
    use super::wm::*;

    pub mod __interfaces {
        // The WM protocol's interface statics (river_seat_v1_interface, …).
        use super::super::wm::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/river-xkb-bindings-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/river-xkb-bindings-v1.xml");
}

// Surface the interface submodules and proxy types at the protocol-module root so
// `use protocol::*` and `protocol::river_seat_v1::…` keep working as before.
pub use wm::{
    river_decoration_v1, river_node_v1, river_output_v1, river_pointer_binding_v1,
    river_seat_v1, river_shell_surface_v1, river_window_manager_v1, river_window_v1,
};
pub use xkb::{river_xkb_binding_v1, river_xkb_bindings_v1};

pub use river_decoration_v1::RiverDecorationV1;
pub use river_node_v1::RiverNodeV1;
pub use river_shell_surface_v1::RiverShellSurfaceV1;
pub use river_output_v1::RiverOutputV1;
pub use river_seat_v1::RiverSeatV1;
pub use river_window_manager_v1::RiverWindowManagerV1;
pub use river_window_v1::RiverWindowV1;

pub use river_pointer_binding_v1::RiverPointerBindingV1;

pub use river_xkb_binding_v1::RiverXkbBindingV1;
pub use river_xkb_bindings_v1::RiverXkbBindingsV1;
