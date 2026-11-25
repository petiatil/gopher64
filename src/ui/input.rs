use std::ops::Neg;

use crate::ui;

const X_AXIS_SHIFT: usize = 16;
const Y_AXIS_SHIFT: usize = 24;

const MAX_AXIS_VALUE: f64 = 85.0;

pub const UNKNOWN_CONTROLLER_NAME: &str = "Unknown controller";

/// Stored in `controller_assignment` for a port that must never be given a
/// controller, not even automatically.
const KEYBOARD_ONLY: &str = "keyboard-only";

#[derive(Default)]
pub struct Controllers {
    pub game_controller: *mut sdl3_sys::gamepad::SDL_Gamepad,
    pub joystick: *mut sdl3_sys::joystick::SDL_Joystick,
    pub guid: sdl3_sys::guid::SDL_GUID,
    pub last_key_state: u32,
}

#[derive(Default, PartialEq, Copy, Clone, serde::Serialize, serde::Deserialize)]
pub struct InputData {
    pub data: u32,
    pub pak_change_pressed: bool,
}

fn bound_axis(x: &mut f64, y: &mut f64) {
    let radius = f64::sqrt(70.0 * 70.0 + 70.0 * 70.0); // this is roughly the maximum diagonal distance of the controller

    // Calculate the distance from the origin (0, 0)
    let distance = f64::sqrt((*x) * (*x) + (*y) * (*y));

    // If the distance is greater than the radius, scale the coordinates
    if distance > radius {
        let scale_factor = radius / distance;
        *x *= scale_factor;
        *y *= scale_factor;
    }
}

fn apply_deadzone(x: &mut f64, y: &mut f64, deadzone: i32) {
    let axis_deadzone = MAX_AXIS_VALUE * (deadzone as f64 / 100.0);

    // Calculate the distance from the origin (0, 0)
    let distance = f64::sqrt((*x) * (*x) + (*y) * (*y));

    if distance <= axis_deadzone {
        *x = 0.0;
        *y = 0.0;
        return;
    }

    let new_distance =
        (distance - axis_deadzone) * MAX_AXIS_VALUE / (MAX_AXIS_VALUE - axis_deadzone);
    *x = *x / distance * new_distance;
    *y = *y / distance * new_distance;
}

fn normalize_axis_position(axis_position: i16) -> f64 {
    axis_position as f64 * MAX_AXIS_VALUE / i16::MAX as f64
}

fn set_axis_value(x: &mut f64, y: &mut f64, axis: usize, magnitude: f64) {
    match axis {
        ui::input_profile::AXIS_LEFT => *x = -magnitude,
        ui::input_profile::AXIS_RIGHT => *x = magnitude,
        ui::input_profile::AXIS_DOWN => *y = magnitude,
        ui::input_profile::AXIS_UP => *y = -magnitude,
        _ => unreachable!(),
    }
}

fn set_axis(
    profile: &ui::config::InputProfile,
    joystick: *mut sdl3_sys::joystick::SDL_Joystick,
    controller: *mut sdl3_sys::gamepad::SDL_Gamepad,
    keyboard_state: *const bool,
    use_keyboard: bool,
) -> (f64, f64) {
    let mut x = 0.0;
    let mut y = 0.0;
    let axes = [
        ui::input_profile::AXIS_LEFT,
        ui::input_profile::AXIS_RIGHT,
        ui::input_profile::AXIS_DOWN,
        ui::input_profile::AXIS_UP,
    ];
    let mut has_deadzone = false;
    for axis in axes {
        for input in profile.inputs[axis].iter() {
            if !controller.is_null()
                && let Some(ui::config::InputItem::ControllerAxis(controller_axis)) = input
            {
                let axis_position = unsafe {
                    sdl3_sys::gamepad::SDL_GetGamepadAxis(
                        controller,
                        sdl3_sys::gamepad::SDL_GamepadAxis(controller_axis.id),
                    )
                };
                if axis_position as isize * controller_axis.axis as isize > 0 {
                    set_axis_value(
                        &mut x,
                        &mut y,
                        axis,
                        normalize_axis_position(axis_position).abs(),
                    );
                    has_deadzone = true;
                }
            } else if !joystick.is_null()
                && let Some(ui::config::InputItem::JoystickAxis(joystick_axis)) = input
            {
                let axis_position =
                    unsafe { sdl3_sys::joystick::SDL_GetJoystickAxis(joystick, joystick_axis.id) };
                if axis_position as isize * joystick_axis.axis as isize > 0 {
                    set_axis_value(
                        &mut x,
                        &mut y,
                        axis,
                        normalize_axis_position(axis_position).abs(),
                    );
                    has_deadzone = true;
                }
            } else if let Some(ui::config::InputItem::Key(key)) = input
                && use_keyboard
                && unsafe { *keyboard_state.offset(key.id as isize) }
            {
                set_axis_value(&mut x, &mut y, axis, MAX_AXIS_VALUE);
            }
        }
    }

    y = y.neg();
    if has_deadzone {
        apply_deadzone(&mut x, &mut y, profile.deadzone);
    }
    (x, y)
}

fn is_controller_button_pressed(
    input: &Option<ui::config::InputItem>,
    joystick: *mut sdl3_sys::joystick::SDL_Joystick,
    controller: *mut sdl3_sys::gamepad::SDL_Gamepad,
) -> bool {
    if !controller.is_null() {
        if let Some(ui::config::InputItem::ControllerButton(controller_button)) = input
            && unsafe {
                sdl3_sys::gamepad::SDL_GetGamepadButton(
                    controller,
                    sdl3_sys::gamepad::SDL_GamepadButton(controller_button.id),
                )
            }
        {
            return true;
        }
        if let Some(ui::config::InputItem::ControllerAxis(controller_axis)) = input {
            let axis_position = unsafe {
                sdl3_sys::gamepad::SDL_GetGamepadAxis(
                    controller,
                    sdl3_sys::gamepad::SDL_GamepadAxis(controller_axis.id),
                )
            };
            if axis_position as isize * controller_axis.axis as isize > 0
                && axis_position.saturating_abs() > i16::MAX / 2
            {
                return true;
            }
        }
    } else if !joystick.is_null() {
        if let Some(ui::config::InputItem::JoystickButton(joystick_button)) = input
            && unsafe { sdl3_sys::joystick::SDL_GetJoystickButton(joystick, joystick_button.id) }
        {
            return true;
        }
        if let Some(ui::config::InputItem::JoystickHat(joystick_hat)) = input
            && (unsafe { sdl3_sys::joystick::SDL_GetJoystickHat(joystick, joystick_hat.id) }
                & joystick_hat.direction)
                != 0
        {
            return true;
        }
        if let Some(ui::config::InputItem::JoystickAxis(joystick_axis)) = input {
            let axis_position =
                unsafe { sdl3_sys::joystick::SDL_GetJoystickAxis(joystick, joystick_axis.id) };
            if (axis_position as isize * joystick_axis.axis as isize > 0
                || joystick_axis.initial_state != 0)
                && axis_position.abs_diff(joystick_axis.initial_state) > (u16::MAX / 4)
            {
                return true;
            }
        }
    }
    false
}

fn set_buttons(
    profile: &ui::config::InputProfile,
    joystick: *mut sdl3_sys::joystick::SDL_Joystick,
    controller: *mut sdl3_sys::gamepad::SDL_Gamepad,
    keyboard_state: *const bool,
    use_keyboard: bool,
) -> u32 {
    let mut keys = 0;
    for (i, item) in profile.inputs.iter().enumerate().take(14) {
        for input in item.iter() {
            if let Some(ui::config::InputItem::Key(key)) = input
                && use_keyboard
                && unsafe { *keyboard_state.offset(key.id as isize) }
            {
                keys |= 1 << i;
            }
            if is_controller_button_pressed(input, joystick, controller) {
                keys |= 1 << i;
            }
        }
    }
    keys
}

pub fn set_rumble(ui: &ui::Ui, channel: usize, rumble: u8) {
    let controller = ui.input.controllers[channel].game_controller;
    let joystick = ui.input.controllers[channel].joystick;
    if !controller.is_null() {
        unsafe {
            sdl3_sys::gamepad::SDL_RumbleGamepad(
                controller,
                (rumble & 1) as u16 * u16::MAX,
                (rumble & 1) as u16 * u16::MAX,
                (rumble & 1) as u32 * sdl3_sys::haptic::SDL_HAPTIC_INFINITY,
            )
        };
    } else if !joystick.is_null() {
        unsafe {
            sdl3_sys::joystick::SDL_RumbleJoystick(
                joystick,
                (rumble & 1) as u16 * u16::MAX,
                (rumble & 1) as u16 * u16::MAX,
                (rumble & 1) as u32 * sdl3_sys::haptic::SDL_HAPTIC_INFINITY,
            )
        };
    }
}

fn hotkey_pressed(
    profile: &ui::config::InputProfile,
    joystick: *mut sdl3_sys::joystick::SDL_Joystick,
    controller: *mut sdl3_sys::gamepad::SDL_Gamepad,
) -> bool {
    for input in profile.inputs[ui::input_profile::HOTKEY].iter() {
        if is_controller_button_pressed(input, joystick, controller) {
            return true;
        }
    }
    false
}

pub fn get_controller_names() -> Vec<String> {
    #[cfg(target_os = "android")]
    return ui::android::get_controller_names();

    #[cfg(not(target_os = "android"))]
    {
        let mut controllers: Vec<String> = vec![];

        for joystick in get_joysticks().iter() {
            let name = unsafe { sdl3_sys::joystick::SDL_GetJoystickNameForID(*joystick) };
            controllers.push(if name.is_null() {
                UNKNOWN_CONTROLLER_NAME.to_string()
            } else {
                unsafe { std::ffi::CStr::from_ptr(name).to_str().unwrap() }.to_string()
            });
        }
        // a port without an assigned controller takes the first free gamepad
        controllers.insert(0, "Automatic".into());
        // last, so that the numbers of the controllers stay what they were
        controllers.push("None (keyboard only)".into());

        controllers
    }
}

#[cfg(feature = "gui")]
pub fn get_controller_paths() -> Vec<String> {
    #[cfg(target_os = "android")]
    return ui::android::get_controller_paths();

    #[cfg(not(target_os = "android"))]
    {
        // despite the name, these are the identities stored in
        // `controller_assignment` (see `controller_identity`)
        let joysticks = get_joysticks();
        let mut controller_paths: Vec<String> = joysticks
            .iter()
            .map(|joystick| controller_identity(*joystick, &joysticks))
            .collect();
        controller_paths.insert(0, String::new());
        controller_paths.push(KEYBOARD_ONLY.to_string());

        controller_paths
    }
}

fn handle_joystick_events(ui: &mut ui::Ui) {
    // drain the queue, several controllers can change in the same frame
    let mut connected = false;
    loop {
        let joystick_event = unsafe { ui::video::get_joystick_event() };
        if joystick_event.joystick_id == 0 {
            break;
        }
        if joystick_event.connected {
            connected = true;
            continue;
        }
        let joystick_id = sdl3_sys::joystick::SDL_JoystickID(joystick_event.joystick_id);
        for controller in ui.input.controllers.iter_mut() {
            if !controller.joystick.is_null()
                && controller.joystick
                    == unsafe { sdl3_sys::joystick::SDL_GetJoystickFromID(joystick_id) }
            {
                unsafe { sdl3_sys::joystick::SDL_CloseJoystick(controller.joystick) };
                controller.joystick = std::ptr::null_mut();
            } else if !controller.game_controller.is_null()
                && controller.game_controller
                    == unsafe { sdl3_sys::gamepad::SDL_GetGamepadFromID(joystick_id) }
            {
                unsafe { sdl3_sys::gamepad::SDL_CloseGamepad(controller.game_controller) };
                controller.game_controller = std::ptr::null_mut();
            }
        }
    }
    if connected {
        // Once every removal has freed its port: a controller changing from
        // Bluetooth to USB can show up before its old connection is gone. This
        // also binds a controller that was not connected when the game started.
        bind_controllers(ui);
    }
}

fn handle_hotkeys(keys: u32, last_key_state: u32) {
    let mut event: sdl3_sys::events::SDL_Event = Default::default();
    event.user.r#type = u32::from(sdl3_sys::events::SDL_EVENT_USER);
    if keys & (1 << ui::input_profile::L_TRIG) != 0
        && last_key_state & (1 << ui::input_profile::L_TRIG) == 0
    {
        event.user.code = 1; //save state
        unsafe { sdl3_sys::events::SDL_PushEvent(&mut event) };
    }
    if keys & (1 << ui::input_profile::R_TRIG) != 0
        && last_key_state & (1 << ui::input_profile::R_TRIG) == 0
    {
        event.user.code = 2; //load state
        unsafe { sdl3_sys::events::SDL_PushEvent(&mut event) };
    }
    if keys & (1 << ui::input_profile::START_BUTTON) != 0
        && last_key_state & (1 << ui::input_profile::START_BUTTON) == 0
    {
        event.user.code = 3; //exit game
        unsafe { sdl3_sys::events::SDL_PushEvent(&mut event) };
    }
    if keys & (1 << ui::input_profile::Z_TRIG) != 0
        && last_key_state & (1 << ui::input_profile::Z_TRIG) == 0
    {
        event.user.code = 4; //fast forward
        unsafe { sdl3_sys::events::SDL_PushEvent(&mut event) };
    }
    if keys & (1 << ui::input_profile::L_CBUTTON) != 0
        && last_key_state & (1 << ui::input_profile::L_CBUTTON) == 0
    {
        event.user.code = 5; //load rewind
        unsafe { sdl3_sys::events::SDL_PushEvent(&mut event) };
    }
}

/// Whether `channel` is the port that reads the keyboard keys of its input profile.
///
/// Among the enabled ports bound to that profile it is the lowest one without a
/// controller (player 1 on a controller leaves the keyboard to player 2), or
/// the lowest one if they all have a controller.
///
/// Netplay reads port 1 even when it is disabled; in that unusual setup two
/// local players sharing a profile still share its keys, as they always did.
fn keyboard_owner(
    bindings: &[String; 4],
    enabled: &[bool; 4],
    has_controller: &[bool; 4],
    channel: usize,
) -> bool {
    let shares =
        |port: usize| port == channel || (enabled[port] && bindings[port] == bindings[channel]);
    let owner = (0..4)
        .find(|port| shares(*port) && !has_controller[*port])
        .or_else(|| (0..4).find(|port| shares(*port)));
    owner == Some(channel)
}

pub fn get(ui: &mut ui::Ui, channel: usize) -> InputData {
    handle_joystick_events(ui);

    let profile_name = &ui.config.input.input_profile_binding[channel];
    let Some(profile) = ui.config.input.input_profiles.get(profile_name) else {
        eprintln!("Invalid profile name: {profile_name}");
        return InputData {
            data: 0,
            pak_change_pressed: false,
        };
    };
    let controller = ui.input.controllers[channel].game_controller;
    let joystick = ui.input.controllers[channel].joystick;

    let alt_pressed = unsafe {
        // ignore key presses if ALT is pressed
        *ui.input
            .keyboard_state
            .offset(i32::from(sdl3_sys::scancode::SDL_SCANCODE_LALT) as isize)
            || *ui
                .input
                .keyboard_state
                .offset(i32::from(sdl3_sys::scancode::SDL_SCANCODE_RALT) as isize)
    };

    // Ports sharing an input profile share its keyboard keys, so only one of
    // them reads the keyboard. Otherwise one key press would drive several
    // players at once.
    let has_controller = std::array::from_fn(|port| port_has_controller(ui, port));
    let owns_keyboard = keyboard_owner(
        &ui.config.input.input_profile_binding,
        &ui.config.input.controller_enabled,
        &has_controller,
        channel,
    );

    let mut keys = set_buttons(
        profile,
        joystick,
        controller,
        ui.input.keyboard_state,
        owns_keyboard && !alt_pressed,
    );

    let (mut x, mut y) = set_axis(
        profile,
        joystick,
        controller,
        ui.input.keyboard_state,
        owns_keyboard,
    );

    #[cfg(target_os = "android")]
    {
        if channel == 0
            && controller.is_null()
            && joystick.is_null()
            && let Ok(touch_overlay) = ui::android::TOUCH_OVERLAY.lock()
        {
            x = normalize_axis_position(touch_overlay.x);
            y = normalize_axis_position(touch_overlay.y);
            keys = touch_overlay.buttons;
        }
    }
    bound_axis(&mut x, &mut y);

    keys |= (x.round() as i8 as u8 as u32) << X_AXIS_SHIFT;
    keys |= (y.round() as i8 as u8 as u32) << Y_AXIS_SHIFT;

    let last_key_state = ui.input.controllers[channel].last_key_state;
    ui.input.controllers[channel].last_key_state = keys;

    if hotkey_pressed(profile, joystick, controller) {
        handle_hotkeys(keys, last_key_state);
        InputData {
            data: 0,
            pak_change_pressed: keys & (1 << ui::input_profile::B_BUTTON) != 0,
        }
    } else {
        let mut pak_change_pressed = false;
        for input in profile.inputs[ui::input_profile::HOTKEY].iter() {
            if let Some(ui::config::InputItem::Key(key)) = input
                && owns_keyboard
                && unsafe { *ui.input.keyboard_state.offset(key.id as isize) }
            {
                pak_change_pressed = true;
            }
        }
        InputData {
            data: keys,
            pak_change_pressed,
        }
    }
}

fn android_controller_identity(joystick_id: sdl3_sys::joystick::SDL_JoystickID) -> String {
    let vendor_id = unsafe { sdl3_sys::joystick::SDL_GetJoystickVendorForID(joystick_id) };
    let product_id = unsafe { sdl3_sys::joystick::SDL_GetJoystickProductForID(joystick_id) };
    format!("{vendor_id}:{product_id}")
}

/// The identity of a controller sharing its GUID with `earlier_same_guid`
/// controllers listed before it.
fn guid_identity(guid: &[u8; 16], earlier_same_guid: usize) -> String {
    let guid = guid.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!("guid:{guid}:{earlier_same_guid}")
}

/// What `controller_assignment` stores to find a controller again.
///
/// SDL's device path is preferred where it stays the same between sessions. It
/// does not on macOS: GameController and IOKit devices have no path at all, and
/// the path of the others is an IORegistry entry ID that changes every time the
/// controller reconnects. There, and for any other controller without a path,
/// the controller model (GUID) and its position among the connected controllers
/// of that model are used instead. Two controllers of the same model can
/// therefore trade places when they are connected in a different order.
fn controller_identity(
    joystick_id: sdl3_sys::joystick::SDL_JoystickID,
    joysticks: &[sdl3_sys::joystick::SDL_JoystickID],
) -> String {
    if cfg!(target_os = "android") {
        // The Slint GUI enumerates controllers through the Android InputDevice
        // API while the running game sees them through SDL, so the two can
        // disagree on details like the device name. Match on vendor:product,
        // which both agree on.
        return android_controller_identity(joystick_id);
    }
    if !cfg!(target_os = "macos") {
        let path = unsafe { sdl3_sys::joystick::SDL_GetJoystickPathForID(joystick_id) };
        if !path.is_null()
            && let Ok(path) = unsafe { std::ffi::CStr::from_ptr(path) }.to_str()
            && !path.is_empty()
        {
            return path.to_string();
        }
    }
    let guid = unsafe { sdl3_sys::joystick::SDL_GetJoystickGUIDForID(joystick_id) };
    let earlier_same_guid = joysticks
        .iter()
        .take_while(|other| **other != joystick_id)
        .filter(|other| {
            unsafe { sdl3_sys::joystick::SDL_GetJoystickGUIDForID(**other) }.data == guid.data
        })
        .count();
    guid_identity(&guid.data, earlier_same_guid)
}

fn joystick_available(joystick_id: sdl3_sys::joystick::SDL_JoystickID) -> bool {
    unsafe { sdl3_sys::joystick::SDL_GetJoystickFromID(joystick_id) }.is_null()
        && unsafe { sdl3_sys::gamepad::SDL_GetGamepadFromID(joystick_id) }.is_null()
}

pub fn assign_controller(config: &mut ui::config::Config, controller: i32, port: usize) {
    let joysticks = get_joysticks();
    if controller < 0 {
        config.input.controller_assignment[port - 1] = None;
    } else if controller < joysticks.len() as i32 {
        config.input.controller_assignment[port - 1] = Some(controller_identity(
            joysticks[controller as usize],
            &joysticks,
        ));
    } else if controller == joysticks.len() as i32 {
        // listed after the controllers, see `get_controller_names`
        config.input.controller_assignment[port - 1] = Some(KEYBOARD_ONLY.to_string());
    } else {
        eprintln!("Invalid controller number")
    }
}

pub fn bind_input_profile(config: &mut ui::config::Config, profile: String, port: usize) {
    if config.input.input_profiles.contains_key(&profile) {
        config.input.input_profile_binding[port - 1] = profile;
    } else {
        eprintln!("Invalid profile name")
    }
}

pub fn clear_bindings(config: &mut ui::config::Config) {
    for i in 0..4 {
        config.input.controller_assignment[i] = None;
        config.input.input_profile_binding[i] = "default".to_string();
    }
}

pub fn get_joysticks() -> Vec<sdl3_sys::joystick::SDL_JoystickID> {
    unsafe { sdl3_sys::events::SDL_PumpEvents() };
    let mut num_joysticks = 0;
    let sdl_joysticks = unsafe { sdl3_sys::joystick::SDL_GetJoysticks(&mut num_joysticks) };
    if !sdl_joysticks.is_null() {
        let parts =
            unsafe { std::slice::from_raw_parts(sdl_joysticks, num_joysticks as usize) }.to_vec();
        unsafe { sdl3_sys::stdinc::SDL_free(sdl_joysticks as *mut std::ffi::c_void) };
        parts
    } else {
        eprintln!("Could not get joysticks");
        vec![]
    }
}

fn port_has_controller(ui: &ui::Ui, port: usize) -> bool {
    !ui.input.controllers[port].game_controller.is_null()
        || !ui.input.controllers[port].joystick.is_null()
}

fn open_controller(ui: &mut ui::Ui, port: usize, joystick_id: sdl3_sys::joystick::SDL_JoystickID) {
    let Some(profile) = ui
        .config
        .input
        .input_profiles
        .get(&ui.config.input.input_profile_binding[port])
    else {
        eprintln!(
            "Could not bind controller, invalid profile on port {}",
            port + 1
        );
        return;
    };
    if !profile.dinput {
        let gamepad = unsafe { sdl3_sys::gamepad::SDL_OpenGamepad(joystick_id) };
        if gamepad.is_null() {
            eprintln!("could not connect gamepad: {}", u32::from(joystick_id))
        } else {
            ui.input.controllers[port].game_controller = gamepad;
            ui.input.controllers[port].guid =
                unsafe { sdl3_sys::gamepad::SDL_GetGamepadGUIDForID(joystick_id) };
        }
    } else {
        let joystick = unsafe { sdl3_sys::joystick::SDL_OpenJoystick(joystick_id) };
        if joystick.is_null() {
            eprintln!("could not connect joystick: {}", u32::from(joystick_id))
        } else {
            ui.input.controllers[port].joystick = joystick;
            ui.input.controllers[port].guid =
                unsafe { sdl3_sys::joystick::SDL_GetJoystickGUIDForID(joystick_id) };
        }
    }
}

/// A connected controller, as seen when choosing which port gets it.
#[derive(Clone)]
struct Candidate {
    identity: String,
    guid: [u8; 16],
    gamepad: bool,
}

/// What is known about a port when choosing its controller.
#[derive(Clone, Default)]
struct PortRequest {
    /// the port is enabled and has no controller yet
    wanted: bool,
    assignment: Option<String>,
    /// GUID of the controller this port used earlier in this session
    last_guid: Option<[u8; 16]>,
    /// the port may take a gamepad nobody asked for
    automatic: bool,
}

/// Decides which candidate (by index) each port opens.
///
/// Controllers go to the port they are assigned to first, then back to the port
/// that lost them (a reconnect), and finally any gamepad left over goes to a
/// port that still has none, lowest port first. That last step is what makes a
/// controller work without visiting the settings, and what rescues a port whose
/// assigned controller is gone.
fn choose_controllers(ports: &[PortRequest; 4], candidates: &[Candidate]) -> [Option<usize>; 4] {
    let mut chosen: [Option<usize>; 4] = [None; 4];
    let mut taken = vec![false; candidates.len()];

    let mut pass = |matches: &dyn Fn(&PortRequest, &Candidate) -> bool| {
        for (port, request) in ports.iter().enumerate() {
            if !request.wanted || chosen[port].is_some() {
                continue;
            }
            if let Some(index) =
                (0..candidates.len()).find(|i| !taken[*i] && matches(request, &candidates[*i]))
            {
                taken[index] = true;
                chosen[port] = Some(index);
            }
        }
    };

    pass(&|request, candidate| request.assignment.as_deref() == Some(candidate.identity.as_str()));
    pass(&|request, candidate| request.last_guid == Some(candidate.guid));
    pass(&|request, candidate| request.automatic && candidate.gamepad);
    chosen
}

/// Opens a controller for every enabled port that has none, if one is available.
fn bind_controllers(ui: &mut ui::Ui) {
    let joysticks = get_joysticks();
    let available: Vec<sdl3_sys::joystick::SDL_JoystickID> = joysticks
        .iter()
        .copied()
        .filter(|joystick| joystick_available(*joystick))
        .collect();
    let candidates: Vec<Candidate> = available
        .iter()
        .map(|joystick| Candidate {
            identity: controller_identity(*joystick, &joysticks),
            guid: unsafe { sdl3_sys::joystick::SDL_GetJoystickGUIDForID(*joystick) }.data,
            gamepad: unsafe { sdl3_sys::gamepad::SDL_IsGamepad(*joystick) },
        })
        .collect();

    let mut ports: [PortRequest; 4] = Default::default();
    for (port, request) in ports.iter_mut().enumerate() {
        let profile = ui
            .config
            .input
            .input_profiles
            .get(&ui.config.input.input_profile_binding[port]);
        let last_guid = ui.input.controllers[port].guid.data;
        let assignment = ui.config.input.controller_assignment[port].clone();
        let keyboard_only = assignment.as_deref() == Some(KEYBOARD_ONLY);
        *request = PortRequest {
            wanted: ui.config.input.controller_enabled[port]
                && !port_has_controller(ui, port)
                && !keyboard_only,
            assignment,
            last_guid: (last_guid != [0; 16]).then_some(last_guid),
            // DirectInput profiles are mapped for one specific device, and on
            // Android a port without a controller uses the touch overlay
            automatic: !cfg!(target_os = "android") && profile.is_some_and(|p| !p.dinput),
        };
    }

    let chosen = choose_controllers(&ports, &candidates);
    for (port, index) in chosen.iter().enumerate() {
        if let Some(index) = index {
            open_controller(ui, port, available[*index]);
        }
    }
}

pub fn init(ui: &mut ui::Ui) {
    ui::sdl_init(sdl3_sys::init::SDL_INIT_GAMEPAD);

    ui.input.keyboard_state =
        unsafe { sdl3_sys::keyboard::SDL_GetKeyboardState(std::ptr::null_mut()) };
    if ui.input.keyboard_state.is_null() {
        panic!("Could not get keyboard state");
    }

    bind_controllers(ui);

    for port in 0..4 {
        if ui.config.input.controller_enabled[port]
            && ui.config.input.controller_assignment[port]
                .as_deref()
                .is_some_and(|assignment| assignment != KEYBOARD_ONLY)
            && !port_has_controller(ui, port)
        {
            // it is bound as soon as it connects
            eprintln!(
                "The controller assigned to port {} is not connected",
                port + 1
            );
        }
    }
}

pub fn close(ui: &mut ui::Ui) {
    for controller in ui.input.controllers.iter_mut() {
        if !controller.joystick.is_null() {
            unsafe { sdl3_sys::joystick::SDL_CloseJoystick(controller.joystick) }
            controller.joystick = std::ptr::null_mut();
        }
        if !controller.game_controller.is_null() {
            unsafe { sdl3_sys::gamepad::SDL_CloseGamepad(controller.game_controller) }
            controller.game_controller = std::ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(identity: &str, guid: u8) -> Candidate {
        Candidate {
            identity: identity.to_string(),
            guid: [guid; 16],
            gamepad: true,
        }
    }

    fn port(assignment: Option<&str>) -> PortRequest {
        PortRequest {
            wanted: true,
            assignment: assignment.map(str::to_string),
            last_guid: None,
            automatic: true,
        }
    }

    fn unused() -> PortRequest {
        PortRequest::default()
    }

    #[test]
    fn unassigned_ports_take_gamepads_in_order() {
        let ports = [port(None), port(None), unused(), unused()];
        let chosen = choose_controllers(&ports, &[pad("a", 1), pad("b", 2), pad("c", 3)]);
        assert_eq!(chosen, [Some(0), Some(1), None, None]);
    }

    #[test]
    fn assignment_wins_over_automatic() {
        // player 1 on the keyboard, the only gamepad assigned to player 2
        let ports = [port(None), port(Some("a")), unused(), unused()];
        let chosen = choose_controllers(&ports, &[pad("a", 1)]);
        assert_eq!(chosen, [None, Some(0), None, None]);
    }

    #[test]
    fn missing_assigned_controller_falls_back_to_a_free_gamepad() {
        let ports = [port(Some("gone")), unused(), unused(), unused()];
        let chosen = choose_controllers(&ports, &[pad("a", 1)]);
        assert_eq!(chosen, [Some(0), None, None, None]);
    }

    #[test]
    fn reconnecting_controller_returns_to_its_port() {
        let mut second = port(None);
        second.last_guid = Some([2; 16]);
        let ports = [port(None), second, unused(), unused()];
        let chosen = choose_controllers(&ports, &[pad("b", 2)]);
        assert_eq!(chosen, [None, Some(0), None, None]);
    }

    #[test]
    fn non_gamepads_and_dinput_ports_are_never_automatic() {
        let mut joystick = pad("a", 1);
        joystick.gamepad = false;
        let ports = [port(None), unused(), unused(), unused()];
        assert_eq!(choose_controllers(&ports, &[joystick.clone()]), [None; 4]);
        // it still binds when assigned
        let ports = [port(Some("a")), unused(), unused(), unused()];
        assert_eq!(choose_controllers(&ports, &[joystick])[0], Some(0));

        let mut dinput = port(None);
        dinput.automatic = false;
        let ports = [dinput, unused(), unused(), unused()];
        assert_eq!(choose_controllers(&ports, &[pad("a", 1)]), [None; 4]);
    }

    #[test]
    fn disabled_or_busy_ports_get_nothing() {
        let ports = [unused(), port(None), unused(), unused()];
        let chosen = choose_controllers(&ports, &[pad("a", 1)]);
        assert_eq!(chosen, [None, Some(0), None, None]);
    }

    #[test]
    fn identical_controllers_get_distinct_identities() {
        assert_ne!(guid_identity(&[7; 16], 0), guid_identity(&[7; 16], 1));
        assert_eq!(
            guid_identity(&[0xab; 16], 1),
            format!("guid:{}:1", "ab".repeat(16))
        );
    }

    #[test]
    fn one_port_per_profile_reads_the_keyboard() {
        let bindings = [
            "default".to_string(),
            "default".to_string(),
            "p3".to_string(),
            "default".to_string(),
        ];
        let owners = |enabled: [bool; 4], has_controller: [bool; 4]| -> Vec<usize> {
            (0..4)
                .filter(|port| keyboard_owner(&bindings, &enabled, &has_controller, *port))
                .collect()
        };
        // nobody has a controller: the first port of each profile
        assert_eq!(owners([true; 4], [false; 4]), [0, 2]);
        // player 1 on a controller leaves the keyboard to player 2
        assert_eq!(owners([true; 4], [true, false, false, false]), [1, 2]);
        // everybody on a controller: back to the first port
        assert_eq!(owners([true; 4], [true; 4]), [0, 2]);
        // a disabled port does not claim the keyboard
        assert!(keyboard_owner(
            &bindings,
            &[false, true, false, false],
            &[false; 4],
            1
        ));
        // single player always has the keyboard
        assert!(keyboard_owner(
            &bindings,
            &[true, false, false, false],
            &[true, false, false, false],
            0
        ));
    }
}
