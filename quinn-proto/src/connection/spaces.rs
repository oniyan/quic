use std::{
    cmp,
    collections::{BTreeMap, HashSet, VecDeque},
    mem,
    ops::{Index, IndexMut},
    time::Instant,
};

use super::assembler::Assembler;
use crate::{
    crypto, crypto::Keys, frame, packet::SpaceId, range_set::RangeSet, shared::IssuedCid, StreamId,
    VarInt,
};

pub(crate) struct PacketSpace<S>
where
    S: crypto::Session,
{
    pub(crate) crypto: Option<Keys<S>>,
    pub(crate) dedup: Dedup,
    /// Highest received packet number
    pub(crate) rx_packet: u64,
    /// Time at which the above was received
    pub(crate) rx_packet_time: Instant,

    /// Data to send
    pub(crate) pending: Retransmits,
    /// Packet numbers to acknowledge
    pub(crate) pending_acks: RangeSet,
    /// Set iff we have received a non-ack frame since the last ack-only packet we sent
    pub(crate) permit_ack_only: bool,

    /// The packet number of the next packet that will be sent, if any.
    pub(crate) next_packet_number: u64,
    /// The largest packet number the remote peer acknowledged in an ACK frame.
    pub(crate) largest_acked_packet: Option<u64>,
    pub(crate) largest_acked_packet_sent: Instant,
    /// Transmitted but not acked
    // We use a BTreeMap here so we can efficiently query by range on ACK and for loss detection
    pub(crate) sent_packets: BTreeMap<u64, SentPacket>,
    /// Number of explicit congestion notification codepoints seen on incoming packets
    pub(crate) ecn_counters: frame::EcnCounts,
    /// Recent ECN counters sent by the peer in ACK frames
    ///
    /// Updated (and inspected) whenever we receive an ACK with a new highest acked packet
    /// number. Stored per-space to simplify verification, which would otherwise have difficulty
    /// distinguishing between ECN bleaching and counts having been updated by a near-simultaneous
    /// ACK already processed in another space.
    pub(crate) ecn_feedback: frame::EcnCounts,

    /// Incoming cryptographic handshake stream
    pub(crate) crypto_stream: Assembler,
    /// Current offset of outgoing cryptographic handshake stream
    pub(crate) crypto_offset: u64,

    /// The time the most recently sent retransmittable packet was sent.
    pub(crate) time_of_last_ack_eliciting_packet: Option<Instant>,
    /// The time at which the earliest sent packet in this space will be considered lost based on
    /// exceeding the reordering window in time. Only set for packets numbered prior to a packet
    /// that has been acknowledged.
    pub(crate) loss_time: Option<Instant>,
    /// Number of tail loss probes to send
    pub(crate) loss_probes: u32,
    pub(crate) ping_pending: bool,
    /// Number of congestion control "in flight" bytes
    pub(crate) in_flight: u64,
}

impl<S> PacketSpace<S>
where
    S: crypto::Session,
{
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            crypto: None,
            dedup: Dedup::new(),
            rx_packet: 0,
            rx_packet_time: now,

            pending: Retransmits::default(),
            pending_acks: RangeSet::new(),
            permit_ack_only: false,

            next_packet_number: 0,
            largest_acked_packet: None,
            largest_acked_packet_sent: now,
            sent_packets: BTreeMap::new(),
            ecn_counters: frame::EcnCounts::ZERO,
            ecn_feedback: frame::EcnCounts::ZERO,

            crypto_stream: Assembler::new(),
            crypto_offset: 0,

            time_of_last_ack_eliciting_packet: None,
            loss_time: None,
            loss_probes: 0,
            ping_pending: false,
            in_flight: 0,
        }
    }

    pub(crate) fn get_tx_number(&mut self) -> u64 {
        // TODO: Handle packet number overflow gracefully
        assert!(self.next_packet_number < 2u64.pow(62));
        let x = self.next_packet_number;
        self.next_packet_number += 1;
        x
    }

    pub(crate) fn can_send(&self) -> bool {
        !self.pending.is_empty()
            || (self.permit_ack_only && !self.pending_acks.is_empty())
            || self.ping_pending
    }

    /// Verifies sanity of an ECN block and returns whether congestion was encountered.
    pub(crate) fn detect_ecn(
        &mut self,
        newly_acked: u64,
        ecn: frame::EcnCounts,
    ) -> Result<bool, &'static str> {
        let ect0_increase = ecn
            .ect0
            .checked_sub(self.ecn_feedback.ect0)
            .ok_or("peer ECT(0) count regression")?;
        let ect1_increase = ecn
            .ect1
            .checked_sub(self.ecn_feedback.ect1)
            .ok_or("peer ECT(1) count regression")?;
        let ce_increase = ecn
            .ce
            .checked_sub(self.ecn_feedback.ce)
            .ok_or("peer CE count regression")?;
        let total_increase = ect0_increase + ect1_increase + ce_increase;
        if total_increase < newly_acked {
            return Err("ECN bleaching");
        }
        if (ect0_increase + ce_increase) < newly_acked || ect1_increase != 0 {
            return Err("ECN corruption");
        }
        // If total_increase > newly_acked (which happens when ACKs are lost), this is required by
        // the draft so that long-term drift does not occur. If =, then the only question is whether
        // to count CE packets as CE or ECT0. Recording them as CE is more consistent and keeps the
        // congestion check obvious.
        self.ecn_feedback = ecn;
        Ok(ce_increase != 0)
    }

    pub(crate) fn sent(&mut self, number: u64, packet: SentPacket) {
        self.in_flight += u64::from(packet.size);
        self.sent_packets.insert(number, packet);
    }
}

impl<S: crypto::Session> Index<SpaceId> for [PacketSpace<S>; 3] {
    type Output = PacketSpace<S>;
    fn index(&self, space: SpaceId) -> &PacketSpace<S> {
        &self.as_ref()[space as usize]
    }
}

impl<S: crypto::Session> IndexMut<SpaceId> for [PacketSpace<S>; 3] {
    fn index_mut(&mut self, space: SpaceId) -> &mut PacketSpace<S> {
        &mut self.as_mut()[space as usize]
    }
}

/// Represents one or more packets subject to retransmission
#[derive(Debug, Clone)]
pub(crate) struct SentPacket {
    /// The time the packet was sent.
    pub(crate) time_sent: Instant,
    /// The number of bytes sent in the packet, not including UDP or IP overhead, but including QUIC
    /// framing overhead. Zero if this packet is not counted towards congestion control, i.e. not an
    /// "in flight" packet.
    pub(crate) size: u16,
    /// Whether an acknowledgement is expected directly in response to this packet.
    pub(crate) ack_eliciting: bool,
    pub(crate) acks: RangeSet,
    pub(crate) retransmits: Retransmits,
    /// Metadata for stream frames in a packet
    ///
    /// The actual application data is stored with the stream state.
    pub(crate) stream_frames: Vec<frame::StreamMeta>,
}

/// Retransmittable data queue
#[derive(Debug, Clone)]
pub struct Retransmits {
    pub(crate) max_data: bool,
    pub(crate) max_uni_stream_id: bool,
    pub(crate) max_bi_stream_id: bool,
    pub(crate) reset_stream: Vec<(StreamId, VarInt)>,
    pub(crate) stop_sending: Vec<frame::StopSending>,
    pub(crate) max_stream_data: HashSet<StreamId>,
    pub(crate) crypto: VecDeque<frame::Crypto>,
    pub(crate) new_cids: Vec<IssuedCid>,
    pub(crate) retire_cids: Vec<u64>,
    pub(crate) handshake_done: bool,
}

impl Retransmits {
    pub fn is_empty(&self) -> bool {
        !self.max_data
            && !self.max_uni_stream_id
            && !self.max_bi_stream_id
            && self.reset_stream.is_empty()
            && self.stop_sending.is_empty()
            && self.max_stream_data.is_empty()
            && self.crypto.is_empty()
            && self.new_cids.is_empty()
            && self.retire_cids.is_empty()
            && !self.handshake_done
    }
}

impl Default for Retransmits {
    fn default() -> Self {
        Self {
            max_data: false,
            max_uni_stream_id: false,
            max_bi_stream_id: false,
            reset_stream: Vec::new(),
            stop_sending: Vec::new(),
            max_stream_data: HashSet::new(),
            crypto: VecDeque::new(),
            new_cids: Vec::new(),
            retire_cids: Vec::new(),
            handshake_done: false,
        }
    }
}

impl ::std::ops::BitOrAssign for Retransmits {
    fn bitor_assign(&mut self, rhs: Self) {
        // We reduce in-stream head-of-line blocking by queueing retransmits before other data for
        // STREAM and CRYPTO frames.
        self.max_data |= rhs.max_data;
        self.max_uni_stream_id |= rhs.max_uni_stream_id;
        self.max_bi_stream_id |= rhs.max_bi_stream_id;
        self.reset_stream.extend_from_slice(&rhs.reset_stream);
        self.stop_sending.extend_from_slice(&rhs.stop_sending);
        self.max_stream_data.extend(&rhs.max_stream_data);
        for crypto in rhs.crypto.into_iter().rev() {
            self.crypto.push_front(crypto);
        }
        self.new_cids.extend(&rhs.new_cids);
        self.retire_cids.extend(rhs.retire_cids);
        self.handshake_done |= rhs.handshake_done;
    }
}

impl ::std::iter::FromIterator<Retransmits> for Retransmits {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = Retransmits>,
    {
        let mut result = Retransmits::default();
        for packet in iter {
            result |= packet;
        }
        result
    }
}

/// RFC4303-style sliding window packet number deduplicator.
///
/// A contiguous bitfield, where each bit corresponds to a packet number and the rightmost bit is
/// always set. A set bit represents a packet that has been successfully authenticated. Bits left of
/// the window are assumed to be set.
///
/// ```text
/// ...xxxxxxxxx 1 0
///     ^        ^ ^
/// window highest next
/// ```
pub struct Dedup {
    window: Window,
    /// Lowest packet number higher than all yet authenticated.
    next: u64,
}

/// Inner bitfield type.
///
/// Because QUIC never reuses packet numbers, this only needs to be large enough to deal with
/// packets that are reordered but still delivered in a timely manner.
type Window = u128;

/// Number of packets tracked by `Dedup`.
const WINDOW_SIZE: u64 = 1 + mem::size_of::<Window>() as u64 * 8;

impl Dedup {
    /// Construct an empty window positioned at the start.
    pub fn new() -> Self {
        Self { window: 0, next: 0 }
    }

    /// Highest packet number authenticated.
    fn highest(&self) -> u64 {
        self.next - 1
    }

    /// Record a newly authenticated packet number.
    ///
    /// Returns whether the packet might be a duplicate.
    pub fn insert(&mut self, packet: u64) -> bool {
        if let Some(diff) = packet.checked_sub(self.next) {
            // Right of window
            self.window = (self.window << 1 | 1)
                .checked_shl(cmp::min(diff, u64::from(u32::max_value())) as u32)
                .unwrap_or(0);
            self.next = packet + 1;
            false
        } else if self.highest() - packet < WINDOW_SIZE {
            // Within window
            if let Some(bit) = (self.highest() - packet).checked_sub(1) {
                // < highest
                let mask = 1 << bit;
                let duplicate = self.window & mask != 0;
                self.window |= mask;
                duplicate
            } else {
                // == highest
                true
            }
        } else {
            // Left of window
            true
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn sanity() {
        let mut dedup = Dedup::new();
        assert!(!dedup.insert(0));
        assert_eq!(dedup.next, 1);
        assert_eq!(dedup.window, 0b1);
        assert!(dedup.insert(0));
        assert_eq!(dedup.next, 1);
        assert_eq!(dedup.window, 0b1);
        assert!(!dedup.insert(1));
        assert_eq!(dedup.next, 2);
        assert_eq!(dedup.window, 0b11);
        assert!(!dedup.insert(2));
        assert_eq!(dedup.next, 3);
        assert_eq!(dedup.window, 0b111);
        assert!(!dedup.insert(4));
        assert_eq!(dedup.next, 5);
        assert_eq!(dedup.window, 0b11110);
        assert!(!dedup.insert(7));
        assert_eq!(dedup.next, 8);
        assert_eq!(dedup.window, 0b1111_0100);
        assert!(dedup.insert(4));
        assert!(!dedup.insert(3));
        assert_eq!(dedup.next, 8);
        assert_eq!(dedup.window, 0b1111_1100);
        assert!(!dedup.insert(6));
        assert_eq!(dedup.next, 8);
        assert_eq!(dedup.window, 0b1111_1101);
        assert!(!dedup.insert(5));
        assert_eq!(dedup.next, 8);
        assert_eq!(dedup.window, 0b1111_1111);
    }

    #[test]
    fn happypath() {
        let mut dedup = Dedup::new();
        for i in 0..(2 * WINDOW_SIZE) {
            assert!(!dedup.insert(i));
            for j in 0..=i {
                assert!(dedup.insert(j));
            }
        }
    }

    #[test]
    fn jump() {
        let mut dedup = Dedup::new();
        dedup.insert(2 * WINDOW_SIZE);
        assert!(dedup.insert(WINDOW_SIZE));
        assert_eq!(dedup.next, 2 * WINDOW_SIZE + 1);
        assert_eq!(dedup.window, 0);
        assert!(!dedup.insert(WINDOW_SIZE + 1));
        assert_eq!(dedup.next, 2 * WINDOW_SIZE + 1);
        assert_eq!(dedup.window, 1 << (WINDOW_SIZE - 2));
    }
}
