use trafficview_ui_wayland::{physical_size, snap_anchor, Anchor, Backend};

#[test]
fn gnome_never_selects_layer_shell() {
    assert_eq!(
        Backend::choose("wayland", "GNOME", true),
        Backend::XdgFallback
    );
    assert_eq!(
        Backend::choose("wayland", "ubuntu:GNOME", true),
        Backend::XdgFallback
    );
    assert_eq!(Backend::choose("wayland", "KDE", true), Backend::LayerShell);
    assert_eq!(
        Backend::choose("wayland", "sway", true),
        Backend::LayerShell
    );
    assert_eq!(
        Backend::choose("wayland", "Hyprland", false),
        Backend::XdgFallback
    );
    assert_eq!(Backend::choose("x11", "X-Cinnamon", true), Backend::X11);
}

#[test]
fn dragging_snaps_to_eight_pixel_grid() {
    // 右上方拖拽结束点：吸附到 TOP|RIGHT。
    let geometry = snap_anchor([1600, 67], [1920, 1080], [240, 80], 8);
    assert_eq!(geometry.anchor, Anchor::TOP | Anchor::RIGHT);
    assert_eq!(geometry.margin.0 % 8, 0);
    assert_eq!(geometry.margin.1 % 8, 0);
    assert!(geometry.margin.0 >= 0 && geometry.margin.1 >= 0);
}

#[test]
fn anchors_choose_the_nearest_edges() {
    let top_left = snap_anchor([10, 12], [1920, 1080], [240, 80], 8);
    assert_eq!(top_left.anchor, Anchor::TOP | Anchor::LEFT);
    assert_eq!(top_left.margin, (8, 16));

    let bottom_right = snap_anchor([1600, 896], [1920, 1080], [240, 80], 8);
    assert_eq!(bottom_right.anchor, Anchor::BOTTOM | Anchor::RIGHT);
    assert_eq!(bottom_right.margin, (80, 104));
}

#[test]
fn fractional_scales_round_to_physical_pixels() {
    assert_eq!(physical_size([240, 80], 1.0), [240, 80]);
    assert_eq!(physical_size([240, 80], 1.25), [300, 100]);
    assert_eq!(physical_size([240, 80], 1.5), [360, 120]);
    assert_eq!(physical_size([240, 80], 2.0), [480, 160]);
    assert_eq!(physical_size([1, 1], 1.25), [1, 1]);
    assert_eq!(physical_size([240, 80], f64::NAN), [240, 80]);
}
