// SPDX-License-Identifier: GPL-3.0-only

use super::*;
use cosmic_comp_config::CosmicCompConfig;
use smithay::{
    output::{PhysicalProperties, Subpixel},
    reexports::wayland_server::Display,
};

fn shell_with_outputs() -> (Shell, [Output; 2], Display<State>) {
    let config = CosmicCompConfig::default();
    let theme = cosmic::theme::system_preference();
    let mut shell = Shell {
        workspaces: Workspaces {
            sets: IndexMap::new(),
            backup_set: None,
            layout: config.workspaces.workspace_layout,
            mode: config.workspaces.workspace_mode,
            autotile: config.autotile,
            autotile_behavior: config.autotile_behavior,
            theme: theme.clone(),
            appearance: config.appearance_settings,
            persisted_workspaces: Vec::new(),
        },
        seats: Seats::new(),
        pending_windows: Vec::new(),
        pending_layers: Vec::new(),
        pending_activations: HashMap::new(),
        override_redirect_windows: Vec::new(),
        session_lock: None,
        previous_workspace_idx: None,
        xwayland_keyboard_grab: None,
        theme,
        active_hint: true,
        allow_tearing: true,
        overview_mode: OverviewMode::None,
        swap_indicator: None,
        resize_mode: ResizeMode::None,
        resize_state: None,
        resize_indicator: None,
        zoom_state: None,
        appearance_conf: config.appearance_settings,
        tiling_exceptions: TilingExceptions::new(std::iter::empty()),
        pending_focus_overrides: Vec::new(),
        #[cfg(feature = "debug")]
        debug_active: false,
    };
    let display = Display::<State>::new().unwrap();
    let mut workspace_state = WorkspaceState::<State>::new(&display.handle(), |_| true);
    let outputs = ["game", "desktop"].map(|name| {
        Output::new(
            name.into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: name.into(),
            },
        )
    });
    for output in &outputs {
        shell
            .workspaces
            .add_output(output, &mut workspace_state.update());
        // Consume any initial workspace invalidation before testing animation.
        shell.output_animations_going(output);
    }
    (shell, outputs, display)
}

#[test]
fn workspace_animation_only_redraws_its_output() {
    let (mut shell, [game, desktop], _display) = shell_with_outputs();
    assert!(!shell.output_animations_going(&game));
    assert!(!shell.output_animations_going(&desktop));

    shell
        .workspaces
        .sets
        .get_mut(&desktop)
        .unwrap()
        .previously_active = Some((0, WorkspaceDelta::Shortcut(Instant::now())));

    // Querying the game output must neither see nor consume desktop activity.
    assert!(!shell.output_animations_going(&game));
    assert!(shell.output_animations_going(&desktop));
    assert!(shell.output_animations_going(&desktop));

    shell
        .workspaces
        .sets
        .get_mut(&desktop)
        .unwrap()
        .previously_active = None;
    assert!(!shell.output_animations_going(&desktop));
}

#[test]
fn shared_overview_transitions_still_redraw_both_outputs() {
    let (mut shell, outputs, _display) = shell_with_outputs();
    shell.overview_mode = OverviewMode::Ended(None, Instant::now());
    for output in &outputs {
        assert!(shell.output_animations_going(output));
    }
    shell.overview_mode = OverviewMode::None;
    for output in &outputs {
        assert!(!shell.output_animations_going(output));
    }
}
