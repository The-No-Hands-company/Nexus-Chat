//! SFU (Selective Forwarding Unit) engine — the heart of Nexus voice.
//!
//! Architecture:
//! ```text
//!   Client A ──WebRTC──▶ SFU ──WebRTC──▶ Client B
//!   Client B ──WebRTC──▶ SFU ──WebRTC──▶ Client A
//!   Client C ──WebRTC──▶ SFU ──WebRTC──▶ Client A, Client B
//! ```
//!
//! Each participant has one WebRTC connection to the SFU.
//! The SFU receives media from each participant and forwards it to all others.
//! NO transcoding or mixing — client handles volume, the server just routes packets.
//!
//! Benefits over P2P (mesh):
//! - Each client only uploads once (saves bandwidth)
//! - Server can do last-N selection (only forward active speakers)
//! - Server can handle bitrate adaptation per-receiver
//! - Scales to 100+ participants
//!
//! Uses `str0m` for WebRTC in Sans-IO style:
//! - We drive the I/O (UDP sockets) ourselves
//! - str0m handles DTLS, SRTP, ICE, SDP negotiation
//! - We get full control over packet routing
//!
//! ## RTP Forwarding Loop
//!
//! All peer UDP sockets feed into a single shared channel
//! `(PeerId, Vec<u8>, SocketAddr)`.  The room task selects between
//! inbound commands and arriving UDP packets, drives the str0m
//! state machine for the relevant peer, drains all pending str0m
//! outputs (UDP transmits + events), and on `Event::MediaData`
//! writes the payload into every other peer's writer for the
//! pre-negotiated forwarding Mid.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use str0m::change::SdpOffer;
use str0m::media::{MediaData, MediaKind, Mid};
use str0m::net::{DatagramRecv, Receive as NetReceive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcError};
use tokio::net::UdpSocket;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

/// Unique identifier for a peer connection within the SFU.
pub type PeerId = Uuid;

/// Channel type: all peers' UDP packets arrive here tagged with their PeerId.
type SharedMediaTx = mpsc::Sender<(PeerId, Vec<u8>, SocketAddr)>;

/// Information about a published media track.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub peer_id: PeerId,
    pub user_id: Uuid,
    pub mid: Mid,
    pub kind: MediaKind,
    pub label: TrackLabel,
}

/// What this track carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackLabel {
    Audio,
    Video,
    ScreenShareVideo,
    ScreenShareAudio,
}

/// Commands sent to the SFU room task.
#[derive(Debug)]
pub enum SfuCommand {
    /// Add a new peer with their SDP offer.
    AddPeer {
        peer_id: PeerId,
        user_id: Uuid,
        offer_sdp: String,
        reply: mpsc::Sender<SfuResponse>,
    },
    /// Remove a peer (disconnected or left).
    RemovePeer { peer_id: PeerId },
    /// Relay an ICE candidate from signaling.
    IceCandidate { peer_id: PeerId, candidate: String },
    /// Update a peer's media state (mute track, add screen share, etc.).
    UpdateMedia {
        peer_id: PeerId,
        audio_enabled: Option<bool>,
        video_enabled: Option<bool>,
    },
    /// Get room statistics.
    GetStats { reply: mpsc::Sender<SfuResponse> },
    /// Shutdown the room.
    Shutdown,
}

/// Responses from the SFU room task.
#[derive(Debug)]
pub enum SfuResponse {
    /// SDP answer to send back to the peer.
    Answer { sdp: String },
    /// Room stats.
    Stats(RoomStats),
    /// Error occurred.
    Error(String),
}

/// Statistics for an SFU room.
#[derive(Debug, Clone, Serialize)]
pub struct RoomStats {
    pub channel_id: Uuid,
    pub peer_count: usize,
    pub audio_tracks: usize,
    pub video_tracks: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// SfuManager

/// Manages all SFU rooms across the voice server.
#[derive(Clone)]
pub struct SfuManager {
    rooms: Arc<RwLock<HashMap<Uuid, mpsc::Sender<SfuCommand>>>>,
    local_ip: std::net::IpAddr,
}

impl SfuManager {
    pub fn new(local_ip: std::net::IpAddr) -> Self {
        Self {
            rooms: Arc::new(RwLock::new(HashMap::new())),
            local_ip,
        }
    }

    /// Get or create an SFU room for a voice channel.
    pub async fn get_or_create_room(&self, channel_id: Uuid) -> mpsc::Sender<SfuCommand> {
        {
            let rooms = self.rooms.read().await;
            if let Some(sender) = rooms.get(&channel_id) {
                return sender.clone();
            }
        }

        let mut rooms = self.rooms.write().await;
        if let Some(sender) = rooms.get(&channel_id) {
            return sender.clone();
        }

        let (cmd_tx, cmd_rx) = mpsc::channel::<SfuCommand>(256);
        let local_ip = self.local_ip;
        let rooms_ref = self.rooms.clone();

        tokio::spawn(async move {
            run_sfu_room(channel_id, cmd_rx, local_ip).await;
            rooms_ref.write().await.remove(&channel_id);
            tracing::info!(channel = %channel_id, "SFU room shut down");
        });

        rooms.insert(channel_id, cmd_tx.clone());
        tracing::info!(channel = %channel_id, "SFU room created");
        cmd_tx
    }

    /// Shut down a room and remove it from the registry.
    pub async fn remove_room(&self, channel_id: Uuid) {
        let mut rooms = self.rooms.write().await;
        if let Some(sender) = rooms.remove(&channel_id) {
            let _ = sender.send(SfuCommand::Shutdown).await;
        }
    }

    pub async fn active_room_count(&self) -> usize {
        self.rooms.read().await.len()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ActivePeer — internal room state

/// An active peer in an SFU room.
struct ActivePeer {
    peer_id: PeerId,
    _user_id: Uuid,
    /// str0m Sans-IO WebRTC instance.
    rtc: Rtc,
    /// UDP socket bound exclusively for this peer.
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
    /// Mids this peer publishes (client→server direction): audio, video,
    /// screen-share tracks we are receiving from them.
    /// Stored as (mid, kind) so forwarding can declare the correct MediaKind.
    recv_mids: Vec<(Mid, MediaKind)>,
    /// For each (source_peer_id, source_mid) pair, the outgoing Mid in *this*
    /// peer's RTC connection that forwards that source track.
    /// Populated in `setup_forwarding_tracks` after every peer change.
    forward_mids: HashMap<(PeerId, Mid), Mid>,
    has_video: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Room event loop

/// Run the SFU room event loop.
///
/// Three-arm select:
/// 1. Commands from the API / signaling layer.
/// 2. UDP packets arriving from any peer's socket (funnelled into a single
///    shared channel so the select stays manageable).
/// 3. A periodic 100 ms tick to drive str0m ICE/DTLS keep-alives and timeouts.
///
/// On receiving a UDP packet from peer X:
///   • Feed bytes into peer X's `Rtc` via `handle_input`.
///   • Drain all pending `Rtc::poll_output()` results:
///       – `Output::Transmit` → send UDP back to the peer.
///       – `Output::Timeout`  → update the next-tick deadline.
///       – `Output::Event(Event::MediaData)` → collect for forwarding.
///   • For every collected `MediaData`, write it into each other peer's
///     writer for the pre-declared forwarding Mid.
async fn run_sfu_room(
    channel_id: Uuid,
    mut cmd_rx: mpsc::Receiver<SfuCommand>,
    local_ip: std::net::IpAddr,
) {
    // Single channel for all peers' UDP packets.
    // Each per-peer recv task sends (peer_id, raw_bytes, src_addr) here.
    let (shared_tx, mut shared_rx) = mpsc::channel::<(PeerId, Vec<u8>, SocketAddr)>(4096);

    let mut peers: HashMap<PeerId, ActivePeer> = HashMap::new();
    // forward_table[(source_peer, source_mid)] = list of dest peer ids
    let mut forward_table: HashMap<(PeerId, Mid), Vec<PeerId>> = HashMap::new();
    let mut next_tick = tokio::time::Instant::now() + Duration::from_millis(100);

    loop {
        tokio::select! {
            // ── Signaling commands ──────────────────────────────────────────
            cmd = cmd_rx.recv() => {
                let cmd = match cmd { Some(c) => c, None => break };
                match cmd {
                    SfuCommand::AddPeer { peer_id, user_id, offer_sdp, reply } => {
                        match create_peer(peer_id, user_id, &offer_sdp, local_ip, shared_tx.clone()).await {
                            Ok((mut peer, answer_sdp)) => {
                                tracing::info!(
                                    channel = %channel_id, peer = %peer_id, "Peer added"
                                );
                                // Register forwarding tracks: this peer gains outgoing
                                // mids for all existing peers' recv tracks, and all
                                // existing peers gain an outgoing mid for this peer's tracks.
                                setup_forwarding_tracks(&mut peers, &mut peer, &mut forward_table);
                                peers.insert(peer_id, peer);
                                let _ = reply.send(SfuResponse::Answer { sdp: answer_sdp }).await;
                            }
                            Err(e) => {
                                tracing::error!(
                                    channel = %channel_id, peer = %peer_id,
                                    error_kind = %nexus_common::logsafe::err_kind(&e), "Failed to create peer"
                                );
                                let _ = reply.send(SfuResponse::Error(e.to_string())).await;
                            }
                        }
                    }

                    SfuCommand::RemovePeer { peer_id } => {
                        if peers.remove(&peer_id).is_some() {
                            // Remove all forward_table entries that involve this peer.
                            forward_table.retain(|k, dests| {
                                dests.retain(|d| *d != peer_id);
                                k.0 != peer_id && !dests.is_empty()
                            });
                            // Remove forward_mids referencing the departed peer from all others.
                            for peer in peers.values_mut() {
                                peer.forward_mids.retain(|(src, _), _| *src != peer_id);
                            }
                            tracing::info!(channel = %channel_id, peer = %peer_id, "Peer removed");
                        }
                        if peers.is_empty() {
                            tracing::info!(channel = %channel_id, "Room empty, shutting down");
                            break;
                        }
                    }

                    SfuCommand::IceCandidate { peer_id, candidate } => {
                        if let Some(peer) = peers.get_mut(&peer_id) {
                            match Candidate::from_sdp_string(&candidate) {
                                Ok(cand) => { peer.rtc.add_remote_candidate(cand); }
                                Err(e) => tracing::warn!(peer = %peer_id, error_kind = %nexus_common::logsafe::err_kind(&e), "Bad ICE candidate"),
                            }
                        }
                    }

                    SfuCommand::UpdateMedia { peer_id, audio_enabled, video_enabled } => {
                        if let Some(peer) = peers.get_mut(&peer_id) {
                            if let Some(has_video) = video_enabled {
                                peer.has_video = has_video;
                            }
                            tracing::debug!(
                                peer = %peer_id,
                                audio = ?audio_enabled,
                                video = ?video_enabled,
                                "Media update"
                            );
                        }
                    }

                    SfuCommand::GetStats { reply } => {
                        let stats = RoomStats {
                            channel_id,
                            peer_count: peers.len(),
                            audio_tracks: peers.len(),
                            video_tracks: peers.values().filter(|p| p.has_video).count(),
                        };
                        let _ = reply.send(SfuResponse::Stats(stats)).await;
                    }

                    SfuCommand::Shutdown => {
                        tracing::info!(channel = %channel_id, "Shutdown command");
                        break;
                    }
                }
            }

            // ── Inbound UDP from any peer ───────────────────────────────────
            packet = shared_rx.recv() => {
                let (peer_id, data, src) = match packet {
                    Some(p) => p,
                    None => break, // Channel closed → room task should exit too
                };

                // Collect media events emitted by str0m after feeding this packet.
                let mut media_events: Vec<MediaData> = Vec::new();

                if let Some(peer) = peers.get_mut(&peer_id) {
                    let now = Instant::now();
                    match DatagramRecv::try_from(data.as_slice()) {
                        Err(e) => tracing::warn!(peer = %peer_id, error_kind = %nexus_common::logsafe::err_kind(&e), "DatagramRecv parse error"),
                        Ok(contents) => {
                            let recv = NetReceive {
                                proto: str0m::net::Protocol::Udp,
                                source: src,
                                destination: peer.local_addr,
                                contents,
                            };
                            match peer.rtc.handle_input(Input::Receive(now, recv)) {
                                Err(e) => tracing::warn!(peer = %peer_id, error_kind = %nexus_common::logsafe::err_kind(&e), "RTC input error"),
                                Ok(()) => drain_rtc(peer, &mut media_events).await,
                            }
                        }
                    }
                }

                // Forward collected media to all other peers.
                forward_media(&mut peers, &forward_table, peer_id, media_events).await;
            }

            // ── Periodic tick: drive ICE/DTLS keep-alives ──────────────────
            _ = tokio::time::sleep_until(next_tick) => {
                let now = Instant::now();
                let mut dead_peers: Vec<PeerId> = Vec::new();

                for (pid, peer) in peers.iter_mut() {
                    if let Err(e) = peer.rtc.handle_input(Input::Timeout(now)) {
                        tracing::warn!(peer = %pid, error_kind = %nexus_common::logsafe::err_kind(&e), "RTC timeout input error; removing peer");
                        dead_peers.push(*pid);
                        continue;
                    }
                    drain_rtc(peer, &mut Vec::new()).await;
                }

                for pid in dead_peers {
                    peers.remove(&pid);
                    forward_table.retain(|k, dests| {
                        dests.retain(|d| *d != pid);
                        k.0 != pid && !dests.is_empty()
                    });
                }

                next_tick = tokio::time::Instant::now() + Duration::from_millis(100);
            }
        }
    }

    tracing::info!(channel = %channel_id, "SFU room event loop exited");
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers

/// Drain all pending outputs from `peer.rtc`.
///
/// - `Output::Transmit` packets are sent immediately via the peer's UDP socket.
/// - `Output::Event(Event::MediaData)` items are appended to `out`.
/// - `Output::Timeout` is ignored here; the tick arm handles scheduling.
/// - Other events (Connected, Disconnected, MediaAdded, …) are logged at DEBUG.
async fn drain_rtc(peer: &mut ActivePeer, out: &mut Vec<MediaData>) {
    loop {
        match peer.rtc.poll_output() {
            // Timeout is the sentinel meaning "no more outputs right now".
            Ok(Output::Timeout(_)) => break,
            Ok(Output::Transmit(tx)) => {
                if let Err(e) = peer.socket.send_to(&tx.contents, tx.destination).await {
                    tracing::warn!(peer = %peer.peer_id, error_kind = %nexus_common::logsafe::err_kind(&e), "UDP send error");
                }
            }
            Ok(Output::Event(event)) => match event {
                Event::Connected => {
                    tracing::info!(peer = %peer.peer_id, "WebRTC connected (DTLS established)");
                }
                Event::MediaAdded(added) => {
                    tracing::debug!(
                        peer = %peer.peer_id,
                        mid = ?added.mid,
                        kind = ?added.kind,
                        "Media track added"
                    );
                    if !peer.recv_mids.iter().any(|(m, _)| *m == added.mid) {
                        peer.recv_mids.push((added.mid, added.kind));
                    }
                    if matches!(added.kind, MediaKind::Video) {
                        peer.has_video = true;
                    }
                }
                Event::MediaData(media) => {
                    out.push(media);
                }
                Event::PeerStats(_) => {}
                Event::MediaEgressStats(_) => {}
                Event::MediaIngressStats(_) => {}
                other => {
                    tracing::debug!(peer = %peer.peer_id, event = ?other, "RTC event");
                }
            },
            Err(e) => {
                tracing::error!(peer = %peer.peer_id, error_kind = %nexus_common::logsafe::err_kind(&e), "RTC poll_output error");
                break;
            }
        }
    }
}

/// Write all `media_events` (originating from `source_peer_id`) into every
/// other peer's RTC writer for the pre-declared forwarding Mid.
async fn forward_media(
    peers: &mut HashMap<PeerId, ActivePeer>,
    forward_table: &HashMap<(PeerId, Mid), Vec<PeerId>>,
    source_peer_id: PeerId,
    media_events: Vec<MediaData>,
) {
    for media in media_events {
        let src_key = (source_peer_id, media.mid);
        let destinations = match forward_table.get(&src_key) {
            Some(d) => d.clone(),
            None => continue,
        };

        for dest_peer_id in &destinations {
            let dest = match peers.get_mut(dest_peer_id) {
                Some(p) => p,
                None => continue,
            };

            let fwd_mid = match dest.forward_mids.get(&src_key) {
                Some(m) => *m,
                None => continue,
            };

            if let Some(writer) = dest.rtc.writer(fwd_mid)
                && let Err(e) =
                    writer.write(media.pt, media.network_time, media.time, media.data.clone())
            {
                tracing::warn!(
                    dest_peer = %dest_peer_id,
                    fwd_mid = ?fwd_mid,
                    error_kind = %nexus_common::logsafe::err_kind(&e),
                    "Failed to forward media"
                );
            }
        }
    }
}

/// When a new peer joins, wire up the forwarding Mids between it and all
/// existing peers.
///
/// For each existing peer E with recv_mids [m0, m1, …]:
///   • Add a send track for (E.peer_id, m_i) in the new peer's RTC.
///   • Record the resulting forwarding mid in new_peer.forward_mids.
///   • Add new_peer.peer_id to forward_table[(E.peer_id, m_i)].
///
/// For each recv_mid the new peer publishes:
///   • Add a send track for (new_peer.peer_id, m_j) in every existing peer.
///   • Record the forwarding mid in existing_peer.forward_mids.
///   • Add existing_peer.peer_id to forward_table[(new_peer.peer_id, m_j)].
///
/// Note: `sdp_api().add_media()` registers the intent. The SDP negotiation
/// change commit triggers re-offer to the client via the signaling path (out
/// of band from the SFU loop). For now the mids are declared into the RTC
/// instance so that `rtc.writer(mid, pt)` succeeds once DTLS is up.
fn setup_forwarding_tracks(
    existing_peers: &mut HashMap<PeerId, ActivePeer>,
    new_peer: &mut ActivePeer,
    forward_table: &mut HashMap<(PeerId, Mid), Vec<PeerId>>,
) {
    // ── new peer subscribes to all existing publishers ────────────────────
    for existing in existing_peers.values_mut() {
        for &(src_mid, kind) in &existing.recv_mids {
            let src_key = (existing.peer_id, src_mid);

            // Declare the forwarded media in the new peer RTC so writer(mid)
            // can be resolved once DTLS is established.
            let mut api = new_peer.rtc.direct_api();
            api.declare_media(src_mid, kind);

            new_peer.forward_mids.insert(src_key, src_mid);

            let dests = forward_table.entry(src_key).or_default();
            if !dests.contains(&new_peer.peer_id) {
                dests.push(new_peer.peer_id);
            }
        }
    }

    // ── existing peers subscribe to the new peer's tracks ─────────────────
    for &(new_mid, new_kind) in &new_peer.recv_mids {
        let src_key = (new_peer.peer_id, new_mid);

        for existing in existing_peers.values_mut() {
            // Declare an outgoing track in the existing peer's RTC so that
            // writer(mid) succeeds once DTLS is established.
            existing.forward_mids.insert(src_key, new_mid);

            let dests = forward_table.entry(src_key).or_default();
            if !dests.contains(&existing.peer_id) {
                dests.push(existing.peer_id);
            }

            let mut api = existing.rtc.direct_api();
            api.declare_media(new_mid, new_kind);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Peer creation

/// Create a new WebRTC peer connection from an SDP offer.
///
/// Binds a UDP socket, creates the `str0m::Rtc` instance, accepts the SDP
/// offer, and spawns a UDP receive task that funnels all arriving packets into
/// `shared_tx` tagged with `peer_id`.
async fn create_peer(
    peer_id: PeerId,
    user_id: Uuid,
    offer_sdp: &str,
    local_ip: std::net::IpAddr,
    shared_tx: SharedMediaTx,
) -> Result<(ActivePeer, String), SfuError> {
    let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0)).await?;
    let local_addr = socket.local_addr()?;
    let socket = Arc::new(socket);

    tracing::debug!(peer = %peer_id, addr = %local_addr, "UDP socket bound");

    let start = Instant::now();
    let mut rtc = Rtc::builder().set_ice_lite(true).build(start);

    let candidate = Candidate::host(local_addr, str0m::net::Protocol::Udp)
        .map_err(|e| SfuError::Sdp(e.to_string()))?;
    rtc.add_local_candidate(candidate);

    let offer = SdpOffer::from_sdp_string(offer_sdp).map_err(|e| SfuError::Sdp(e.to_string()))?;

    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| SfuError::Sdp(e.to_string()))?;

    let answer_sdp = answer.to_sdp_string();

    // Spawn the UDP receive loop for this peer.
    // All raw bytes arrive in `shared_tx` tagged with `peer_id`.
    let recv_socket = socket.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        loop {
            match recv_socket.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    let pkt = buf[..len].to_vec();
                    if shared_tx.send((peer_id, pkt, src)).await.is_err() {
                        // Room task exited; stop receiving.
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(peer = %peer_id, error_kind = %nexus_common::logsafe::err_kind(&e), "UDP recv error; peer loop exiting");
                    break;
                }
            }
        }
    });

    let peer = ActivePeer {
        peer_id,
        _user_id: user_id,
        rtc,
        socket,
        local_addr,
        recv_mids: Vec::<(Mid, MediaKind)>::new(),
        forward_mids: HashMap::new(),
        has_video: false,
    };

    Ok((peer, answer_sdp))
}

// ─────────────────────────────────────────────────────────────────────────────
// Errors

/// SFU-specific errors.
#[derive(Debug, thiserror::Error)]
pub enum SfuError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("WebRTC error: {0}")]
    Rtc(#[from] RtcError),

    #[error("SDP parse error: {0}")]
    Sdp(String),

    #[error("Peer not found: {0}")]
    PeerNotFound(PeerId),

    #[error("Room is full (max {0} participants)")]
    RoomFull(usize),
}

#[cfg(test)]
mod forwarding_tests {
    //! Routing correctness for the SFU's media graph.
    //!
    //! These do not prove RTP bytes traverse the wire — that needs a DTLS
    //! handshake between two str0m instances and belongs in an integration
    //! test. What they prove is the layer above, which is where an SFU's own
    //! bugs live: who is wired to hear whom. A selective forwarding unit that
    //! routes a peer's audio back to itself, or fails to wire a late joiner to
    //! the people already talking, is broken regardless of how well the media
    //! layer works underneath.
    //!
    //! Before these, `sfu.rs` had no tests at all.

    use super::*;

    /// A peer with a real `Rtc` and a real bound socket, so the code under test
    /// runs against the types it does in production. Loopback port 0 — the
    /// kernel picks a free port and nothing is ever sent.
    async fn peer(id: PeerId, mids: &[(Mid, MediaKind)]) -> ActivePeer {
        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback socket");
        let local_addr = socket.local_addr().expect("read back the bound address");
        ActivePeer {
            peer_id: id,
            _user_id: Uuid::new_v4(),
            // Mirrors the production construction in the room loop.
            rtc: Rtc::builder().set_ice_lite(true).build(Instant::now()),
            socket: Arc::new(socket),
            local_addr,
            recv_mids: mids.to_vec(),
            forward_mids: HashMap::new(),
            has_video: false,
        }
    }

    fn mid(s: &str) -> Mid {
        Mid::from(s)
    }

    /// Join `n` peers one at a time, exactly as the room does, and return the
    /// resulting graph.
    #[allow(clippy::type_complexity)]
    async fn join_all(
        n: usize,
    ) -> (
        HashMap<PeerId, ActivePeer>,
        HashMap<(PeerId, Mid), Vec<PeerId>>,
        Vec<PeerId>,
    ) {
        let mut peers: HashMap<PeerId, ActivePeer> = HashMap::new();
        let mut table: HashMap<(PeerId, Mid), Vec<PeerId>> = HashMap::new();
        // Join order, kept explicitly: HashMap iteration order says nothing
        // about who arrived last, and the late-joiner case depends on it.
        let mut order: Vec<PeerId> = Vec::new();

        for i in 0..n {
            let id = Uuid::new_v4();
            // One audio track each, named per peer so sources stay distinct.
            let mut joining = peer(id, &[(mid(&i.to_string()), MediaKind::Audio)]).await;
            setup_forwarding_tracks(&mut peers, &mut joining, &mut table);
            peers.insert(id, joining);
            order.push(id);
        }
        (peers, table, order)
    }

    #[tokio::test]
    async fn a_peer_is_never_routed_its_own_media() {
        // Echo is the defining failure of an SFU. If this ever passes silently
        // everyone hears themselves back with a round-trip of delay.
        let (_peers, table, _order) = join_all(3).await;
        assert!(!table.is_empty(), "three peers must produce routes");
        for ((source, _mid), destinations) in &table {
            assert!(
                !destinations.contains(source),
                "peer {source} is routed its own media"
            );
        }
    }

    #[tokio::test]
    async fn every_peer_reaches_every_other_peer() {
        let n = 3;
        let (peers, table, _order) = join_all(n).await;
        for id in peers.keys() {
            let destinations: Vec<_> = table
                .iter()
                .filter(|((src, _), _)| src == id)
                .flat_map(|(_, d)| d.iter().copied())
                .collect();
            assert_eq!(
                destinations.len(),
                n - 1,
                "peer {id} should reach the other {} peers, reached {:?}",
                n - 1,
                destinations
            );
        }
    }

    #[tokio::test]
    async fn a_late_joiner_is_wired_in_both_directions() {
        // The asymmetric case, and the easy one to get wrong: it is simple to
        // subscribe a new peer to existing publishers and forget that the
        // people already in the room must also start receiving the newcomer.
        let (peers, table, order) = join_all(3).await;
        let last = *order.last().expect("a peer exists");

        let outbound = table.iter().any(|((src, _), d)| *src == last && !d.is_empty());
        assert!(outbound, "the newest peer must be heard by the room");

        let inbound = table.iter().any(|((src, _), d)| *src != last && d.contains(&last));
        assert!(inbound, "the newest peer must hear the room");
    }

    #[tokio::test]
    async fn each_destination_has_a_local_mid_for_every_source_it_receives() {
        // forward_media drops any media whose (source, mid) has no entry in the
        // destination's forward_mids. A route in the table without a matching
        // mid is therefore a silent black hole, not an error.
        let (peers, table, _order) = join_all(3).await;
        for ((source, source_mid), destinations) in &table {
            for dest_id in destinations {
                let dest = peers.get(dest_id).expect("destination is a known peer");
                assert!(
                    dest.forward_mids.contains_key(&(*source, *source_mid)),
                    "peer {dest_id} is routed ({source}, {source_mid}) but has no mid to write it to"
                );
            }
        }
    }
}
