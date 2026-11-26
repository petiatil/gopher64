#![deny(warnings)]

mod cheats;
mod device;
mod netplay;
mod retroachievements;
mod savestates;
mod ui;
use clap::Parser;
#[cfg(target_os = "android")]
use slint::ComponentHandle;
use std::io::Error;

#[cfg(target_os = "android")]
use ui::android;

/// N64 emulator
#[derive(Parser, Debug)]
#[command(author, version=env!("GIT_DESCRIBE"), about, long_about = None, arg_required_else_help = if cfg!(feature = "gui") { false } else { true })]
pub struct Args {
    pub game: Option<String>,
    #[arg(short, long)]
    pub fullscreen: bool,
    #[arg(long)]
    pub widescreen: bool,
    #[arg(long)]
    pub overclock: Option<bool>,
    #[arg(long)]
    pub disable_expansion_pak: Option<bool>,
    #[arg(long, value_name = "CHEATS_FILE", hide = true)]
    pub cheats: Option<String>,
    #[arg(long, value_name = "NETPLAY_SERVER_ADDR", hide = true)]
    pub netplay_server_addr: Option<String>,
    #[arg(
        long,
        value_name = "NETPLAY_PLAYER_NUMBER",
        value_delimiter = ',',
        hide = true
    )]
    pub netplay_player_number: Vec<usize>,
    #[arg(long, value_name = "NETPLAY_NUMBER_OF_PLAYERS", hide = true)]
    pub netplay_number_of_players: Option<usize>,
    #[arg(long, value_name = "NETPLAY_INPUT_DELAY", hide = true)]
    pub netplay_input_delay: Option<usize>,
    #[arg(
        long,
        conflicts_with_all = ["netplay_join", "netplay_server_addr"],
        help = "Host a netplay game that other computers join with --netplay-join"
    )]
    pub netplay_host: bool,
    #[arg(
        long,
        value_name = "ADDRESS",
        conflicts_with = "netplay_server_addr",
        help = "Join the netplay game hosted on the computer at this address"
    )]
    pub netplay_join: Option<String>,
    #[arg(
        long,
        value_name = "COUNT",
        help = "Used with --netplay-host and --netplay-join: how many people play on this computer (default 1)"
    )]
    pub netplay_local_players: Option<usize>,
    #[arg(
        long,
        value_name = "COUNT",
        help = "Used with --netplay-host: how many people play in total (default: one more than on this computer)"
    )]
    pub netplay_total_players: Option<usize>,
    #[arg(
        long,
        value_name = "PROFILE_NAME",
        help = "Create a new input profile (keyboard/gamepad mappings)"
    )]
    pub configure_input_profile: Option<String>,
    #[arg(long, help = "Use DirectInput when configuring a new input profile")]
    pub use_dinput: bool,
    #[arg(
        long,
        value_name = "DEADZONE_PERCENTAGE",
        help = "Used along with --configure-input-profile to set the deadzone for analog sticks"
    )]
    pub deadzone: Option<i32>,
    #[arg(
        long,
        value_name = "PROFILE_NAME",
        help = "Must also specify --port. Used to bind a previously created profile to a port"
    )]
    pub bind_input_profile: Option<String>,
    #[arg(
        long,
        help = "Lists connected controllers which can be used in --assign-controller"
    )]
    pub list_controllers: bool,
    #[arg(
        long,
        value_name = "CONTROLLER_NUMBER",
        help = "Must also specify --port. Used to assign a controller listed in --list-controllers to a port"
    )]
    pub assign_controller: Option<i32>,
    #[arg(
        long,
        value_name = "PORT",
        help = "Valid values: 1-4. To be used alongside --bind-input-profile and --assign-controller"
    )]
    pub port: Option<usize>,
    #[arg(
        long,
        help = "Clear all input profile bindings and controller assignments"
    )]
    pub clear_input_bindings: bool,
    #[arg(
        long,
        value_name = "SLOT",
        help = "Load savestate from slot 0-9 when starting the game"
    )]
    pub load_state: Option<u32>,
    #[arg(
        long = "ra-username",
        value_name = "USERNAME",
        help = "Username for RetroAchievements"
    )]
    pub ra_username: Option<String>,
    #[arg(
        long = "ra-password",
        value_name = "PASSWORD",
        help = "Password for RetroAchievements"
    )]
    pub ra_password: Option<String>,
}

#[cfg(feature = "gui")]
fn set_app_id() {
    if let Err(e) = slint::set_xdg_app_id(ui::APP_ID) {
        eprintln!("Could not set application ID: {}", e);
    }
}

pub fn create_runtime() -> (tokio::sync::oneshot::Sender<()>, tokio::runtime::Handle) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (close_tx, close_rx) = tokio::sync::oneshot::channel::<()>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to build runtime");
        tx.send(rt.handle().clone()).unwrap();
        rt.block_on(close_rx).unwrap();
    });

    (close_tx, rx.blocking_recv().unwrap())
}

/// The port of the connection server that a netplay host runs.
const DIRECT_NETPLAY_PORT: u16 = 3536;
const DIRECT_NETPLAY_INPUT_DELAY: usize = 2;

/// Turns what a player types to join a host (an IP address or a name, with or
/// without a port, or a complete URL) into the URL of its connection server.
fn netplay_server_url(address: &str) -> String {
    let address = address.trim().trim_end_matches('/');
    if address.starts_with("ws://") || address.starts_with("wss://") {
        return address.to_string();
    }
    let has_port = address
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.contains(':') && port.parse::<u16>().is_ok());
    if has_port {
        format!("ws://{address}/gopher64")
    } else {
        format!("ws://{address}:{DIRECT_NETPLAY_PORT}/gopher64")
    }
}

/// Runs the connection server through which the clients of a hosted netplay
/// game find each other. It only introduces them; the game traffic goes
/// directly between the clients.
fn start_netplay_server() -> std::io::Result<()> {
    let address =
        std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, DIRECT_NETPLAY_PORT));
    let mut server = matchbox_signaling::SignalingServer::full_mesh_builder(address).build();
    match server.bind() {
        Ok(_) => {
            tokio::spawn(async move {
                if let Err(e) = server.serve().await {
                    eprintln!("Netplay connection server stopped: {e}");
                }
            });
            Ok(())
        }
        // carrying on would connect to whatever holds the port instead
        Err(e) => Err(Error::other(format!(
            "Could not host: port {DIRECT_NETPLAY_PORT} is in use, maybe by another copy of the game that is hosting ({e})"
        ))),
    }
}

pub fn run(mut args: Args, arg_count: usize) -> std::io::Result<()> {
    let dirs = ui::get_dirs();

    std::fs::create_dir_all(&dirs.config_dir)?;
    std::fs::create_dir_all(&dirs.cache_dir)?;
    std::fs::create_dir_all(dirs.data_dir.join("saves"))?;
    std::fs::create_dir_all(dirs.data_dir.join("states"))?;

    ui::sdl_hints();

    if let Some(number_of_players) = args.netplay_number_of_players
        && !args.netplay_player_number.is_empty()
    {
        let mut local_players = args.netplay_player_number.clone();
        local_players.sort_unstable();
        local_players.dedup();
        if number_of_players > 4 {
            return Err(Error::other("Netplay supports at most 4 players"));
        }
        if local_players.iter().any(|slot| *slot >= number_of_players) {
            return Err(Error::other(
                "Netplay player numbers must be lower than the number of players",
            ));
        }
        if local_players.len() >= number_of_players {
            return Err(Error::other(
                "At least one netplay player must be on another client",
            ));
        }
    }

    // hosting or joining: the player slots are worked out between the clients
    let auto_role = if args.netplay_host {
        Some(netplay::AutoRole::Host)
    } else if args.netplay_join.is_some() {
        Some(netplay::AutoRole::Join)
    } else {
        None
    };
    let local_player_count = args.netplay_local_players.unwrap_or(1);
    if let Some(role) = &auto_role {
        if !(1..=3).contains(&local_player_count) {
            return Err(Error::other(
                "Between 1 and 3 people can play netplay on one computer",
            ));
        }
        if *role == netplay::AutoRole::Host {
            let total = args.netplay_total_players.unwrap_or(local_player_count + 1);
            if total > 4 || total <= local_player_count {
                return Err(Error::other(
                    "The total number of netplay players must be at most 4 and include at least one player on another computer",
                ));
            }
            args.netplay_server_addr =
                Some(format!("ws://127.0.0.1:{DIRECT_NETPLAY_PORT}/gopher64"));
            args.netplay_player_number = (0..local_player_count).collect();
            args.netplay_number_of_players = Some(total);
        } else {
            args.netplay_server_addr = args.netplay_join.as_deref().map(netplay_server_url);
            // placeholders, the host decides both
            args.netplay_player_number = (0..local_player_count).collect();
            args.netplay_number_of_players = Some(0);
        }
        args.netplay_input_delay
            .get_or_insert(DIRECT_NETPLAY_INPUT_DELAY);
        // these must be the same on every client, so use the defaults
        args.overclock = Some(false);
        args.disable_expansion_pak = Some(false);
    }

    if let Some(game) = args.game {
        let file_path = std::path::Path::new(&game).to_path_buf();
        let Some(rom_contents) = device::get_rom_contents(&file_path) else {
            return Err(Error::other(format!(
                "Could not read ROM file: {}",
                file_path.display()
            )));
        };

        let game_id = ui::storage::get_game_crc(&rom_contents);
        let cheats = if auto_role.is_some() {
            // cheats must also be the same on every client
            Default::default()
        } else if let Some(cheats_file) = args.cheats {
            if let Ok(data) = std::fs::read(cheats_file)
                && let Ok(cheats) = serde_json::from_slice(&data)
            {
                cheats
            } else {
                return Err(Error::other("Could not read cheats file"));
            }
        } else {
            ui::config::Cheats::new()
                .cheats
                .get(&game_id)
                .cloned()
                .unwrap_or_default()
        };

        if let Some(slot) = args.load_state
            && slot > 9
        {
            return Err(Error::other("Savestate slot must be between 0 and 9"));
        }

        if args.netplay_host {
            start_netplay_server()?;
        }

        let mut device = device::Device::new(true);
        let configured_ports = device.ui.config.input.controller_enabled;

        if args.netplay_server_addr.is_some() {
            let mut local_players = args.netplay_player_number.clone();
            local_players.sort_unstable();
            local_players.dedup();
            if auto_role.is_some() {
                // the player said how many people play here, so there is no
                // point in refusing to start over a port that is switched off
                let enabled = &mut device.ui.config.input.controller_enabled;
                enabled[..local_player_count].fill(true);
            }
            // the first local player has always used port 1, whatever its settings
            let enabled = device.ui.config.input.controller_enabled;
            if let Some(port) = (1..local_players.len()).find(|port| !enabled[*port]) {
                return Err(Error::other(format!(
                    "Controller port {} must be enabled in the controller settings to play {} netplay players on this client",
                    port + 1,
                    local_players.len()
                )));
            }
        }

        device.ui.config.ui.recent_roms.retain(|x| *x != game);
        device.ui.config.ui.recent_roms.insert(0, game);
        device.ui.config.ui.recent_roms.truncate(5);

        if args.fullscreen {
            device.ui.video.fullscreen = true;
        } else {
            device.ui.video.fullscreen = device.ui.config.video.fullscreen;
        }

        if args.widescreen {
            device.ui.video.widescreen = true;
        } else {
            device.ui.video.widescreen = device.ui.config.video.widescreen;
        }

        let overclock = args
            .overclock
            .unwrap_or(device.ui.config.emulation.overclock);
        let disable_expansion_pak = args
            .disable_expansion_pak
            .unwrap_or(device.ui.config.emulation.disable_expansion_pak);

        let mut shutdown_tx = None;
        let mut usb_handle = None;

        if args.netplay_server_addr.is_none() {
            for i in 0..4 {
                if device.ui.config.input.transfer_pak[i]
                    && !device.ui.config.input.gb_rom_path[i].is_empty()
                    && !device.ui.config.input.gb_ram_path[i].is_empty()
                    && let Some(rom) = device::controller::gbcart::read_file(
                        &device.ui.config.input.gb_rom_path[i],
                    )
                    && let Some(ram) = device::controller::gbcart::read_file(
                        &device.ui.config.input.gb_ram_path[i],
                    )
                {
                    device::controller::gbcart::init(&mut device.transferpaks[i].cart, &rom, &ram);
                }
            }

            if device.ui.config.emulation.usb {
                (shutdown_tx, usb_handle, device.ui.usb) = ui::usb::init();
            }
        }

        let file_path = dirs.config_dir.join("retroachievements.json");
        let ra_config = if let ra_config = std::fs::read(&file_path).unwrap_or_default()
            && let ra_config =
                serde_json::from_slice::<retroachievements::RAConfig>(ra_config.as_ref())
                    .unwrap_or_default()
            && (ra_config.enabled || args.ra_username.is_some())
        {
            let username = args.ra_username.unwrap_or(ra_config.username.clone());
            retroachievements::init_client(
                if cfg!(ra_hardcore_enabled) && args.netplay_server_addr.is_none() {
                    ra_config.hardcore
                } else {
                    false
                },
                ra_config.challenge,
                ra_config.leaderboard,
            );

            let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
            if let Some(password) = args.ra_password {
                retroachievements::login_user(username, password, tx);
            } else if !ra_config.token.is_empty() {
                retroachievements::login_token_user(username, ra_config.token.clone(), tx);
            } else {
                tx.send(false).unwrap();
            }

            rx.blocking_recv().unwrap();

            ra_config
        } else {
            retroachievements::RAConfig::default()
        };

        let netplay_config = if let Some(server_addr) = args.netplay_server_addr
            && !args.netplay_player_number.is_empty()
            && let Some(number_of_players) = args.netplay_number_of_players
            && let Some(input_delay) = args.netplay_input_delay
        {
            Some(netplay::NetplayConfig {
                server_addr,
                local_players: args.netplay_player_number,
                number_of_players,
                input_delay,
                ice_config_path: dirs.cache_dir.join("ice_config.json"),
                auto_slots: auto_role.map(|role| netplay::AutoSlots {
                    role,
                    local_player_count,
                    game_id,
                }),
            })
        } else {
            None
        };

        device::run_game(
            &mut device,
            &rom_contents,
            ui::GameSettings {
                overclock,
                disable_expansion_pak,
                cheats,
                load_savestate_slot: args.load_state,
            },
            ra_config,
            netplay_config,
        );
        // ports enabled just for a hosted or joined game are not a setting
        device.ui.config.input.controller_enabled = configured_ports;

        // on Android, the client is shut down in the app_window function
        #[cfg(not(target_os = "android"))]
        retroachievements::shutdown_client();

        if device.netplay.is_none() {
            for i in 0..4 {
                if device.ui.config.input.transfer_pak[i]
                    && !device.ui.config.input.gb_ram_path[i].is_empty()
                    && !device.transferpaks[i].cart.ram.is_empty()
                {
                    device::controller::gbcart::save(
                        &mut device.transferpaks[i].cart,
                        device.vi.elapsed_time as i64,
                        &device.ui.config.input.gb_ram_path[i],
                    );
                }
            }
        }

        ui::usb::close(shutdown_tx, usb_handle);
    } else if arg_count > 1 {
        let mut config = ui::config::Config::new();

        if let Some(profile) = args.configure_input_profile {
            ui::input_profile::configure_input_profile(
                &mut config,
                profile,
                args.use_dinput,
                args.deadzone.unwrap_or(ui::input_profile::DEADZONE_DEFAULT),
            );

            ui::sdl_close();
            return Ok(());
        } else {
            if args.clear_input_bindings {
                ui::input::clear_bindings(&mut config);
                return Ok(());
            }
            if let Some(port) = args.port
                && !(1..=4).contains(&port)
            {
                return Err(Error::other("Port must be between 1 and 4"));
            }

            ui::sdl_init(sdl3_sys::init::SDL_INIT_GAMEPAD);

            if args.list_controllers {
                let controllers = ui::input::get_controller_names();
                for (i, controller) in controllers.iter().enumerate() {
                    println!("Controller {i}: {controller}");
                }
            } else {
                if let Some(assign_controller) = args.assign_controller {
                    let Some(port) = args.port else {
                        ui::sdl_close();
                        return Err(Error::other("Must specify port number"));
                    };
                    ui::input::assign_controller(&mut config, assign_controller - 1, port);
                }
                if let Some(profile) = args.bind_input_profile {
                    let Some(port) = args.port else {
                        ui::sdl_close();
                        return Err(Error::other("Must specify port number"));
                    };
                    ui::input::bind_input_profile(&mut config, profile, port);
                }
            }
        }
    } else {
        #[cfg(feature = "gui")]
        {
            let app = ui::gui::AppWindow::new().unwrap();
            let no_intro_map =
                std::sync::Arc::new(tokio::sync::Mutex::new(rustc_hash::FxHashMap::default()));
            set_app_id();
            ui::gui::app_window(&app, false, no_intro_map);
        }
    }

    ui::sdl_close();
    Ok(())
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    let (close_tx, handle) = create_runtime();
    let _guard = handle.enter();

    slint::android::init_with_event_listener(app.clone(), move |event| match event {
        slint::android::android_activity::PollEvent::Main(main_event) => match main_event {
            slint::android::android_activity::MainEvent::TerminateWindow { .. } => {
                if let Ok(weak_app_window) = android::WEAK_SLINT_WINDOW.lock()
                    && let Some(weak_app_window) = weak_app_window.as_ref()
                {
                    weak_app_window
                        .upgrade_in_event_loop(|handle| ui::gui::save_settings(&handle))
                        .unwrap();
                }
            }
            _ => {}
        },
        _ => {}
    })
    .unwrap();
    let app_window = ui::gui::AppWindow::new().unwrap();
    *android::WEAK_SLINT_WINDOW.lock().unwrap() = Some(app_window.as_weak());

    *android::ANDROID_APP.lock().unwrap() = Some(app);

    let dirs = ui::get_dirs();

    std::fs::create_dir_all(&dirs.config_dir).unwrap();
    std::fs::create_dir_all(&dirs.cache_dir).unwrap();
    std::fs::create_dir_all(dirs.data_dir.join("saves")).unwrap();
    std::fs::create_dir_all(dirs.data_dir.join("states")).unwrap();

    let no_intro_map =
        std::sync::Arc::new(tokio::sync::Mutex::new(rustc_hash::FxHashMap::default()));
    ui::gui::app_window(&app_window, true, no_intro_map);
    close_tx.send(()).unwrap();
    *android::WEAK_SLINT_WINDOW.lock().unwrap() = None;
    *android::ANDROID_APP.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_address_becomes_a_server_url() {
        assert_eq!(
            netplay_server_url("100.101.254.114"),
            "ws://100.101.254.114:3536/gopher64"
        );
        assert_eq!(
            netplay_server_url(" my-mac.tailnet.ts.net "),
            "ws://my-mac.tailnet.ts.net:3536/gopher64"
        );
        assert_eq!(
            netplay_server_url("10.0.0.5:4000"),
            "ws://10.0.0.5:4000/gopher64"
        );
        assert_eq!(
            netplay_server_url("ws://10.0.0.5:3536/room"),
            "ws://10.0.0.5:3536/room"
        );
        assert_eq!(
            netplay_server_url("wss://example.com/room/"),
            "wss://example.com/room"
        );
    }
}
