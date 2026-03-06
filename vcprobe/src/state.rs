use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use protobuf::Message;
use videocall_types::protos::health_packet::HealthPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

// Mark participant as stale (grayed out) after no packets
// Reduced from 6s to 2s since we track all packet types (AUDIO/VIDEO at ~70/sec combined)
const STALE_THRESHOLD: Duration = Duration::from_secs(2);
// Remove from list entirely after this long
const PRUNE_THRESHOLD: Duration = Duration::from_secs(10);

/// Quality data about a participant as observed by one of their peers.
/// Populated from HEALTH packets. May be absent if no health data received yet.
#[derive(Debug, Clone)]
pub struct QualitySnapshot {
    /// Audio jitter buffer depth (ms) — lower is better
    pub jitter_ms: f64,
    /// Audio concealment rate (expand_per_sec) — the key quality signal.
    /// 0 = perfect, >5 = degraded, >10 = poor
    pub conceal_per_sec: f64,
    /// Video FPS as observed by a peer
    pub fps: f64,
    /// Video bitrate as observed by a peer (kbps)
    pub bitrate_kbps: u64,
    /// Whether the observer can see this participant's video
    pub can_see: bool,
    /// Whether the observer can hear this participant's audio
    pub can_listen: bool,
    pub updated_at: Instant,
}

#[derive(Debug)]
pub struct Participant {
    pub email: String,
    pub video_enabled: bool,
    pub audio_enabled: bool,
    pub last_heartbeat: Instant,
    /// RTT to server, self-reported in HEALTH packets
    pub rtt_ms: Option<f64>,
    /// Latest quality snapshot from any peer observing this participant
    pub quality: Option<QualitySnapshot>,
    pub _joined_at: Instant,
}

impl Participant {
    fn new(email: String) -> Self {
        Self {
            email,
            video_enabled: false,
            audio_enabled: false,
            last_heartbeat: Instant::now(),
            rtt_ms: None,
            quality: None,
            _joined_at: Instant::now(),
        }
    }

    pub fn is_stale(&self) -> bool {
        self.last_heartbeat.elapsed() > STALE_THRESHOLD
    }

    fn should_prune(&self) -> bool {
        self.last_heartbeat.elapsed() > PRUNE_THRESHOLD
    }

    pub fn quality_score(&self) -> Option<f64> {
        self.quality.as_ref().map(|q| {
            // 0.0 = perfect, 1.0 = terrible
            // Weighted: concealment is the most important signal
            let conceal_score = (q.conceal_per_sec / 10.0).min(1.0);
            let jitter_score = (q.jitter_ms / 500.0).min(1.0);
            let rtt_score = self
                .rtt_ms
                .map(|r| (r / 300.0).min(1.0))
                .unwrap_or(0.0);
            (conceal_score * 0.5 + jitter_score * 0.3 + rtt_score * 0.2).min(1.0)
        })
    }
}

#[derive(Debug)]
pub struct Event {
    /// Wall-clock time captured at event creation — fixed, not recomputed.
    pub when: DateTime<Local>,
    pub msg: String,
}

pub struct MeetingState {
    pub meeting_id: String,
    pub started_at: Instant,
    pub participants: HashMap<String, Participant>,
    pub events: Vec<Event>,
    /// Maps session IDs (numbers as strings) to email addresses
    /// Built from MEDIA packets to resolve HEALTH packet peer_stats keys
    session_to_email: HashMap<String, String>,
}

impl MeetingState {
    pub fn new(meeting_id: String) -> Self {
        Self {
            meeting_id,
            started_at: Instant::now(),
            participants: HashMap::new(),
            events: Vec::new(),
            session_to_email: HashMap::new(),
        }
    }

    pub fn process_packet(&mut self, raw: &[u8]) {
        let pkt = match PacketWrapper::parse_from_bytes(raw) {
            Ok(p) => p,
            Err(_) => return,
        };

        match pkt.packet_type.enum_value().unwrap_or(PacketType::PACKET_TYPE_UNKNOWN) {
            PacketType::MEDIA => self.process_media(&pkt),
            PacketType::HEALTH => self.process_health(&pkt),
            _ => {}
        }
    }

    fn process_media(&mut self, pkt: &PacketWrapper) {
        let media = match MediaPacket::parse_from_bytes(&pkt.data) {
            Ok(m) => m,
            Err(_) => return,
        };

        // email is in the MediaPacket; fall back to PacketWrapper email
        let email = if !media.email.is_empty() {
            media.email.clone()
        } else if !pkt.email.is_empty() {
            pkt.email.clone()
        } else {
            return;
        };

        // Track session_id → email mapping for HEALTH packet resolution
        if pkt.session_id > 0 {
            let session_key = pkt.session_id.to_string();
            self.session_to_email.insert(session_key, email.clone());
        }

        // Update last_heartbeat on ANY packet type for fast drop detection
        // At ~70 packets/sec combined (AUDIO+VIDEO), this enables ~1-2sec detection
        let is_new = !self.participants.contains_key(&email);
        let p = self
            .participants
            .entry(email.clone())
            .or_insert_with(|| Participant::new(email.clone()));
        p.last_heartbeat = Instant::now();

        // Process type-specific metadata
        if media.media_type.enum_value().unwrap_or(MediaType::MEDIA_TYPE_UNKNOWN) == MediaType::HEARTBEAT {
            if let Some(hb) = media.heartbeat_metadata.as_ref() {
                p.video_enabled = hb.video_enabled;
                p.audio_enabled = hb.audio_enabled;
            }
            if is_new {
                self.push_event(format!("{} joined", email));
            }
        }
    }

    fn process_health(&mut self, pkt: &PacketWrapper) {
        let health = match HealthPacket::parse_from_bytes(&pkt.data) {
            Ok(h) => h,
            Err(_) => return,
        };

        if health.reporting_peer.is_empty() {
            return;
        }

        // Update reporter's own RTT and mark as active
        if let Some(p) = self.participants.get_mut(&health.reporting_peer) {
            p.rtt_ms = Some(health.active_server_rtt_ms);
            p.last_heartbeat = Instant::now(); // HEALTH packets also indicate activity
        }

        // Update quality for each peer the reporter is observing
        for (peer_id, peer_stats) in &health.peer_stats {
            // peer_id might be an email or a session_id (numeric string)
            // Try direct lookup first, then session_to_email mapping
            let email = if self.participants.contains_key(peer_id) {
                peer_id.clone()
            } else if let Some(resolved) = self.session_to_email.get(peer_id) {
                resolved.clone()
            } else {
                // Unknown peer - skip
                continue;
            };

            if let Some(p) = self.participants.get_mut(&email) {
                let jitter_ms = peer_stats
                    .neteq_stats
                    .as_ref()
                    .map(|n| n.current_buffer_size_ms)
                    .unwrap_or(0.0);

                let conceal_per_sec = peer_stats
                    .neteq_stats
                    .as_ref()
                    .and_then(|n| n.network.as_ref())
                    .and_then(|net| net.operation_counters.as_ref())
                    .map(|op| op.expand_per_sec)
                    .unwrap_or(0.0);

                let fps = peer_stats
                    .video_stats
                    .as_ref()
                    .map(|v| v.fps_received)
                    .unwrap_or(0.0);

                let bitrate_kbps = peer_stats
                    .video_stats
                    .as_ref()
                    .map(|v| v.bitrate_kbps)
                    .unwrap_or(0);

                p.quality = Some(QualitySnapshot {
                    jitter_ms,
                    conceal_per_sec,
                    fps,
                    bitrate_kbps,
                    can_see: peer_stats.can_see,
                    can_listen: peer_stats.can_listen,
                    updated_at: Instant::now(),
                });
            }
        }
    }

    /// Called every second: prune participants that have been silent too long.
    pub fn tick(&mut self) {
        let stale: Vec<String> = self
            .participants
            .values()
            .filter(|p| p.should_prune())
            .map(|p| p.email.clone())
            .collect();

        for email in stale {
            self.participants.remove(&email);
            self.push_event(format!("{} left", email));
        }
    }

    fn push_event(&mut self, msg: String) {
        self.events.push(Event {
            when: Local::now(),
            msg,
        });
        if self.events.len() > 50 {
            self.events.remove(0);
        }
    }

    /// Participants sorted alphabetically for stable table ordering.
    pub fn sorted_participants(&self, by_quality: bool) -> Vec<&Participant> {
        let mut v: Vec<&Participant> = self.participants.values().collect();

        if by_quality {
            // Sort by quality (worst-first), then by email
            v.sort_by(|a, b| {
                let score_a = a.quality_score().unwrap_or(0.0);
                let score_b = b.quality_score().unwrap_or(0.0);

                // Higher score = worse quality, so reverse for worst-first
                match score_b.partial_cmp(&score_a) {
                    Some(std::cmp::Ordering::Equal) | None => a.email.cmp(&b.email),
                    Some(ord) => ord,
                }
            });
        } else {
            // Alphabetical by email
            v.sort_by(|a, b| a.email.cmp(&b.email));
        }

        v
    }

    pub fn elapsed_str(&self) -> String {
        let secs = self.started_at.elapsed().as_secs();
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        if h > 0 {
            format!("{}:{:02}:{:02}", h, m, s)
        } else {
            format!("{:02}:{:02}", m, s)
        }
    }
}
