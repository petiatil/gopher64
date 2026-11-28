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
    delayed: Option<TestPacketDelay>,
}

/// Loopback diagnostic. Set the one-way delay on each client to half the desired RTT.
/// Keep packets in send order and leave the emulation thread free to poll and play.
struct TestPacketDelay {
    delay: std::time::Duration,
    tx: tokio::sync::mpsc::UnboundedSender<(
        tokio::time::Instant,
        matchbox_socket::PeerId,
        matchbox_socket::Packet,
    )>,
}

impl TestPacketDelay {
    fn from_env(channel: &matchbox_socket::WebRtcChannel) -> Option<Self> {
        let ms: u64 = std::env::var("GOPHER64_NETPLAY_TEST_DELAY_MS")
            .ok()?
            .parse()
            .ok()?;
        if ms == 0 {
            return None;
        }
        let sender = channel.sender_clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some((deadline, peer, packet)) = rx.recv().await {
                tokio::time::sleep_until(deadline).await;
                if sender.unbounded_send((peer, packet)).is_err() {
                    break;
                }
            }
        });
        Some(Self {
            delay: std::time::Duration::from_millis(ms),
            tx,
        })
    }
}

impl ggrs::NonBlockingSocket<matchbox_socket::PeerId> for MatchboxChannel {
    fn send_to(&mut self, msg: &ggrs::Message, addr: &matchbox_socket::PeerId) {
        let encoded = postcard::to_stdvec(msg).expect("serialization failed");
        if let Some(delayed) = &self.delayed {
            let _ = delayed.tx.send((
                tokio::time::Instant::now() + delayed.delay,
                *addr,
                encoded.into(),
            ));
        } else {
            let _ = self.channel.try_send(encoded.into(), *addr);
        }
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
    /// Set when the player slots are handed out by the host instead of being
    /// chosen on every client.
    pub auto_slots: Option<AutoSlots>,
}

pub struct AutoSlots {
    pub role: AutoRole,
    /// How many people play on this client.
    pub local_player_count: usize,
    /// Identifies the game, so that clients running different ROMs do not start.
    pub game_id: String,
}

#[derive(PartialEq)]
pub enum AutoRole {
    /// Plays the first slots (`local_players` and `number_of_players` are set)
    /// and hands the others out.
    Host,
    /// Asks the host for slots (`local_players` and `number_of_players` are
    /// only known once it answers).
    Join,
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
    pacing: FramePacing,
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
    pub fn pace_frame(&mut self, frame_time: f64) {
        let ahead = if self.disconnected {
            0
        } else {
            self.session.frames_ahead()
        };
        self.pacing.adjust(ahead);
        let interval =
            std::time::Duration::from_secs_f64(frame_time * (1.0 + self.pacing.extra_period));
        let now = std::time::Instant::now();
        let deadline = self.pacing.deadline.unwrap_or(now);
        if deadline > now {
            spin_sleep::sleep(deadline - now);
        }
        // Preserve phase across ordinary jitter. After a long replay allow one
        // immediate frame, without banking a burst of overdue presentations.
        self.pacing.deadline = Some((deadline + interval).max(std::time::Instant::now()));
    }

    pub fn audio_rate(&self) -> f64 {
        (1.0 + self.pacing.extra_period).recip()
    }

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

/// Wall-clock state stays with the session across rollback loads.
#[derive(Default)]
struct FramePacing {
    deadline: Option<std::time::Instant>,
    wait_frames: f64,
    extra_period: f64,
}

impl FramePacing {
    fn recommend_wait(&mut self, frames: u32) {
        // Each recommendation is a new estimate of the same lead.
        self.wait_frames = f64::from(frames);
    }

    fn adjust(&mut self, ahead: i32) {
        if ahead <= 0 {
            self.wait_frames = 0.0;
        }
        // Trim smaller leads before GGRS reaches its three-frame threshold.
        // Spread a recommended wait over at least 25 VIs per frame of lead.
        let trim = f64::from(ahead.clamp(0, 2)) / 60.0;
        let target = trim.max(self.wait_frames.min(0.04));
        self.extra_period += (target - self.extra_period).clamp(-0.002, 0.002);
        self.wait_frames = (self.wait_frames - self.extra_period).max(0.0);
    }
}

#[derive(Default)]
struct FrameProgress {
    next_frame: i32,
    next_live_frame: i32,
    replaying: bool,
}

impl FrameProgress {
    fn load(&mut self, frame: i32) {
        self.next_frame = frame;
    }

    fn advance(&mut self) {
        // Count each frame once for presentation, audio and wall-clock pacing.
        self.replaying = self.next_frame < self.next_live_frame;
        self.next_frame += 1;
        self.next_live_frame = self.next_live_frame.max(self.next_frame);
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
fn decode_player_numbers(data: &[u8]) -> Vec<usize> {
    data.as_chunks::<8>()
        .0
        .iter()
        .map(|slot| u64::from_be_bytes(*slot) as usize)
        .collect()
}

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
        let slots = decode_player_numbers(data);
        // A peer only holds what its latest message asks for. Slots it asked
        // for earlier are free again, also when this message is refused: with
        // slots handed out by a host, a client can be given different ones.
        self.known
            .retain(|slot, owner| *owner != Some(peer) || slots.contains(slot));
        self.confirmed
            .retain(|slot, owner| *owner != Some(peer) || slots.contains(slot));
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

// asks the host for slots: the number of local players, then the game id
const MESSAGE_AUTO_JOIN: &str = "auto_join";
// the host's answer: the number of players in the session, then the slots given
const MESSAGE_AUTO_SLOTS: &str = "auto_slots";
// the host's other answer: one of the REFUSED_ codes
const MESSAGE_AUTO_REFUSED: &str = "auto_refused";
const REFUSED_DIFFERENT_GAME: u8 = 1;
const REFUSED_FULL: u8 = 2;

fn encode_auto_join(local_player_count: usize, game_id: &str) -> Vec<u8> {
    let mut data = (local_player_count as u64).to_be_bytes().to_vec();
    data.extend_from_slice(game_id.as_bytes());
    data
}

fn encode_auto_slots(number_of_players: usize, slots: &[usize]) -> Vec<u8> {
    let mut data = (number_of_players as u64).to_be_bytes().to_vec();
    data.extend(encode_player_numbers(slots));
    data
}

/// What a joining client accepts as its slots: the number of players in the
/// session and exactly `local_player_count` valid slots. Anything else is ignored.
fn decode_auto_slots(data: &[u8], local_player_count: usize) -> Option<(usize, Vec<usize>)> {
    let mut numbers = data
        .as_chunks::<8>()
        .0
        .iter()
        .map(|number| u64::from_be_bytes(*number) as usize);
    let number_of_players = numbers.next()?;
    let slots: Vec<usize> = numbers.collect();
    let valid = number_of_players <= 4
        && slots.len() == local_player_count
        && slots.len() < number_of_players
        && slots.iter().all(|slot| *slot < number_of_players)
        && slots.windows(2).all(|pair| pair[0] < pair[1]);
    valid.then_some((number_of_players, slots))
}

enum AutoReply {
    Slots(Vec<usize>),
    Refused(u8),
}

/// The host's side of handing out slots: it plays the first ones itself and
/// gives every client that asks the lowest free ones.
struct SlotAssigner<P> {
    number_of_players: usize,
    local_player_count: usize,
    game_id: String,
    assigned: Vec<(P, Vec<usize>)>,
}

impl<P: Copy + PartialEq> SlotAssigner<P> {
    fn new(number_of_players: usize, local_player_count: usize, game_id: &str) -> Self {
        Self {
            number_of_players,
            local_player_count,
            game_id: game_id.to_string(),
            assigned: vec![],
        }
    }

    /// Whether `slots` is what `peer` was given. The host ignores every other
    /// claim, such as one a client still sends for slots it had before.
    fn gave(&self, peer: P, slots: &[usize]) -> bool {
        self.assigned
            .iter()
            .any(|(owner, given)| *owner == peer && given == slots)
    }

    /// Answers an `auto_join` message. A client keeps the slots it was given
    /// for as long as it stays connected and asks for the same number of them.
    fn request(&mut self, peer: P, data: &[u8], connected: &[P]) -> Option<AutoReply> {
        let (count, game_id) = data.split_first_chunk::<8>()?;
        let count = u64::from_be_bytes(*count) as usize;

        // the slots of a client that left are free again
        self.assigned.retain(|(owner, _)| connected.contains(owner));

        if game_id != self.game_id.as_bytes() {
            self.assigned.retain(|(owner, _)| *owner != peer);
            return Some(AutoReply::Refused(REFUSED_DIFFERENT_GAME));
        }
        if let Some((_, slots)) = self.assigned.iter().find(|(owner, _)| *owner == peer)
            && slots.len() == count
        {
            return Some(AutoReply::Slots(slots.clone()));
        }
        self.assigned.retain(|(owner, _)| *owner != peer);

        let free: Vec<usize> = (self.local_player_count..self.number_of_players)
            .filter(|slot| !self.assigned.iter().any(|(_, slots)| slots.contains(slot)))
            .take(count)
            .collect();
        if count == 0 || free.len() < count {
            return Some(AutoReply::Refused(REFUSED_FULL));
        }
        self.assigned.push((peer, free.clone()));
        Some(AutoReply::Slots(free))
    }
}

fn send_handshake_message(
    channel: &mut matchbox_socket::WebRtcChannel,
    peers: &[matchbox_socket::PeerId],
    name: &str,
    local_players: &[usize],
) {
    send_raw_message(channel, peers, name, encode_player_numbers(local_players));
}

fn send_raw_message(
    channel: &mut matchbox_socket::WebRtcChannel,
    peers: &[matchbox_socket::PeerId],
    name: &str,
    data: Vec<u8>,
) {
    let message = NetplayMessage {
        name: name.to_string(),
        data,
    };
    let data = postcard::to_stdvec(&message).unwrap();
    for peer in peers {
        if let Err(e) = channel.try_send(data.clone().into(), *peer) {
            eprintln!("Failed to send message: {}", e);
        }
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
    // Even the last frame of a replay-only batch has already been presented.
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
                netplay.pacing.recommend_wait(skip_frames);
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
    let mut number_of_players = netplay_config.number_of_players;
    let auto_slots = netplay_config.auto_slots.as_ref();
    let joining = auto_slots.filter(|auto| auto.role == AutoRole::Join);
    // a joining client only knows its slots once the host has answered
    let mut handshake = joining
        .is_none()
        .then(|| Handshake::new(&local_players, number_of_players));
    let mut assigner = auto_slots
        .filter(|auto| auto.role == AutoRole::Host)
        .map(|auto| SlotAssigner::new(number_of_players, auto.local_player_count, &auto.game_id));
    let mut refused: Option<u8> = None;
    // the peer whose `auto_slots` this joining client follows
    let mut assigned_by: Option<matchbox_socket::PeerId> = None;

    ui::video::onscreen_message(
        "Connecting to netplay peers...\nPlease wait...",
        ui::video::MESSAGE_LENGTH_MESSAGE_SHORT,
    );

    device.cpu.running = true;
    while device.cpu.running {
        match socket.try_update_peers() {
            Ok(changes) => {
                if changes
                    .iter()
                    .any(|(_, peer_state)| *peer_state == matchbox_socket::PeerState::Disconnected)
                {
                    // if someone has disconnected, reset the timeout
                    now = std::time::Instant::now();
                }
            }
            Err(_) => {
                // The socket gave up on the connection server. That is normal
                // when joining a host that is not running yet, or that has
                // left: keep trying until the player closes the window.
                if let Some(handshake) = handshake.as_mut() {
                    handshake.reset();
                }
                socket = create_socket(builder.clone());
                now = std::time::Instant::now();
            }
        }

        let connected_peers = socket
            .connected_peers()
            .collect::<Vec<matchbox_socket::PeerId>>();

        if joining.is_some() && assigned_by.is_some_and(|host| !connected_peers.contains(&host)) {
            // the host that gave out the slots is gone, and with it the slots
            assigned_by = None;
            handshake = None;
        }

        for (peer, data) in socket.channel_mut(1).receive() {
            let Ok(message) = postcard::from_bytes::<NetplayMessage>(&data) else {
                continue;
            };
            match message.name.as_str() {
                MESSAGE_AUTO_JOIN => {
                    if let Some(assigner) = assigner.as_mut()
                        && let Some(reply) = assigner.request(peer, &message.data, &connected_peers)
                    {
                        let (name, data) = match reply {
                            AutoReply::Slots(slots) => (
                                MESSAGE_AUTO_SLOTS,
                                encode_auto_slots(number_of_players, &slots),
                            ),
                            AutoReply::Refused(code) => (MESSAGE_AUTO_REFUSED, vec![code]),
                        };
                        send_raw_message(socket.channel_mut(1), &[peer], name, data);
                    }
                }
                MESSAGE_AUTO_SLOTS => {
                    if let Some(auto) = joining
                        && let Some((total, slots)) =
                            decode_auto_slots(&message.data, auto.local_player_count)
                    {
                        refused = None;
                        assigned_by = Some(peer);
                        if handshake.is_none()
                            || total != number_of_players
                            || slots != local_players
                        {
                            number_of_players = total;
                            local_players = slots;
                            handshake = Some(Handshake::new(&local_players, number_of_players));
                        }
                    }
                }
                MESSAGE_AUTO_REFUSED => {
                    if joining.is_some() {
                        refused = message.data.first().copied();
                    }
                }
                _ => {
                    let given = assigner.as_ref().is_none_or(|assigner| {
                        assigner.gave(peer, &decode_player_numbers(&message.data))
                    });
                    if given && let Some(handshake) = handshake.as_mut() {
                        handshake.receive(peer, &message.name, &message.data, &connected_peers);
                    }
                }
            }
        }

        if let Some(auto) = joining {
            // repeated like the other handshake messages: the host may only
            // start listening later, and forgets clients that reconnect
            send_raw_message(
                socket.channel_mut(1),
                &connected_peers,
                MESSAGE_AUTO_JOIN,
                encode_auto_join(auto.local_player_count, &auto.game_id),
            );
        }

        let mut fully_connected = false;
        if let Some(handshake) = handshake.as_ref() {
            send_handshake_message(
                socket.channel_mut(1),
                &connected_peers,
                MESSAGE_PLAYER_SLOTS,
                &local_players,
            );
            fully_connected = handshake.fully_connected(&connected_peers);
            if fully_connected {
                send_handshake_message(
                    socket.channel_mut(1),
                    &connected_peers,
                    MESSAGE_PLAYER_NUMBER,
                    &local_players,
                );
                if handshake.complete(&connected_peers) {
                    break;
                }
            }
        }
        if !fully_connected && now.elapsed() > socket_timeout {
            socket.close();
            if let Some(handshake) = handshake.as_mut() {
                handshake.reset();
            }
            socket = create_socket(builder.clone());
            now = std::time::Instant::now();
        }

        if message_timer.elapsed() > std::time::Duration::from_secs(4) {
            let message = match (refused, handshake.as_ref()) {
                (Some(REFUSED_DIFFERENT_GAME), _) => {
                    "The netplay host is running a different game".to_string()
                }
                (Some(_), _) => "The netplay session has no room for this client".to_string(),
                (None, None) => "Waiting for the netplay host...\nPlease wait...".to_string(),
                (None, Some(handshake)) if handshake.conflict(&connected_peers) => {
                    "Netplay player numbers conflict between clients".to_string()
                }
                (None, Some(handshake)) => format!(
                    "Still waiting for {} netplay player(s)...\nPlease wait...",
                    handshake.missing_players()
                ),
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
    // the loop above only ends with a complete handshake
    let handshake = handshake?;

    let mut session_builder = ggrs::SessionBuilder::<GgrsConfig>::new()
        .with_num_players(number_of_players)
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

    let channel = socket.take_channel(0).unwrap();
    let matchbox_channel = MatchboxChannel {
        delayed: TestPacketDelay::from_env(&channel),
        channel,
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
            number_of_players > 0,
            number_of_players > 1,
            number_of_players > 2,
            number_of_players > 3,
        ],
        inputs: vec![ui::input::InputData::default(); number_of_players],
        requests: std::collections::VecDeque::new(),
        frame_progress: FrameProgress::default(),
        pacing: FramePacing::default(),
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
    fn wait_recommendations_slew_and_expire() {
        let mut pacing = FramePacing::default();
        pacing.recommend_wait(3);
        for _ in 0..30 {
            let previous = pacing.extra_period;
            pacing.adjust(3);
            assert!(pacing.extra_period > 0.0 && pacing.extra_period <= 0.04);
            assert!(pacing.extra_period - previous <= 0.002 + f64::EPSILON);
        }
        assert!((0.0..3.0).contains(&pacing.wait_frames));
        pacing.recommend_wait(3);
        assert_eq!(pacing.wait_frames, 3.0);
        for _ in 0..100 {
            pacing.adjust(3);
        }
        assert_eq!(pacing.wait_frames, 0.0);

        pacing.recommend_wait(5);
        for _ in 0..20 {
            pacing.adjust(-1);
        }
        assert_eq!(pacing.wait_frames, 0.0);
        assert_eq!(pacing.extra_period, 0.0);
    }

    #[test]
    fn only_new_frames_are_paced_after_rollback() {
        let mut progress = FrameProgress::default();
        progress.advance();
        progress.advance();
        progress.load(0);
        for _ in 0..2 {
            progress.advance();
            assert!(progress.replaying);
        }
        progress.advance();
        assert!(!progress.replaying);
    }

    fn join(count: usize, game: &str) -> Vec<u8> {
        encode_auto_join(count, game)
    }

    fn slots_of(reply: Option<AutoReply>) -> Option<Vec<usize>> {
        match reply {
            Some(AutoReply::Slots(slots)) => Some(slots),
            _ => None,
        }
    }

    fn refusal_of(reply: Option<AutoReply>) -> Option<u8> {
        match reply {
            Some(AutoReply::Refused(code)) => Some(code),
            _ => None,
        }
    }

    #[test]
    fn host_hands_out_the_slots_after_its_own() {
        // the host plays two of three: one slot is left
        let mut assigner = SlotAssigner::<u8>::new(3, 2, "game");
        assert_eq!(
            slots_of(assigner.request(1, &join(1, "game"), &[1])),
            Some(vec![2])
        );
        // asking again gives the same answer
        assert_eq!(
            slots_of(assigner.request(1, &join(1, "game"), &[1])),
            Some(vec![2])
        );
        // and nobody else fits
        assert_eq!(
            refusal_of(assigner.request(2, &join(1, "game"), &[1, 2])),
            Some(REFUSED_FULL)
        );
    }

    #[test]
    fn joiners_share_the_remaining_slots() {
        let mut assigner = SlotAssigner::<u8>::new(4, 1, "game");
        assert_eq!(
            slots_of(assigner.request(1, &join(2, "game"), &[1, 2])),
            Some(vec![1, 2])
        );
        assert_eq!(
            slots_of(assigner.request(2, &join(1, "game"), &[1, 2])),
            Some(vec![3])
        );
        // a client that changes its mind gets new slots, if they fit
        assert_eq!(
            refusal_of(assigner.request(2, &join(2, "game"), &[1, 2])),
            Some(REFUSED_FULL)
        );
    }

    #[test]
    fn reconnecting_joiner_gets_its_slots_back() {
        let mut assigner = SlotAssigner::<u8>::new(3, 1, "game");
        assert_eq!(
            slots_of(assigner.request(1, &join(2, "game"), &[1])),
            Some(vec![1, 2])
        );
        // same client under a new id, the old one is gone
        assert_eq!(
            slots_of(assigner.request(9, &join(2, "game"), &[9])),
            Some(vec![1, 2])
        );
        // but not while the old id is still connected
        let mut assigner = SlotAssigner::<u8>::new(3, 1, "game");
        assigner.request(1, &join(2, "game"), &[1]);
        assert_eq!(
            refusal_of(assigner.request(9, &join(2, "game"), &[1, 9])),
            Some(REFUSED_FULL)
        );
    }

    #[test]
    fn different_game_or_bad_request_is_refused() {
        let mut assigner = SlotAssigner::<u8>::new(3, 1, "game");
        assert_eq!(
            refusal_of(assigner.request(1, &join(1, "other"), &[1])),
            Some(REFUSED_DIFFERENT_GAME)
        );
        assert_eq!(
            refusal_of(assigner.request(1, &join(0, "game"), &[1])),
            Some(REFUSED_FULL)
        );
        assert_eq!(
            refusal_of(assigner.request(1, &join(3, "game"), &[1])),
            Some(REFUSED_FULL)
        );
        // too short to be a request at all
        assert!(assigner.request(1, &[0; 4], &[1]).is_none());
        // none of this used up a slot
        assert_eq!(
            slots_of(assigner.request(1, &join(2, "game"), &[1])),
            Some(vec![1, 2])
        );
    }

    #[test]
    fn joiner_only_accepts_slots_that_make_sense() {
        assert_eq!(
            decode_auto_slots(&encode_auto_slots(3, &[2]), 1),
            Some((3, vec![2]))
        );
        assert_eq!(
            decode_auto_slots(&encode_auto_slots(4, &[1, 2]), 2),
            Some((4, vec![1, 2]))
        );
        // not the number of slots that was asked for
        assert_eq!(decode_auto_slots(&encode_auto_slots(3, &[1, 2]), 1), None);
        // out of range, duplicated, too many players, or nobody else left
        assert_eq!(decode_auto_slots(&encode_auto_slots(3, &[3]), 1), None);
        assert_eq!(decode_auto_slots(&encode_auto_slots(4, &[2, 2]), 2), None);
        assert_eq!(decode_auto_slots(&encode_auto_slots(5, &[4]), 1), None);
        assert_eq!(decode_auto_slots(&encode_auto_slots(1, &[0]), 1), None);
        assert_eq!(decode_auto_slots(&[], 1), None);
    }

    #[test]
    fn a_peer_only_holds_its_latest_claim() {
        // B and C swap slots after the host handed them out again
        let mut host = Handshake::<u8>::new(&[0], 3);
        deliver(&mut host, 2, MESSAGE_PLAYER_NUMBER, &[1], &[2, 3]);
        deliver(&mut host, 3, MESSAGE_PLAYER_NUMBER, &[2], &[2, 3]);
        assert!(host.complete(&[2, 3]));
        // the first new claim is refused, but frees the slot it held
        deliver(&mut host, 2, MESSAGE_PLAYER_NUMBER, &[2], &[2, 3]);
        assert!(host.conflict(&[2, 3]) && !host.complete(&[2, 3]));
        deliver(&mut host, 3, MESSAGE_PLAYER_NUMBER, &[1], &[2, 3]);
        deliver(&mut host, 2, MESSAGE_PLAYER_NUMBER, &[2], &[2, 3]);
        assert!(!host.conflict(&[2, 3]) && host.complete(&[2, 3]));
    }

    #[test]
    fn host_knows_which_claims_it_gave_out() {
        let mut assigner = SlotAssigner::<u8>::new(3, 1, "game");
        assigner.request(5, &join(2, "game"), &[5]);
        assert!(assigner.gave(5, &[1, 2]));
        assert!(!assigner.gave(5, &[1]));
        assert!(!assigner.gave(6, &[1, 2]));
    }

    #[test]
    fn host_and_joiner_agree_end_to_end() {
        // PC hosts two of three players, the Mac joins with one
        let mut assigner = SlotAssigner::<u8>::new(3, 2, "game");
        let reply = slots_of(assigner.request(2, &join(1, "game"), &[2])).unwrap();
        let (total, mac_slots) = decode_auto_slots(&encode_auto_slots(3, &reply), 1).unwrap();
        let mut pc = Handshake::<u8>::new(&[0, 1], 3);
        let mut mac = Handshake::<u8>::new(&mac_slots, total);
        deliver(&mut pc, 2, MESSAGE_PLAYER_SLOTS, &mac_slots, &[2]);
        deliver(&mut mac, 1, MESSAGE_PLAYER_SLOTS, &[0, 1], &[1]);
        deliver(&mut pc, 2, MESSAGE_PLAYER_NUMBER, &mac_slots, &[2]);
        deliver(&mut mac, 1, MESSAGE_PLAYER_NUMBER, &[0, 1], &[1]);
        assert!(pc.complete(&[2]) && mac.complete(&[1]));
    }

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
