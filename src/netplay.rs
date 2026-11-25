use crate::device;
use crate::savestates;
use crate::ui;
use sha2::digest::Digest;

pub const MAX_PREDICTION_WINDOW: usize = 16;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RtcIceServerConfig {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

pub struct GgrsConfig;
impl ggrs::Config for GgrsConfig {
    type Input = ui::input::InputData;
    type InputPredictor = ggrs::PredictRepeatLast;
    type State = i32;
    type Address = matchbox_socket::PeerId;
}

pub struct MatchboxChannel {
    channel: matchbox_socket::WebRtcChannel,
}

impl ggrs::NonBlockingSocket<matchbox_socket::PeerId> for MatchboxChannel {
    fn send_to(&mut self, msg: &ggrs::Message, addr: &matchbox_socket::PeerId) {
        let encoded = postcard::to_stdvec(msg).expect("serialization failed");
        let _ = self.channel.try_send(encoded.into(), *addr);
    }

    fn receive_all_messages(&mut self) -> Vec<(matchbox_socket::PeerId, ggrs::Message)> {
        self.channel
            .receive()
            .iter()
            .filter_map(|(peer, packet)| {
                let msg = postcard::from_bytes::<ggrs::Message>(packet).ok()?;
                Some((*peer, msg))
            })
            .collect()
    }
}

pub struct NetplayConfig {
    pub server_addr: String,
    /// Player slots (0-based) played on this client. More than one means several
    /// people share this machine; the lowest slot uses local controller port 1,
    /// the next one port 2, and so on.
    pub local_players: Vec<usize>,
    pub number_of_players: usize,
    pub input_delay: usize,
    pub ice_config_path: std::path::PathBuf,
}

pub struct Netplay {
    pub disconnected: bool,
    pub session: ggrs::P2PSession<GgrsConfig>,
    pub reliable_channel: matchbox_socket::WebRtcChannel,
    pub peers: Vec<matchbox_socket::PeerId>,
    /// Player slots played on this client, sorted ascending.
    pub local_players: Vec<usize>,
    pub connected: [bool; 4],
    pub input_delay: usize,
    pub messages: std::collections::HashMap<String, Vec<u8>>,
    pub received_data: std::collections::VecDeque<Vec<u8>>,
    pub inputs: Vec<ui::input::InputData>,
    pub requests: std::collections::VecDeque<ggrs::GgrsRequest<GgrsConfig>>,
    frame_progress: FrameProgress,
    pub incoming_message: Vec<u8>,
    pub ice_config_path: std::path::PathBuf,
    /// Diagnostic: see `SYNC_LOG_VARIABLE`.
    sync_log: Option<std::fs::File>,
    /// Diagnostic: see `TEST_INPUT_VARIABLE`.
    test_input: Option<TestInput>,
}

/// Set this environment variable to a number to replace this client's
/// controllers with random button mashing seeded by that number. Together with
/// the sync log it tests that clients stay in sync without anyone playing. The
/// real controllers are not read at all, so their hotkeys do nothing.
const TEST_INPUT_VARIABLE: &str = "GOPHER64_NETPLAY_TEST_INPUT";

struct TestInput {
    state: u64,
    /// per local port: the input being held and for how many more frames
    held: [(u32, u32); 4],
}

impl TestInput {
    fn new(seed: u64) -> Self {
        TestInput {
            // xorshift must not start at zero
            state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
            held: [(0, 0); 4],
        }
    }

    fn random(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    fn next(&mut self, port: usize) -> ui::input::InputData {
        if self.held[port].1 == 0 {
            let random = self.random();
            // each of the 14 buttons is down a quarter of the time
            let mut data = (random & (random >> 14) & 0x3FFF) as u32;
            // stick position, within the range of a real controller
            let x = ((random >> 28) % 161) as i32 - 80;
            let y = ((random >> 36) % 161) as i32 - 80;
            data |= (x as i8 as u8 as u32) << 16;
            data |= (y as i8 as u8 as u32) << 24;
            self.held[port] = (data, 1 + ((random >> 44) % 30) as u32);
        }
        self.held[port].1 -= 1;
        ui::input::InputData {
            data: self.held[port].0,
            pak_change_pressed: false,
        }
    }
}

/// Set this environment variable to a file path to record hashes of the game's
/// memory every `SYNC_LOG_INTERVAL` frames: the frame number, then one hash per
/// `SYNC_LOG_CHUNK` bytes. A frame can be simulated several times (rollback);
/// the last line written for a frame is the final one. Clients that stayed in
/// sync have the same final hashes for every frame, except in the chunks holding
/// the frame and depth buffers: the renderer writes those back when the GPU
/// gets to it. This is a far stronger check than the on-screen desync message.
/// Clients running on the same machine each need their own file.
const SYNC_LOG_VARIABLE: &str = "GOPHER64_NETPLAY_SYNC_LOG";
const SYNC_LOG_INTERVAL: i32 = 60;
const SYNC_LOG_CHUNK: usize = 0x10000;

fn sync_log_line(frame: i32, memory: &[u8]) -> String {
    use std::hash::Hasher;

    let mut line = frame.to_string();
    for chunk in memory.chunks(SYNC_LOG_CHUNK) {
        let mut hasher = rustc_hash::FxHasher::default();
        hasher.write(chunk);
        line.push_str(&format!(" {:016x}", hasher.finish()));
    }
    line
}

fn write_sync_log(log: &mut std::fs::File, frame: i32, memory: &[u8]) {
    use std::io::Write;

    if let Err(e) = writeln!(log, "{}", sync_log_line(frame, memory)) {
        eprintln!("Could not write netplay sync log: {e}");
    }
}

impl Netplay {
    /// The client playing as player 1 hosts the session: it provides the saves,
    /// the RNG seed and the RTC timestamp.
    pub fn is_host(&self) -> bool {
        self.local_players.contains(&0)
    }

    /// The local controller port that drives `slot`, if this client plays it.
    pub fn local_port(&self, slot: usize) -> Option<usize> {
        self.local_players.iter().position(|local| *local == slot)
    }
}

#[derive(Default)]
struct FrameProgress {
    next_frame: i32,
    next_audio_frame: i32,
    replaying: bool,
}

impl FrameProgress {
    fn load(&mut self, frame: i32) {
        self.next_frame = frame;
    }

    fn advance(&mut self) {
        // Audio already submitted to SDL survives a rollback. Only extend its
        // timeline when we simulate a frame beyond the previous frontier.
        self.replaying = self.next_frame < self.next_audio_frame;
        self.next_frame += 1;
        self.next_audio_frame = self.next_audio_frame.max(self.next_frame);
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct NetplayMessage {
    name: String,
    data: Vec<u8>,
}

fn send_message(netplay: &mut Netplay, message: NetplayMessage) {
    let data = postcard::to_stdvec(&message).unwrap();
    let chunks = data.chunks(16384).collect::<Vec<&[u8]>>();
    for peer in netplay.peers.iter() {
        for chunk in chunks.iter() {
            if let Err(e) = netplay
                .reliable_channel
                .try_send(chunk.to_vec().into(), *peer)
            {
                eprintln!("Failed to send message: {}", e);
            }
        }
    }
}

fn process_reliable_messages(netplay: &mut Netplay) {
    netplay.received_data.extend(
        netplay
            .reliable_channel
            .receive()
            .iter()
            .map(|(_, data)| data.to_vec()),
    );

    while !netplay.received_data.is_empty() {
        if let Some(data) = netplay.received_data.pop_front() {
            netplay.incoming_message.extend(data);

            if let Ok(decoded_message) =
                postcard::from_bytes::<NetplayMessage>(&netplay.incoming_message)
            {
                netplay
                    .messages
                    .insert(decoded_message.name, decoded_message.data);
                netplay.incoming_message.clear();
                check_disconnect(netplay);
            }
        }
    }
}

fn receive_message(netplay: &mut Netplay, name: &str) -> Vec<u8> {
    let timeout = std::time::Duration::from_secs(10);
    let now = std::time::Instant::now();

    loop {
        process_reliable_messages(netplay);
        if let Some(data) = netplay.messages.remove(name) {
            return data;
        }

        if now.elapsed() > timeout {
            panic!("Could not receive message for {name}");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

// Handshake message a client sends as soon as it sees a peer, so that every
// client can learn which peer plays which slots.
const MESSAGE_PLAYER_SLOTS: &str = "player_slots";
// Handshake message a client sends once it is connected to the owner of every
// slot. A client with a single player sends what it always has: 8 bytes.
const MESSAGE_PLAYER_NUMBER: &str = "player_number";

// One 8 byte entry per local player.
fn encode_player_numbers(local_players: &[usize]) -> Vec<u8> {
    local_players
        .iter()
        .flat_map(|slot| (*slot as u64).to_be_bytes())
        .collect()
}

// Works out which peer plays which slots. A peer can play several slots, so
// the number of peers is not known in advance.
struct Handshake<P> {
    number_of_players: usize,
    // slot -> owner, from either message. `None` is a slot played on this client.
    known: std::collections::BTreeMap<usize, Option<P>>,
    // slot -> owner, only from peers that are connected to everyone
    confirmed: std::collections::BTreeMap<usize, Option<P>>,
    // peers whose last message asked for a slot that they cannot have
    refused: Vec<P>,
}

impl<P: Copy + PartialEq> Handshake<P> {
    fn new(local_players: &[usize], number_of_players: usize) -> Self {
        let local: std::collections::BTreeMap<usize, Option<P>> =
            local_players.iter().map(|slot| (*slot, None)).collect();
        Self {
            number_of_players,
            known: local.clone(),
            confirmed: local,
            refused: vec![],
        }
    }

    // forget everything learned from peers, after the socket is recreated
    fn reset(&mut self) {
        self.known.retain(|_, peer| peer.is_none());
        self.confirmed.retain(|_, peer| peer.is_none());
        self.refused.clear();
    }

    fn receive(&mut self, peer: P, name: &str, data: &[u8], connected: &[P]) {
        if name != MESSAGE_PLAYER_SLOTS && name != MESSAGE_PLAYER_NUMBER {
            return;
        }
        let slots: Vec<usize> = data
            .as_chunks::<8>()
            .0
            .iter()
            .map(|slot| u64::from_be_bytes(*slot) as usize)
            .collect();
        // a slot is free for this peer unless it is out of range, played here,
        // or held by another peer that is still connected
        let allowed = slots.iter().all(|slot| match self.known.get(slot) {
            _ if *slot >= self.number_of_players => false,
            Some(None) => false,
            Some(Some(owner)) => owner == &peer || !connected.contains(owner),
            None => true,
        });
        self.refused.retain(|refused| refused != &peer);
        if !allowed {
            // refuse the whole message, so a bad setup can never start a game
            self.refused.push(peer);
            return;
        }
        for slot in slots {
            self.known.insert(slot, Some(peer));
            if name == MESSAGE_PLAYER_NUMBER {
                self.confirmed.insert(slot, Some(peer));
            }
        }
    }

    fn all_owned_by_connected(
        slots: &std::collections::BTreeMap<usize, Option<P>>,
        number_of_players: usize,
        connected: &[P],
    ) -> bool {
        slots.len() == number_of_players
            && slots
                .values()
                .all(|owner| owner.is_none_or(|peer| connected.contains(&peer)))
    }

    // this client is connected to the owner of every slot
    fn fully_connected(&self, connected: &[P]) -> bool {
        Self::all_owned_by_connected(&self.known, self.number_of_players, connected)
    }

    // two clients were set up with the same slot, or with a slot that does not exist
    fn conflict(&self, connected: &[P]) -> bool {
        self.refused.iter().any(|peer| connected.contains(peer))
    }

    // every client is connected to every other one, the game can start
    fn complete(&self, connected: &[P]) -> bool {
        !self.conflict(connected)
            && Self::all_owned_by_connected(&self.confirmed, self.number_of_players, connected)
    }

    fn missing_players(&self) -> usize {
        self.number_of_players - self.confirmed.len()
    }
}

fn send_handshake_message(
    channel: &mut matchbox_socket::WebRtcChannel,
    peers: &[matchbox_socket::PeerId],
    name: &str,
    local_players: &[usize],
) {
    let message = NetplayMessage {
        name: name.to_string(),
        data: encode_player_numbers(local_players),
    };
    let data = postcard::to_stdvec(&message).unwrap();
    for peer in peers {
        if let Err(e) = channel.try_send(data.clone().into(), *peer) {
            eprintln!("Failed to send message: {}", e);
        }
    }
}

fn receive_handshake_messages(
    channel: &mut matchbox_socket::WebRtcChannel,
    handshake: &mut Handshake<matchbox_socket::PeerId>,
    connected: &[matchbox_socket::PeerId],
) {
    for (peer, data) in channel.receive() {
        let message = postcard::from_bytes::<NetplayMessage>(&data).unwrap();
        handshake.receive(peer, &message.name, &message.data, connected);
    }
}

pub fn send_rtc(netplay: &mut Netplay, rtc: i64) {
    let message = NetplayMessage {
        name: "rtc".to_string(),
        data: rtc.to_be_bytes().to_vec(),
    };
    send_message(netplay, message);
}

pub fn receive_rtc(netplay: &mut Netplay) -> i64 {
    let message = receive_message(netplay, "rtc");

    i64::from_be_bytes(message.try_into().unwrap())
}

pub fn send_rng(netplay: &mut Netplay, seed: u64) {
    let message = NetplayMessage {
        name: "rng".to_string(),
        data: seed.to_be_bytes().to_vec(),
    };
    send_message(netplay, message);
}

pub fn receive_rng(netplay: &mut Netplay) -> u64 {
    let message = receive_message(netplay, "rng");
    u64::from_be_bytes(message.try_into().unwrap())
}

pub fn send_save(netplay: &mut Netplay, save_type: &str, save_data: &[u8]) {
    let message = NetplayMessage {
        name: save_type.to_string(),
        data: save_data.to_vec(),
    };
    send_message(netplay, message);
}

pub fn receive_save(netplay: &mut Netplay, save_type: &str, save_data: &mut Vec<u8>) {
    let message = receive_message(netplay, save_type);
    *save_data = message;
}

pub fn change_input_delay(netplay: &mut Netplay, input_delay: usize) {
    if input_delay > MAX_PREDICTION_WINDOW {
        return;
    }

    netplay.input_delay = input_delay;
    for handle in netplay.session.local_player_handles() {
        if let Err(e) = netplay.session.set_input_delay(handle, input_delay) {
            eprintln!("Error setting input delay: {}", e);
        } else {
            ui::video::onscreen_message(
                &format!("Input delay: {}", input_delay),
                ui::video::MESSAGE_LENGTH_MESSAGE_VERY_SHORT,
            );
        }
    }
}

fn check_disconnect(netplay: &mut Netplay) {
    if !netplay.disconnected && netplay.messages.remove("disconnect").is_some() {
        netplay.disconnected = true;
        ui::video::onscreen_message(
            "Player disconnected, session has ended",
            ui::video::MESSAGE_LENGTH_MESSAGE_LONG,
        );
    }
}

pub fn in_rollback(netplay: Option<&Netplay>) -> bool {
    netplay.is_some_and(|netplay| {
        netplay
            .requests
            .iter()
            .any(|r| matches!(r, ggrs::GgrsRequest::AdvanceFrame { .. }))
    })
}

pub fn replaying_audio(netplay: Option<&Netplay>) -> bool {
    // A replay-only batch can end while prediction is still blocked. Even its
    // final frame must not resubmit audio, regardless of the pending requests.
    netplay.is_some_and(|netplay| netplay.frame_progress.replaying)
}

pub fn process_requests(device: &mut device::Device) -> Vec<ui::input::InputData> {
    loop {
        if let Some(request) = device.netplay.as_mut().unwrap().requests.pop_front() {
            match request {
                ggrs::GgrsRequest::SaveGameState { cell, frame } => {
                    savestates::create_savestate(device, true, Some(frame));

                    let mut hasher = sha2::Sha256::new();
                    for reg in device.cpu.cop0.regs.as_ref() {
                        hasher.update(reg.to_be_bytes());
                    }
                    let hash = u128::from_be_bytes(hasher.finalize()[..16].try_into().unwrap());
                    cell.save(frame, Some(frame), Some(hash));
                }
                ggrs::GgrsRequest::LoadGameState { cell, frame: _ } => {
                    if let Some(frame) = cell.load() {
                        savestates::load_savestate(device, true, Some(frame));
                        device.netplay.as_mut().unwrap().frame_progress.load(frame);
                    }
                }
                ggrs::GgrsRequest::AdvanceFrame { inputs } => {
                    let netplay = device.netplay.as_mut().unwrap();
                    let frame = netplay.frame_progress.next_frame;
                    if frame % SYNC_LOG_INTERVAL == 0
                        && let Some(log) = netplay.sync_log.as_mut()
                    {
                        write_sync_log(log, frame, &device.rdram.mem);
                    }
                    netplay.frame_progress.advance();
                    return inputs.into_iter().map(|(input, _)| input).collect();
                }
            }
        } else {
            process_netplay(device);
        }
    }
}

fn poll_clients(netplay: &mut Netplay) {
    netplay.session.poll_remote_clients();
    for event in netplay.session.events() {
        match event {
            ggrs::GgrsEvent::Synchronizing { .. } => {}
            ggrs::GgrsEvent::Synchronized { .. } => {}
            ggrs::GgrsEvent::Disconnected { .. } => {
                if !netplay.disconnected {
                    netplay.disconnected = true;
                    ui::video::onscreen_message(
                        "Lost connection to peer(s)",
                        ui::video::MESSAGE_LENGTH_MESSAGE_LONG,
                    );
                }
            }
            ggrs::GgrsEvent::NetworkInterrupted { .. } => {
                println!("network interrupted");
            }
            ggrs::GgrsEvent::NetworkResumed { .. } => {
                println!("network resumed");
            }
            ggrs::GgrsEvent::WaitRecommendation { skip_frames } => {
                println!("wait recommendation: skip_frames={}", skip_frames);
            }
            ggrs::GgrsEvent::DesyncDetected { .. } => {
                eprintln!("desync detected");
                ui::video::onscreen_message(
                    "Desync detected",
                    ui::video::MESSAGE_LENGTH_MESSAGE_LONG,
                );
            }
        }
    }
}

fn process_netplay(device: &mut device::Device) {
    let netplay = device.netplay.as_mut().unwrap();

    poll_clients(netplay);
    process_reliable_messages(netplay);
    advance_frame(device);
}

fn advance_frame(device: &mut device::Device) {
    let netplay = device.netplay.as_mut().unwrap();
    // the GGRS handle of a player is its slot number
    for (port, slot) in netplay.local_players.iter().enumerate() {
        let local_input = match netplay.test_input.as_mut() {
            Some(test_input) => test_input.next(port),
            None => ui::input::get(&mut device.ui, port),
        };
        netplay.session.add_local_input(*slot, local_input).unwrap();
    }

    if netplay.disconnected {
        netplay.requests.push_back(ggrs::GgrsRequest::AdvanceFrame {
            inputs: vec![
                (
                    ui::input::InputData::default(),
                    ggrs::InputStatus::Disconnected
                );
                4
            ],
        });
        return;
    }

    match netplay.session.advance_frame() {
        Ok(requests) => {
            netplay.requests.extend(requests);
        }
        Err(ggrs::GgrsError::PredictionThreshold) => {
            println!("prediction threshold reached");
        }
        Err(e) => panic!("{e}"),
    }
}

fn create_socket(builder: matchbox_socket::WebRtcSocketBuilder) -> matchbox_socket::WebRtcSocket {
    let (socket, loop_fut) = builder.build();
    tokio::spawn(async move {
        if let Err(e) = loop_fut.await {
            eprintln!("WebRTC loop failed: {}", e);
        }
    });
    socket
}

pub fn init(
    device: &mut device::Device,
    netplay_config: &NetplayConfig,
    pal: bool,
) -> Option<Netplay> {
    let mut builder = matchbox_socket::WebRtcSocketBuilder::new(&netplay_config.server_addr)
        .add_unreliable_channel()
        .add_reliable_channel();

    if let Ok(ice_config) = std::fs::read(&netplay_config.ice_config_path)
        && let Ok(ice_config) = serde_json::from_slice::<RtcIceServerConfig>(&ice_config)
    {
        builder = builder.ice_server(matchbox_socket::RtcIceServerConfig {
            urls: ice_config.urls,
            username: ice_config.username,
            credential: ice_config.credential,
        });
    } else {
        eprintln!("Using default ICE config");
    }

    let mut socket = create_socket(builder.clone());

    let mut now = std::time::Instant::now();
    let mut message_timer = now;
    let socket_timeout = std::time::Duration::from_secs_f64(rand::random_range(8.0..10.0));
    let mut local_players = netplay_config.local_players.clone();
    local_players.sort_unstable();
    local_players.dedup();
    let mut handshake = Handshake::new(&local_players, netplay_config.number_of_players);

    ui::video::onscreen_message(
        "Connecting to netplay peers...\nPlease wait...",
        ui::video::MESSAGE_LENGTH_MESSAGE_SHORT,
    );

    device.cpu.running = true;
    while device.cpu.running {
        if socket
            .update_peers()
            .iter()
            .any(|(_, peer_state)| *peer_state == matchbox_socket::PeerState::Disconnected)
        {
            // if someone has disconnected, reset the timeout
            now = std::time::Instant::now();
        }

        let connected_peers = socket
            .connected_peers()
            .collect::<Vec<matchbox_socket::PeerId>>();

        receive_handshake_messages(socket.channel_mut(1), &mut handshake, &connected_peers);
        send_handshake_message(
            socket.channel_mut(1),
            &connected_peers,
            MESSAGE_PLAYER_SLOTS,
            &local_players,
        );
        if handshake.fully_connected(&connected_peers) {
            send_handshake_message(
                socket.channel_mut(1),
                &connected_peers,
                MESSAGE_PLAYER_NUMBER,
                &local_players,
            );
            if handshake.complete(&connected_peers) {
                break;
            }
        } else if now.elapsed() > socket_timeout {
            socket.close();
            handshake.reset();
            socket = create_socket(builder.clone());
            now = std::time::Instant::now();
        }

        if message_timer.elapsed() > std::time::Duration::from_secs(4) {
            let message = if handshake.conflict(&connected_peers) {
                "Netplay player numbers conflict between clients".to_string()
            } else {
                format!(
                    "Still waiting for {} netplay player(s)...\nPlease wait...",
                    handshake.missing_players()
                )
            };
            ui::video::onscreen_message(&message, ui::video::MESSAGE_LENGTH_MESSAGE_SHORT);
            message_timer = std::time::Instant::now();
        }

        ui::video::render_frame();
        ui::video::update_screen();
        std::thread::sleep(std::time::Duration::from_millis(10));
        ui::video::check_callback(device);
    }
    if !device.cpu.running {
        // user closed the window
        return None;
    }
    device.cpu.running = false;

    let mut session_builder = ggrs::SessionBuilder::<GgrsConfig>::new()
        .with_num_players(netplay_config.number_of_players)
        .unwrap()
        .with_input_delay(netplay_config.input_delay)
        .with_sparse_saving_mode(true)
        .with_fps(if pal { 50 } else { 60 })
        .unwrap()
        .with_desync_detection_mode(ggrs::DesyncDetection::On { interval: 60 })
        .with_max_prediction_window(MAX_PREDICTION_WINDOW)
        .with_disconnect_timeout(std::time::Duration::from_secs(if cfg!(debug_assertions) {
            10
        } else {
            5
        }));

    let mut peers = vec![];
    for (i, peer) in handshake.confirmed.iter() {
        if let Some(peer) = peer {
            session_builder = session_builder
                .add_player(ggrs::PlayerType::Remote(*peer), *i)
                .unwrap();
            // a peer playing several slots must only be messaged once
            if !peers.contains(peer) {
                peers.push(*peer);
            }
        } else {
            session_builder = session_builder
                .add_player(ggrs::PlayerType::Local, *i)
                .unwrap();
        }
    }

    let matchbox_channel = MatchboxChannel {
        channel: socket.take_channel(0).unwrap(),
    };
    let reliable_channel = socket.take_channel(1).unwrap();

    if matchbox_channel.channel.config().max_retransmits != Some(0)
        || matchbox_channel.channel.config().ordered
    {
        eprintln!("Sending GGRS traffic over reliable channel");
    }

    let mut session = session_builder.start_p2p_session(matchbox_channel).unwrap();

    let now = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(10);
    while session.current_state() != ggrs::SessionState::Running {
        session.poll_remote_clients();
        if now.elapsed() > timeout {
            eprintln!("Could not start netplay session");
            return None;
        }
        unsafe { sdl3_sys::events::SDL_PumpEvents() };
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    Some(Netplay {
        disconnected: false,
        incoming_message: vec![],
        input_delay: netplay_config.input_delay,
        session,
        reliable_channel,
        peers,
        local_players,
        connected: [
            netplay_config.number_of_players > 0,
            netplay_config.number_of_players > 1,
            netplay_config.number_of_players > 2,
            netplay_config.number_of_players > 3,
        ],
        inputs: vec![ui::input::InputData::default(); netplay_config.number_of_players],
        requests: std::collections::VecDeque::new(),
        frame_progress: FrameProgress::default(),
        received_data: std::collections::VecDeque::new(),
        messages: std::collections::HashMap::new(),
        ice_config_path: netplay_config.ice_config_path.clone(),
        sync_log: std::env::var_os(SYNC_LOG_VARIABLE).and_then(|path| {
            std::fs::File::create(&path)
                .inspect_err(|e| eprintln!("Could not create netplay sync log: {e}"))
                .ok()
        }),
        test_input: std::env::var(TEST_INPUT_VARIABLE).ok().and_then(|seed| {
            seed.parse()
                .inspect_err(|_| eprintln!("{TEST_INPUT_VARIABLE} must be a number"))
                .ok()
                .map(TestInput::new)
        }),
    })
}

pub fn close(netplay: &mut Netplay) {
    if !netplay.disconnected {
        let message = NetplayMessage {
            name: "disconnect".to_string(),
            data: vec![],
        };
        send_message(netplay, message);
        std::thread::sleep(std::time::Duration::from_millis(200)); // give the message time to be sent
    }

    let _ = std::fs::remove_file(&netplay.ice_config_path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_log_line_locates_a_difference() {
        let mut memory = vec![0u8; SYNC_LOG_CHUNK * 3];
        let before = sync_log_line(120, &memory);
        memory[SYNC_LOG_CHUNK + 5] = 1;
        let after = sync_log_line(120, &memory);
        let (before, after): (Vec<&str>, Vec<&str>) =
            (before.split(' ').collect(), after.split(' ').collect());
        assert_eq!(before.len(), 4);
        assert_eq!(before[0], "120");
        let differing: Vec<usize> = (0..4).filter(|i| before[*i] != after[*i]).collect();
        assert_eq!(differing, [2]);
    }

    #[test]
    fn test_input_is_repeatable_and_varied() {
        let mut a = TestInput::new(7);
        let mut b = TestInput::new(7);
        let mut c = TestInput::new(8);
        let (mut differs, mut changes, mut last) = (false, 0, 0);
        for _ in 0..600 {
            let input = a.next(0);
            assert!(input == b.next(0));
            differs |= input != c.next(0);
            changes += (input.data != last) as u32;
            last = input.data;
            // the stick stays within what a controller can report
            assert!((input.data >> 16) as u8 as i8 >= -80 && (input.data >> 16) as u8 as i8 <= 80);
        }
        assert!(differs);
        assert!(changes > 10);
    }

    // delivers one handshake message from a peer
    fn deliver(to: &mut Handshake<u8>, from: u8, name: &str, slots: &[usize], connected: &[u8]) {
        to.receive(from, name, &encode_player_numbers(slots), connected);
    }

    #[test]
    fn single_player_message_is_unchanged() {
        // what a client with one player has always sent
        assert_eq!(encode_player_numbers(&[2]), 2u64.to_be_bytes().to_vec());
    }

    #[test]
    fn two_clients_three_players() {
        const PC: u8 = 1;
        const MAC: u8 = 2;
        let mut pc = Handshake::new(&[0, 1], 3);
        let mut mac = Handshake::new(&[2], 3);
        assert!(!pc.fully_connected(&[MAC]));
        assert_eq!(pc.missing_players(), 1);

        deliver(&mut pc, MAC, MESSAGE_PLAYER_SLOTS, &[2], &[MAC]);
        deliver(&mut mac, PC, MESSAGE_PLAYER_SLOTS, &[0, 1], &[PC]);
        assert!(pc.fully_connected(&[MAC]) && mac.fully_connected(&[PC]));
        // knowing the slots is not enough, the other side must confirm
        assert!(!pc.complete(&[MAC]) && !mac.complete(&[PC]));

        deliver(&mut pc, MAC, MESSAGE_PLAYER_NUMBER, &[2], &[MAC]);
        deliver(&mut mac, PC, MESSAGE_PLAYER_NUMBER, &[0, 1], &[PC]);
        assert!(pc.complete(&[MAC]) && mac.complete(&[PC]));
        for slot in 0..3 {
            // each slot is local on exactly one side
            assert_ne!(
                pc.confirmed[&slot].is_none(),
                mac.confirmed[&slot].is_none()
            );
        }
        assert_eq!(pc.confirmed[&2], Some(MAC));
        assert_eq!(mac.confirmed[&0], Some(PC));
        assert_eq!(mac.confirmed[&1], Some(PC));
    }

    #[test]
    fn works_with_a_client_that_only_sends_player_number() {
        // an unpatched client sends nothing until it is connected to everyone
        let mut patched = Handshake::new(&[0], 2);
        assert!(!patched.fully_connected(&[9]));
        deliver(&mut patched, 9, MESSAGE_PLAYER_NUMBER, &[1], &[9]);
        assert!(patched.fully_connected(&[9]));
        assert!(patched.complete(&[9]));
    }

    #[test]
    fn waits_until_every_client_is_fully_connected() {
        // three clients: A must not start while B and C are still connecting
        const B: u8 = 2;
        const C: u8 = 3;
        let mut a = Handshake::new(&[0], 3);
        deliver(&mut a, B, MESSAGE_PLAYER_SLOTS, &[1], &[B, C]);
        deliver(&mut a, C, MESSAGE_PLAYER_SLOTS, &[2], &[B, C]);
        assert!(a.fully_connected(&[B, C]));
        assert!(!a.complete(&[B, C]));
        deliver(&mut a, B, MESSAGE_PLAYER_NUMBER, &[1], &[B, C]);
        assert!(!a.complete(&[B, C]));
        deliver(&mut a, C, MESSAGE_PLAYER_NUMBER, &[2], &[B, C]);
        assert!(a.complete(&[B, C]));
        // and not if one of them has dropped again
        assert!(!a.complete(&[B]));
    }

    #[test]
    fn overlapping_slots_never_complete() {
        // misconfigured: both clients ask for slot 1
        let mut a = Handshake::new(&[0, 1], 3);
        let mut b = Handshake::new(&[1, 2], 3);
        for name in [MESSAGE_PLAYER_SLOTS, MESSAGE_PLAYER_NUMBER] {
            deliver(&mut a, 2, name, &[1, 2], &[2]);
            deliver(&mut b, 1, name, &[0, 1], &[1]);
        }
        assert!(a.conflict(&[2]) && b.conflict(&[1]));
        // nothing from the refused message is kept
        assert_eq!(a.known.len(), 2);
        assert!(!a.fully_connected(&[2]) && !a.complete(&[2]));
        assert!(!b.fully_connected(&[1]) && !b.complete(&[1]));
    }

    #[test]
    fn two_peers_cannot_hold_the_same_slot() {
        let mut a = Handshake::new(&[0], 3);
        deliver(&mut a, 2, MESSAGE_PLAYER_SLOTS, &[1], &[2, 3]);
        deliver(&mut a, 3, MESSAGE_PLAYER_SLOTS, &[1], &[2, 3]);
        assert!(a.conflict(&[2, 3]));
        assert_eq!(a.known[&1], Some(2));
        // once the first one has gone, the second one may have the slot
        deliver(&mut a, 3, MESSAGE_PLAYER_SLOTS, &[1], &[3]);
        assert!(!a.conflict(&[3]));
        assert_eq!(a.known[&1], Some(3));
    }

    #[test]
    fn out_of_range_slot_is_refused() {
        let mut a = Handshake::new(&[0], 3);
        deliver(&mut a, 2, MESSAGE_PLAYER_NUMBER, &[1, 3], &[2]);
        assert!(a.conflict(&[2]));
        assert_eq!(a.known.len(), 1);
    }

    #[test]
    fn reconnecting_peer_replaces_its_old_id() {
        let mut mac = Handshake::new(&[2], 3);
        deliver(&mut mac, 7, MESSAGE_PLAYER_NUMBER, &[0, 1], &[7]);
        assert!(mac.complete(&[7]));
        // the peer comes back with a new id, the old one is gone
        assert!(!mac.complete(&[9]));
        deliver(&mut mac, 9, MESSAGE_PLAYER_NUMBER, &[0, 1], &[9]);
        assert!(!mac.conflict(&[9]));
        assert!(mac.complete(&[9]));
    }

    #[test]
    fn reconnect_while_the_old_id_lingers_recovers() {
        let mut mac = Handshake::new(&[2], 3);
        deliver(&mut mac, 7, MESSAGE_PLAYER_NUMBER, &[0, 1], &[7]);
        // the new id shows up before the old connection is seen to drop
        deliver(&mut mac, 9, MESSAGE_PLAYER_NUMBER, &[0, 1], &[7, 9]);
        assert!(mac.conflict(&[7, 9]));
        // the old connection drops and the peer repeats its message
        deliver(&mut mac, 9, MESSAGE_PLAYER_NUMBER, &[0, 1], &[9]);
        assert!(!mac.conflict(&[9]));
        assert!(mac.complete(&[9]));
    }

    #[test]
    fn reset_forgets_peers_but_not_local_players() {
        let mut pc = Handshake::new(&[0, 1], 3);
        deliver(&mut pc, 2, MESSAGE_PLAYER_NUMBER, &[2], &[2]);
        deliver(&mut pc, 3, MESSAGE_PLAYER_NUMBER, &[0], &[2, 3]);
        assert!(pc.conflict(&[2, 3]) && !pc.complete(&[2, 3]));
        pc.reset();
        assert!(!pc.conflict(&[2, 3]));
        assert_eq!(pc.missing_players(), 1);
        assert_eq!(pc.known.len(), 2);
    }

    #[test]
    fn other_messages_and_truncated_data_are_ignored() {
        let mut a = Handshake::new(&[0], 2);
        deliver(&mut a, 2, "rng", &[1], &[2]);
        a.receive(2, MESSAGE_PLAYER_NUMBER, &[0, 0, 0], &[2]);
        assert!(!a.conflict(&[2]));
        assert_eq!(a.known.len(), 1);
    }
}
