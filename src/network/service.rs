// Smoldot
// Copyright (C) 2019-2022  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use crate::header;
use crate::libp2p::{
    connection, multiaddr, peer_id,
    peers::{self, QueueNotificationError},
    PeerId,
};
use crate::network::{kademlia, protocol};
use crate::util::{self, SipHasherBuild};

use alloc::{
    borrow::Cow,
    format,
    string::{String, ToString as _},
    vec::Vec,
};
use core::{
    fmt, iter, mem,
    num::NonZeroUsize,
    ops::{Add, Sub},
    time::Duration,
};
use futures::{
    lock::{Mutex, MutexGuard},
    prelude::*,
};
use rand::{seq::SliceRandom as _, Rng as _, SeedableRng as _};

pub use crate::libp2p::{
    collection::ReadWrite,
    peers::{ConnectionId, InboundError},
};

mod addresses;

/// Configuration for a [`ChainNetwork`].
pub struct Config<TNow> {
    /// Time at the moment of the initialization of the service.
    pub now: TNow,

    /// Capacity to initially reserve to the list of connections.
    pub connections_capacity: usize,

    /// Capacity to initially reserve to the list of peers.
    pub peers_capacity: usize,

    /// Seed for the randomness within the networking state machine.
    ///
    /// While this seed influences the general behaviour of the networking state machine, it
    /// notably isn't used when generating the ephemeral key used for the Diffie-Hellman
    /// handshake.
    /// This is a defensive measure against users passing a dummy seed instead of actual entropy.
    pub randomness_seed: [u8; 32],

    /// List of blockchain peer-to-peer networks to be connected to.
    ///
    /// > **Note**: As documented in [the module-level documentation](..), the [`ChainNetwork`]
    /// >           can connect to multiple blockchain networks at the same time.
    ///
    /// The order in which the chains are list is important. The index of each entry needs to be
    /// used later in order to refer to a specific chain.
    pub chains: Vec<ChainConfig>,

    /// Key used for the encryption layer.
    /// This is a Noise static key, according to the Noise specification.
    /// Signed using the actual libp2p key.
    pub noise_key: connection::NoiseKey,

    /// Amount of time after which a connection handshake is considered to have taken too long
    /// and must be aborted.
    pub handshake_timeout: Duration,

    /// Maximum number of addresses kept in memory per network identity.
    ///
    /// > **Note**: As the number of network identities kept in memory is capped, having a
    /// >           maximum number of addresses per peer ensures that the total number of
    /// >           addresses is capped as well.
    pub max_addresses_per_peer: NonZeroUsize,

    /// Number of events that can be buffered internally before connections are back-pressured.
    ///
    /// A good default value is 64.
    ///
    /// # Context
    ///
    /// The [`ChainNetwork`] maintains an internal buffer of the events returned by
    /// [`ChainNetwork::next_event`]. When [`ChainNetwork::read_write`] is called, an event might
    /// get pushed to this buffer. If this buffer is full, back-pressure will be applied to the
    /// connections in order to prevent new events from being pushed.
    ///
    /// This value is important if [`ChainNetwork::next_event`] is called at a slower than the
    /// calls to [`ChainNetwork::read_write`] generate events.
    pub pending_api_events_buffer_size: NonZeroUsize,
}

/// Configuration for a specific overlay network.
///
/// See [`Config::chains`].
pub struct ChainConfig {
    /// Identifier of the protocol, used on the wire to determine which chain messages refer to.
    ///
    /// > **Note**: This value is typically found in the specification of the chain (the
    /// >           "chain spec").
    pub protocol_id: String,

    /// If `Some`, the chain uses the GrandPa networking protocol.
    pub grandpa_protocol_config: Option<GrandpaState>,

    /// `true` if incoming block requests are allowed.
    pub allow_inbound_block_requests: bool,

    pub in_slots: u32,

    pub out_slots: u32,

    /// Hash of the best block according to the local node.
    pub best_hash: [u8; 32],
    /// Height of the best block according to the local node.
    pub best_number: u64,
    /// Hash of the genesis block (i.e. block number 0) according to the local node.
    pub genesis_hash: [u8; 32],
    pub role: protocol::Role,
}

#[derive(Debug, Copy, Clone)]
// TODO: link to some doc about how GrandPa works: what is a round, what is the set id, etc.
pub struct GrandpaState {
    pub round_number: u64,
    /// Set of authorities that will be used by the node to try finalize the children of the block
    /// of [`GrandpaState::commit_finalized_height`].
    pub set_id: u64,
    /// Height of the highest block considered final by the node.
    pub commit_finalized_height: u32,
}

/// Identifier of a pending connection requested by the network through a [`StartConnect`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PendingId(usize);

/// Data structure containing the list of all connections, pending or not, and their latest known
/// state. See also [the module-level documentation](..).
pub struct ChainNetwork<TNow> {
    /// Underlying data structure.
    inner: peers::Peers<multiaddr::Multiaddr, TNow>,

    /// See [`Config::handshake_timeout`].
    handshake_timeout: Duration,

    /// See [`Config::max_addresses_per_peer`].
    max_addresses_per_peer: NonZeroUsize,

    /// Extra fields protected by a `Mutex` and that relate to the logic in
    /// [`ChainNetwork::next_event`]. Must only be locked within that method and is kept locked
    /// throughout that method.
    next_event_guarded: Mutex<NextEventGuarded>,

    /// Extra fields protected by a `Mutex` and that are briefly accessed.
    ephemeral_guarded: Mutex<EphemeralGuarded<TNow>>,

    /// Number of chains. Equal to the length of [`EphemeralGuarded::chains`].
    num_chains: usize,

    /// Generator for randomness.
    randomness: Mutex<rand_chacha::ChaCha20Rng>,

    /// Event notified when [`ChainNetwork::next_start_connect`] should be called again by the
    /// user.
    start_connect_needed: event_listener::Event,
}

/// See [`ChainNetwork::next_event_guarded`].
struct NextEventGuarded {
    /// In the [`ChainNetwork::next_event`] function, an event is grabbed from the underlying
    /// [`peers::Peers`]. This event might lead to some asynchronous post-processing being
    /// needed. Because the user can interrupt the future returned by [`ChainNetwork::next_event`]
    /// at any point in time, this post-processing cannot be immediately performed, as the user
    /// could interrupt the future and lose the event. Instead, the event is temporarily stored
    /// in this field while this post-processing happens and is only cleared afterwards.
    to_process_pre_event: Option<peers::Event<multiaddr::Multiaddr>>,

    /// Tuples of `(peer_id, chain_index)` that have been reported as open to the API user.
    ///
    /// This is a subset of the block announce notification protocol substreams that are open.
    /// Some substreams might have been opened and have been left out of this map if their
    /// handshake was invalid, or had a different genesis hash, or similar problem.
    open_chains: hashbrown::HashSet<(PeerId, usize), SipHasherBuild>,
}

/// See [`ChainNetwork::ephemeral_guarded`].
struct EphemeralGuarded<TNow> {
    /// For each peer, the number of pending attempts.
    num_pending_per_peer: hashbrown::HashMap<PeerId, NonZeroUsize, SipHasherBuild>,

    /// Keys of this slab are [`PendingId`]s. Values are the parameters associated to that
    /// [`PendingId`].
    /// The entries here correspond to the entries in
    /// [`EphemeralGuarded::num_pending_per_peer`].
    pending_ids: slab::Slab<(PeerId, multiaddr::Multiaddr, TNow)>,

    /// List of all open connections.
    connections: hashbrown::HashSet<PeerId, SipHasherBuild>,

    /// For each item in [`Config::chains`], the corresponding chain state.
    ///
    /// The `Vec` always has the same length as [`Config::chains`].
    chains: Vec<EphemeralGuardedChain<TNow>>,
}

struct EphemeralGuardedChain<TNow> {
    /// See [`ChainConfig`].
    chain_config: ChainConfig,

    // TODO: merge in_peers and out_peers into one hashmap<_, SlotTy>
    /// List of peers with an inbound slot attributed to them. Only includes peers the local node
    /// is connected to and who have opened a block announces substream with the local node.
    in_peers: hashbrown::HashSet<PeerId, SipHasherBuild>,

    /// List of peers with an outbound slot attributed to them. Can include peers not connected to
    /// the local node yet. The peers in this list are always marked as desired in the underlying
    /// state machine.
    out_peers: hashbrown::HashSet<PeerId, SipHasherBuild>,

    /// Kademlia k-buckets of this chain.
    ///
    /// Used in order to hold the list of peers that are known to be part of this chain.
    ///
    /// A peer is marked as "connected" in the k-buckets when a block announces substream is open,
    /// and disconnected when it is closed.
    ///
    /// For each peer, a list of addresses is hold. This list must never become empty.
    kbuckets: kademlia::kbuckets::KBuckets<PeerId, addresses::Addresses, TNow, 20>,
}

// Update this when a new request response protocol is added.
const REQUEST_RESPONSE_PROTOCOLS_PER_CHAIN: usize = 5;
// Update this when a new notifications protocol is added.
const NOTIFICATIONS_PROTOCOLS_PER_CHAIN: usize = 3;

impl<TNow> ChainNetwork<TNow>
where
    TNow: Clone + Add<Duration, Output = TNow> + Sub<TNow, Output = Duration> + Ord,
{
    /// Initializes a new [`ChainNetwork`].
    pub fn new(config: Config<TNow>) -> Self {
        // The order of protocols here is important, as it defines the values of `protocol_index`
        // to pass to libp2p or that libp2p produces.
        let notification_protocols = config
            .chains
            .iter()
            .flat_map(|chain| {
                iter::once(peers::NotificationProtocolConfig {
                    protocol_name: format!("/{}/block-announces/1", chain.protocol_id),
                    fallback_protocol_names: Vec::new(),
                    max_handshake_size: 1024 * 1024, // TODO: arbitrary
                    max_notification_size: 1024 * 1024,
                })
                .chain(iter::once(peers::NotificationProtocolConfig {
                    protocol_name: format!("/{}/transactions/1", chain.protocol_id),
                    fallback_protocol_names: Vec::new(),
                    max_handshake_size: 4,
                    max_notification_size: 16 * 1024 * 1024,
                }))
                .chain({
                    // The `has_grandpa_protocol` flag controls whether the chain uses GrandPa.
                    // Note, however, that GrandPa is technically left enabled (but unused) on all
                    // chains, in order to make the rest of the code of this module more
                    // comprehensible.
                    iter::once(peers::NotificationProtocolConfig {
                        protocol_name: "/paritytech/grandpa/1".to_string(),
                        fallback_protocol_names: Vec::new(),
                        max_handshake_size: 4,
                        max_notification_size: 1024 * 1024,
                    })
                })
            })
            .collect();

        // The order of protocols here is important, as it defines the values of `protocol_index`
        // to pass to libp2p or that libp2p produces.
        let request_response_protocols = iter::once(peers::ConfigRequestResponse {
            name: "/ipfs/id/1.0.0".into(),
            inbound_config: peers::ConfigRequestResponseIn::Empty,
            max_response_size: 4096,
            inbound_allowed: true,
        })
        .chain(config.chains.iter().flat_map(|chain| {
            // TODO: limits are arbitrary
            iter::once(peers::ConfigRequestResponse {
                name: format!("/{}/sync/2", chain.protocol_id),
                inbound_config: peers::ConfigRequestResponseIn::Payload { max_size: 1024 },
                max_response_size: 16 * 1024 * 1024,
                inbound_allowed: chain.allow_inbound_block_requests,
            })
            .chain(iter::once(peers::ConfigRequestResponse {
                name: format!("/{}/light/2", chain.protocol_id),
                inbound_config: peers::ConfigRequestResponseIn::Payload {
                    max_size: 1024 * 512,
                },
                max_response_size: 10 * 1024 * 1024,
                // TODO: make this configurable
                inbound_allowed: false,
            }))
            .chain(iter::once(peers::ConfigRequestResponse {
                name: format!("/{}/kad", chain.protocol_id),
                inbound_config: peers::ConfigRequestResponseIn::Payload { max_size: 1024 },
                max_response_size: 1024 * 1024,
                // TODO: `false` here means we don't insert ourselves in the DHT, which is the polite thing to do for as long as Kad isn't implemented
                inbound_allowed: false,
            }))
            .chain(iter::once(peers::ConfigRequestResponse {
                name: format!("/{}/sync/warp", chain.protocol_id),
                inbound_config: peers::ConfigRequestResponseIn::Payload { max_size: 32 },
                max_response_size: 16 * 1024 * 1024,
                // We don't support inbound warp sync requests (yet).
                inbound_allowed: false,
            }))
            .chain(iter::once(peers::ConfigRequestResponse {
                name: format!("/{}/state/2", chain.protocol_id),
                inbound_config: peers::ConfigRequestResponseIn::Payload { max_size: 1024 },
                max_response_size: 16 * 1024 * 1024,
                // We don't support inbound state requests (yet).
                inbound_allowed: false,
            }))
        }))
        .collect();

        let mut randomness = rand_chacha::ChaCha20Rng::from_seed(config.randomness_seed);

        let local_peer_id = PeerId::from_public_key(&peer_id::PublicKey::Ed25519(
            *config.noise_key.libp2p_public_ed25519_key(),
        ));

        let chains = config
            .chains
            .into_iter()
            .map(|chain| {
                EphemeralGuardedChain {
                    in_peers: hashbrown::HashSet::with_capacity_and_hasher(
                        usize::try_from(chain.in_slots).unwrap_or(0),
                        SipHasherBuild::new(randomness.gen()),
                    ),
                    out_peers: hashbrown::HashSet::with_capacity_and_hasher(
                        usize::try_from(chain.out_slots).unwrap_or(0),
                        SipHasherBuild::new(randomness.gen()),
                    ),
                    chain_config: chain,
                    kbuckets: kademlia::kbuckets::KBuckets::new(
                        local_peer_id.clone(),
                        Duration::from_secs(20), // TODO: hardcoded
                    ),
                }
            })
            .collect::<Vec<_>>();

        ChainNetwork {
            inner: peers::Peers::new(peers::Config {
                connections_capacity: config.connections_capacity,
                peers_capacity: config.peers_capacity,
                request_response_protocols,
                noise_key: config.noise_key,
                randomness_seed: randomness.sample(rand::distributions::Standard),
                pending_api_events_buffer_size: config.pending_api_events_buffer_size,
                notification_protocols,
                ping_protocol: "/ipfs/ping/1.0.0".into(),
                handshake_timeout: config.handshake_timeout,
            }),
            num_chains: chains.len(),
            next_event_guarded: Mutex::new(NextEventGuarded {
                to_process_pre_event: None,
                open_chains: hashbrown::HashSet::with_capacity_and_hasher(
                    config.peers_capacity * chains.len(),
                    SipHasherBuild::new(randomness.gen()),
                ),
            }),
            ephemeral_guarded: Mutex::new(EphemeralGuarded {
                num_pending_per_peer: hashbrown::HashMap::with_capacity_and_hasher(
                    config.peers_capacity,
                    SipHasherBuild::new(randomness.gen()),
                ),
                pending_ids: slab::Slab::with_capacity(config.peers_capacity),
                connections: hashbrown::HashSet::with_capacity_and_hasher(
                    config.peers_capacity,
                    SipHasherBuild::new(randomness.gen()),
                ),
                chains,
            }),
            handshake_timeout: config.handshake_timeout,
            max_addresses_per_peer: config.max_addresses_per_peer,
            randomness: Mutex::new(randomness),
            start_connect_needed: event_listener::Event::new(),
        }
    }

    fn protocol_index(&self, chain_index: usize, protocol: usize) -> usize {
        1 + chain_index * REQUEST_RESPONSE_PROTOCOLS_PER_CHAIN + protocol
    }

    /// Returns the number of established TCP connections, both incoming and outgoing.
    // TODO: note about race
    pub async fn num_established_connections(&self) -> usize {
        // TODO: better impl
        self.peers_list().await.count()
    }

    /// Returns the number of peers we have a substream with.
    pub async fn num_peers(&self, chain_index: usize) -> usize {
        self.inner
            .num_outgoing_substreams(self.protocol_index(chain_index, 0))
            .await
    }

    /// Returns the number of chains. Always equal to the length of [`Config::chains`].
    pub fn num_chains(&self) -> usize {
        self.num_chains
    }

    /// Returns the Noise key originalled passed as [`Config::noise_key`].
    pub fn noise_key(&self) -> &connection::NoiseKey {
        self.inner.noise_key()
    }

    /// Adds an incoming connection to the state machine.
    ///
    /// This connection hasn't finished handshaking and the [`PeerId`] of the remote isn't known
    /// yet.
    ///
    /// Must be passed the moment (as a `TNow`) when the connection as been established, in order
    /// to determine when the handshake timeout expires.
    ///
    /// After this function has returned, you must process the connection with
    /// [`ChainNetwork::read_write`].
    ///
    /// The `remote_addr` is the address used to reach back the remote. In the case of TCP, it
    /// contains the TCP dialing port of the remote. The remote can ask, through the `identify`
    /// libp2p protocol, its own address, in which case we send it.
    pub async fn add_incoming_connection(
        &self,
        when_connected: TNow,
        remote_addr: multiaddr::Multiaddr,
    ) -> ConnectionId {
        self.inner
            .add_incoming_connection(when_connected, remote_addr)
            .await
    }

    /// Modifies the best block of the local node. See [`ChainConfig::best_hash`] and
    /// [`ChainConfig::best_number`].
    ///
    /// # Panic
    ///
    /// Panics if `chain_index` is out of range.
    ///
    pub async fn set_local_best_block(
        &self,
        chain_index: usize,
        best_hash: [u8; 32],
        best_number: u64,
    ) {
        let mut guarded = self.ephemeral_guarded.lock().await;
        let mut config = &mut guarded.chains[chain_index].chain_config;
        config.best_hash = best_hash;
        config.best_number = best_number;
    }

    /// Update the state of the local node with regards to GrandPa rounds.
    ///
    /// Calling this method does two things:
    ///
    /// - Send on all the active GrandPa substreams a "neighbor packet" indicating the state of
    ///   the local node.
    /// - Update the neighbor packet that is automatically sent to peers when a GrandPa substream
    ///   gets opened.
    ///
    /// In other words, calling this function atomically informs all the present and future peers
    /// of the state of the local node regarding the GrandPa protocol.
    ///
    /// > **Note**: The information passed as parameter isn't validated in any way by this method.
    ///
    /// # Panic
    ///
    /// Panics if `chain_index` is out of range, or if the chain has GrandPa disabled.
    ///
    pub async fn set_local_grandpa_state(&self, chain_index: usize, grandpa_state: GrandpaState) {
        let mut guarded = self.ephemeral_guarded.lock().await;

        // Bytes of the neighbor packet to send out.
        let packet = protocol::GrandpaNotificationRef::Neighbor(protocol::NeighborPacket {
            round_number: grandpa_state.round_number,
            set_id: grandpa_state.set_id,
            commit_finalized_height: grandpa_state.commit_finalized_height,
        })
        .scale_encoding()
        .fold(Vec::new(), |mut a, b| {
            a.extend_from_slice(b.as_ref());
            a
        });

        // Now sending out.
        let _ = self
            .inner
            .broadcast_notification(chain_index * NOTIFICATIONS_PROTOCOLS_PER_CHAIN + 2, packet)
            .await;

        // Update the locally-stored state, but only after the notification has been broadcasted.
        // This way, if the user cancels the future while `broadcast_notification` is executing,
        // the whole operation is cancelled.
        *guarded.chains[chain_index]
            .chain_config
            .grandpa_protocol_config
            .as_mut()
            .unwrap() = grandpa_state;
    }

    /// Sends a blocks request to the given peer.
    // TODO: more docs
    pub async fn blocks_request(
        &self,
        now: TNow,
        target: &peer_id::PeerId,
        chain_index: usize,
        config: protocol::BlocksRequestConfig,
        timeout: Duration,
    ) -> Result<Vec<protocol::BlockData>, BlocksRequestError> {
        if !config.fields.header {
            return Err(BlocksRequestError::NotVerifiable);
        }

        let request_start = config.start.clone();
        let requested_fields = config.fields.clone();

        let mut result = self
            .blocks_request_unchecked(now, target, chain_index, config, timeout)
            .await?;

        if result.is_empty() {
            return Err(BlocksRequestError::EmptyResponse);
        }

        // Verify validity of all the blocks.
        for (block_index, block) in result.iter_mut().enumerate() {
            if block.header.is_none() {
                return Err(BlocksRequestError::Entry {
                    index: block_index,
                    error: BlocksRequestResponseEntryError::MissingField,
                });
            }

            if block
                .header
                .as_ref()
                .map_or(false, |h| header::decode(h).is_err())
            {
                return Err(BlocksRequestError::Entry {
                    index: block_index,
                    error: BlocksRequestResponseEntryError::InvalidHeader,
                });
            }

            match (block.body.is_some(), requested_fields.body) {
                (false, true) => {
                    return Err(BlocksRequestError::Entry {
                        index: block_index,
                        error: BlocksRequestResponseEntryError::MissingField,
                    });
                }
                (true, false) => {
                    block.body = None;
                }
                _ => {}
            }

            // Note: the presence of a justification isn't checked and can't be checked, as not
            // all blocks have a justification in the first place.

            if block.header.as_ref().map_or(false, |h| {
                header::hash_from_scale_encoded_header(&h) != block.hash
            }) {
                return Err(BlocksRequestError::Entry {
                    index: block_index,
                    error: BlocksRequestResponseEntryError::InvalidHash,
                });
            }

            if let (Some(header), Some(body)) = (&block.header, &block.body) {
                let decoded_header = header::decode(header).unwrap();
                let expected = header::extrinsics_root(&body[..]);
                if expected != *decoded_header.extrinsics_root {
                    return Err(BlocksRequestError::Entry {
                        index: block_index,
                        error: BlocksRequestResponseEntryError::InvalidExtrinsicsRoot {
                            calculated: expected,
                            in_header: *decoded_header.extrinsics_root,
                        },
                    });
                }
            }
        }

        match request_start {
            protocol::BlocksRequestConfigStart::Hash(hash) if result[0].hash != hash => {
                return Err(BlocksRequestError::InvalidStart);
            }
            protocol::BlocksRequestConfigStart::Number(n)
                if header::decode(result[0].header.as_ref().unwrap())
                    .unwrap()
                    .number
                    != n =>
            {
                return Err(BlocksRequestError::InvalidStart)
            }
            _ => {}
        }

        Ok(result)
    }

    /// Sends a blocks request to the given peer.
    // TODO: more docs
    pub async fn blocks_request_unchecked(
        &self,
        now: TNow,
        target: &peer_id::PeerId,
        chain_index: usize,
        config: protocol::BlocksRequestConfig,
        timeout: Duration,
    ) -> Result<Vec<protocol::BlockData>, BlocksRequestError> {
        let request_data = protocol::build_block_request(config).fold(Vec::new(), |mut a, b| {
            a.extend_from_slice(b.as_ref());
            a
        });

        let response = self
            .inner
            .request(
                target,
                self.protocol_index(chain_index, 0),
                request_data,
                now + timeout,
            )
            .map_err(BlocksRequestError::Request)
            .await?;

        protocol::decode_block_response(&response).map_err(BlocksRequestError::Decode)
    }

    pub async fn grandpa_warp_sync_request(
        &self,
        now: TNow,
        target: &peer_id::PeerId,
        chain_index: usize,
        begin_hash: [u8; 32],
        timeout: Duration,
    ) -> Result<protocol::GrandpaWarpSyncResponse, GrandpaWarpSyncRequestError> {
        let request_data = begin_hash.to_vec();

        let response = self
            .inner
            .request(
                target,
                self.protocol_index(chain_index, 3),
                request_data,
                now + timeout,
            )
            .map_err(GrandpaWarpSyncRequestError::Request)
            .await?;

        protocol::decode_grandpa_warp_sync_response(&response)
            .map_err(GrandpaWarpSyncRequestError::Decode)
    }

    /// Sends a state request to a peer.
    ///
    /// A state request makes it possible to download the storage of the chain at a given block.
    /// The response is not unverified by this function. In other words, the peer is free to send
    /// back erroneous data. It is the responsibility of the API user to verify the storage by
    /// calculating the state trie root hash and comparing it with the value stored in the
    /// block's header.
    ///
    /// Because response have a size limit, it is unlikely that a single request will return the
    /// entire storage of the chain at once. Instead, call this function multiple times, each call
    /// passing a `start_key` that follows the last key of the previous response.
    // TODO: does an empty response mean that `start_key` is the last key of the storage? unclear
    pub async fn state_request_unchecked(
        &self,
        now: TNow,
        target: &peer_id::PeerId,
        chain_index: usize,
        block_hash: [u8; 32],
        start_key: &[u8],
        timeout: Duration,
    ) -> Result<Vec<protocol::StateResponseEntry>, StateRequestError> {
        let request_data = protocol::build_state_request(protocol::StateRequestConfig {
            block_hash,
            start_key: start_key.to_vec(),
        })
        .fold(Vec::new(), |mut a, b| {
            a.extend_from_slice(b.as_ref());
            a
        });

        let response = self
            .inner
            .request(
                target,
                self.protocol_index(chain_index, 4),
                request_data,
                now + timeout,
            )
            .map_err(StateRequestError::Request)
            .await?;

        protocol::decode_state_response(&response).map_err(StateRequestError::Decode)
    }

    /// Sends a storage request to the given peer.
    // TODO: more docs
    pub async fn storage_proof_request(
        &self,
        now: TNow,
        target: &peer_id::PeerId,
        chain_index: usize,
        config: protocol::StorageProofRequestConfig<impl Iterator<Item = impl AsRef<[u8]>>>,
        timeout: Duration,
    ) -> Result<Vec<Vec<u8>>, StorageProofRequestError> {
        let request_data =
            protocol::build_storage_proof_request(config).fold(Vec::new(), |mut a, b| {
                a.extend_from_slice(b.as_ref());
                a
            });

        let response = self
            .inner
            .request(
                target,
                self.protocol_index(chain_index, 1),
                request_data,
                now + timeout,
            )
            .map_err(StorageProofRequestError::Request)
            .await?;

        protocol::decode_storage_proof_response(&response).map_err(StorageProofRequestError::Decode)
    }

    /// Sends a call proof request to the given peer.
    ///
    /// This request is similar to [`ChainNetwork::storage_proof_request`]. Instead of requesting
    /// specific keys, we request the list of all the keys that are accessed for a specific
    /// runtime call.
    ///
    /// There exists no guarantee that the proof is complete (i.e. that it contains all the
    /// necessary entries), as it is impossible to know this from just the proof itself. As such,
    /// this method is just an optimization. When performing the actual call, regular storage proof
    /// requests should be performed if the key is not present in the call proof response.
    pub async fn call_proof_request(
        &self,
        now: TNow,
        target: &peer_id::PeerId,
        chain_index: usize,
        config: protocol::CallProofRequestConfig<'_, impl Iterator<Item = impl AsRef<[u8]>>>,
        timeout: Duration,
    ) -> Result<Vec<Vec<u8>>, CallProofRequestError> {
        let request_data =
            protocol::build_call_proof_request(config).fold(Vec::new(), |mut a, b| {
                a.extend_from_slice(b.as_ref());
                a
            });

        let response = self
            .inner
            .request(
                target,
                self.protocol_index(chain_index, 1),
                request_data,
                now + timeout,
            )
            .map_err(CallProofRequestError::Request)
            .await?;

        protocol::decode_call_proof_response(&response).map_err(CallProofRequestError::Decode)
    }

    ///
    ///
    /// Must be passed the SCALE-encoded transaction.
    // TODO: -> broadcast_transaction
    pub async fn announce_transaction(
        &self,
        target: &peer_id::PeerId,
        chain_index: usize,
        extrinsic: &[u8],
    ) -> Result<(), QueueNotificationError> {
        let mut val = Vec::with_capacity(1 + extrinsic.len());
        val.extend_from_slice(util::encode_scale_compact_usize(1).as_ref());
        val.extend_from_slice(extrinsic);
        self.inner
            .queue_notification(
                target,
                chain_index * NOTIFICATIONS_PROTOCOLS_PER_CHAIN + 1,
                val,
            )
            .await
    }

    /// Inserts the given list of nodes into the list of known nodes held within the state machine.
    ///
    /// The service might, but without guarantee, try to connect to these nodes in the future.
    pub async fn discover(
        &self,
        now: &TNow,
        chain_index: usize,
        list: impl IntoIterator<
            Item = (
                peer_id::PeerId,
                impl IntoIterator<Item = multiaddr::Multiaddr>,
            ),
        >,
    ) {
        let mut lock = self.ephemeral_guarded.lock().await;
        let lock = &mut *lock; // Avoids borrow checker issues.

        let kbuckets = &mut lock.chains[chain_index].kbuckets;

        for (peer_id, discovered_addrs) in list {
            let mut discovered_addrs = discovered_addrs.into_iter().peekable();

            // Check whether there is any address in the iterator at all before inserting the
            // node in the buckets.
            if discovered_addrs.peek().is_none() {
                continue;
            }

            // TODO: also insert addresses in kbuckets of other chains? a bit unclear
            if let Ok(mut kbuckets_addrs) = kbuckets.entry(&peer_id).or_insert(
                addresses::Addresses::with_capacity(self.max_addresses_per_peer.get()),
                now,
                kademlia::kbuckets::PeerState::Disconnected,
            ) {
                for to_insert in discovered_addrs {
                    if kbuckets_addrs.get_mut().len() >= self.max_addresses_per_peer.get() {
                        continue;
                    }

                    kbuckets_addrs.get_mut().insert_discovered(to_insert);
                }

                // List of addresses must never be empty.
                debug_assert!(!kbuckets_addrs.get_mut().is_empty());
            }
        }
    }

    /// After calling [`ChainNetwork::next_start_connect`], notifies the [`ChainNetwork`] of the
    /// success of the dialing attempt.
    ///
    /// See also [`ChainNetwork::pending_outcome_err`].
    ///
    /// After this function has returned, you must process the connection with
    /// [`ChainNetwork::read_write`].
    ///
    /// # Panic
    ///
    /// Panics if the [`PendingId`] is invalid.
    ///
    pub async fn pending_outcome_ok(&self, id: PendingId) -> ConnectionId {
        let mut lock = self.ephemeral_guarded.lock().await;
        let lock = &mut *lock; // Prevents borrow checker issues.

        // Don't remove the value in `pending_ids` yet, so that the state remains consistent if
        // the user cancels the future returned by `add_outgoing_connection`.
        let (expected_peer_id, multiaddr, when_connected) = lock.pending_ids.get(id.0).unwrap();

        let connection_id = self
            .inner
            .add_outgoing_connection(when_connected.clone(), expected_peer_id, multiaddr.clone())
            .await;

        // Update `lock.peers`.
        {
            let value = lock.num_pending_per_peer.get_mut(expected_peer_id).unwrap();
            if let Some(new_value) = NonZeroUsize::new(value.get() - 1) {
                *value = new_value;
            } else {
                lock.num_pending_per_peer.remove(expected_peer_id).unwrap();
            }
        }

        // Update the list of addresses.
        // TODO: O(n)
        for chain in &mut lock.chains {
            if let Some(addrs) = chain.kbuckets.get_mut(expected_peer_id) {
                addrs.set_connected(multiaddr);
            }
        }

        lock.pending_ids.remove(id.0);

        connection_id
    }

    /// After calling [`ChainNetwork::next_start_connect`], notifies the [`ChainNetwork`] of the
    /// failure of the dialing attempt.
    ///
    /// See also [`ChainNetwork::pending_outcome_ok`].
    ///
    /// `is_unreachable` should be `true` if the address is invalid or unreachable and should
    /// thus never be attempted again unless it is re-discovered. It should be `false` if the
    /// address might only be temporarily unreachable, such as because of a timeout. If `false`
    /// is passed, the address might be attempted again in the future.
    ///
    /// # Panic
    ///
    /// Panics if the [`PendingId`] is invalid.
    ///
    pub async fn pending_outcome_err(&self, id: PendingId, is_unreachable: bool) {
        let mut lock = self.ephemeral_guarded.lock().await;
        let (expected_peer_id, multiaddr, _) = lock.pending_ids.get(id.0).unwrap();
        let multiaddr = multiaddr.clone(); // Solves borrowck issues.

        let has_any_attempt_left = lock
            .num_pending_per_peer
            .get(expected_peer_id)
            .unwrap()
            .get()
            != 1;

        // If the peer is completely unreachable, unassign all of its slots.
        if !has_any_attempt_left && !lock.connections.contains(expected_peer_id) {
            let expected_peer_id = expected_peer_id.clone(); // Necessary for borrowck reasons.

            for chain_index in 0..lock.chains.len() {
                // TODO: report as event or something; this is complicated because of futures cancellation issues, and because of concerns shown in `assign_slots`
                self.unassign_slot(&mut *lock, chain_index, &expected_peer_id)
                    .await;
            }
        }

        // Now update `lock`.
        // For future-cancellation-safety reasons, this is done after all the asynchronous
        // operations.

        let (expected_peer_id, _, _) = lock.pending_ids.remove(id.0);

        // Updates the addresses book.
        // TODO: O(n)
        for chain in &mut lock.chains {
            if let Some(addrs) = chain.kbuckets.get_mut(&expected_peer_id) {
                if is_unreachable {
                    // Do not remove last remaining address, in order to prevent the addresses
                    // list from ever becoming empty.
                    debug_assert!(!addrs.is_empty());
                    if addrs.len() <= 1 {
                        continue;
                    }

                    addrs.remove(&multiaddr);
                } else {
                    addrs.set_disconnected(&multiaddr);

                    // Shuffle the known addresses, otherwise the same address might get picked
                    // again.
                    addrs.shuffle();
                }
            }
        }

        {
            let value = lock
                .num_pending_per_peer
                .get_mut(&expected_peer_id)
                .unwrap();
            if let Some(new_value) = NonZeroUsize::new(value.get() - 1) {
                *value = new_value;
            } else {
                lock.num_pending_per_peer.remove(&expected_peer_id).unwrap();
            }
        };

        self.start_connect_needed.notify_additional(1);
    }

    /// Returns the next event produced by the service.
    ///
    /// This function should be called at a high enough rate that [`ChainNetwork::read_write`] can
    /// continue pushing events to the internal buffer of events. Failure to call this function
    /// often enough will lead to connections being back-pressured.
    /// See also [`Config::pending_api_events_buffer_size`].
    ///
    /// It is technically possible to call this function multiple times simultaneously, in which
    /// case the events will be distributed amongst the multiple calls in an unspecified way.
    /// Keep in mind that some [`Event`]s have logic attached to the order in which they are
    /// produced, and calling this function multiple times is therefore discouraged.
    // TODO: this `now` parameter, it's a hack
    pub async fn next_event(&'_ self, now: TNow) -> Event<'_, TNow> {
        let mut guarded = self.next_event_guarded.lock().await;
        let guarded = &mut *guarded;

        loop {
            // It might be that a previous call to `next_event` has been interrupted. If that is
            // the case, an event will have been left in `to_process_pre_event`. Only pull a new
            // event if there isn't any not-fully-processed-yet event.
            let inner_event = match &mut guarded.to_process_pre_event {
                Some(ev) => ev,
                ev @ None => {
                    let new_event = self.inner.next_event().await;
                    ev.insert(new_event)
                }
            };

            // `inner_event` is a mutable reference to `guarded.to_process_pre_event`. All the
            // branches below must clear `to_process_pre_event` after all potentially-cancellable
            // asynchronous operations are finished.
            match inner_event {
                peers::Event::Connected {
                    peer_id,
                    num_peer_connections,
                    ..
                } if num_peer_connections.get() == 1 => {
                    let mut ephemeral_guarded = self.ephemeral_guarded.lock().await;

                    let _was_inserted = ephemeral_guarded.connections.insert(peer_id.clone());
                    debug_assert!(_was_inserted);

                    return match guarded.to_process_pre_event.take().unwrap() {
                        peers::Event::Connected { peer_id, .. } => Event::Connected(peer_id),
                        _ => unreachable!(),
                    };
                }
                peers::Event::Connected { .. } => {
                    guarded.to_process_pre_event = None;
                }

                peers::Event::Disconnected {
                    peer_id,
                    num_peer_connections,
                    peer_is_desired,
                    user_data: address,
                } if *num_peer_connections == 0 => {
                    if *peer_is_desired {
                        self.start_connect_needed.notify_additional(1);
                    }

                    // TODO: O(n)
                    let chain_indices = guarded
                        .open_chains
                        .iter()
                        .filter(|(pid, _)| pid == peer_id)
                        .map(|(_, c)| *c)
                        .collect::<Vec<_>>();

                    let mut ephemeral_guarded = self.ephemeral_guarded.lock().await;

                    // Un-assign all the slots of that peer.
                    // Because this is an asynchronous operation, this is done ahead of time and
                    // before any modification to `guarded` or `ephemeral_guarded`.
                    for idx in &chain_indices {
                        self.unassign_slot(&mut *ephemeral_guarded, *idx, peer_id)
                            .await;
                    }

                    let _was_in = ephemeral_guarded.connections.remove(peer_id);
                    debug_assert!(_was_in);

                    // Update the k-buckets.
                    // TODO: `Disconnected` is only generated for connections that weren't handshaking, so this is not correct
                    for chain in &mut ephemeral_guarded.chains {
                        if let Some(mut entry) = chain.kbuckets.entry(peer_id).into_occupied() {
                            entry.set_state(&now, kademlia::kbuckets::PeerState::Disconnected);
                            entry.get_mut().set_disconnected(address);
                        }
                    }

                    for idx in &chain_indices {
                        guarded.open_chains.remove(&(peer_id.clone(), *idx)); // TODO: cloning :-/
                    }

                    return match guarded.to_process_pre_event.take().unwrap() {
                        peers::Event::Disconnected { peer_id, .. } => Event::Disconnected {
                            peer_id,
                            chain_indices,
                        },
                        _ => unreachable!(),
                    };
                }
                peers::Event::Disconnected {
                    peer_id,
                    user_data: address,
                    ..
                } => {
                    let mut ephemeral_guarded = self.ephemeral_guarded.lock().await;

                    // Update the k-buckets.
                    // TODO: `Disconnected` is only generated for connections that weren't handshaking, so this is not correct
                    for chain in &mut ephemeral_guarded.chains {
                        if let Some(mut entry) = chain.kbuckets.entry(peer_id).into_occupied() {
                            entry.set_state(&now, kademlia::kbuckets::PeerState::Disconnected);
                            entry.get_mut().set_disconnected(address);
                        }
                    }

                    guarded.to_process_pre_event = None;
                }

                // Insubstantial error for diagnostic purposes.
                peers::Event::InboundError { .. } => {
                    match guarded.to_process_pre_event.take().unwrap() {
                        peers::Event::InboundError { peer_id, error, .. } => {
                            return Event::ProtocolError {
                                peer_id,
                                error: ProtocolError::InboundError(error),
                            };
                        }
                        _ => unreachable!(),
                    }
                }

                // Incoming requests of the "identify" protocol.
                peers::Event::RequestIn {
                    protocol_index: 0,
                    request_payload,
                    request_id,
                    ..
                } => {
                    if request_payload.is_empty() {
                        return match guarded.to_process_pre_event.take().unwrap() {
                            peers::Event::RequestIn {
                                peer_id,
                                request_id,
                                connection_user_data: observed_addr,
                                ..
                            } => Event::IdentifyRequestIn {
                                peer_id,
                                request: IdentifyRequestIn {
                                    service: self,
                                    request_id,
                                    observed_addr,
                                },
                            },
                            _ => unreachable!(),
                        };
                    }
                    let _ = self.inner.respond(*request_id, Err(())).await;
                    return match guarded.to_process_pre_event.take().unwrap() {
                        peers::Event::RequestIn { peer_id, .. } => Event::ProtocolError {
                            peer_id,
                            error: ProtocolError::BadIdentifyRequest,
                        },
                        _ => unreachable!(),
                    };
                }
                // Incoming requests of the "sync" protocol.
                peers::Event::RequestIn {
                    request_id,
                    protocol_index,
                    request_payload,
                    ..
                } if ((*protocol_index - 1) % REQUEST_RESPONSE_PROTOCOLS_PER_CHAIN) == 0 => {
                    let chain_index = (*protocol_index - 1) / REQUEST_RESPONSE_PROTOCOLS_PER_CHAIN;

                    match protocol::decode_block_request(request_payload) {
                        Ok(config) => {
                            return match guarded.to_process_pre_event.take().unwrap() {
                                peers::Event::RequestIn {
                                    peer_id,
                                    request_id,
                                    ..
                                } => Event::BlocksRequestIn {
                                    peer_id,
                                    chain_index,
                                    config,
                                    request: BlocksRequestIn {
                                        service: self,
                                        request_id,
                                    },
                                },
                                _ => unreachable!(),
                            };
                        }
                        Err(error) => {
                            let _ = self.inner.respond(*request_id, Err(())).await;
                            return match guarded.to_process_pre_event.take().unwrap() {
                                peers::Event::RequestIn { peer_id, .. } => Event::ProtocolError {
                                    peer_id,
                                    error: ProtocolError::BadBlocksRequest(error),
                                },
                                _ => unreachable!(),
                            };
                        }
                    }
                }
                // Only protocol 0 (identify) can receive requests at the moment.
                peers::Event::RequestIn { .. } => unreachable!(),

                // Remote is no longer interested in the response.
                // We don't do anything yet. The obsolescence is detected when trying to answer
                // it.
                peers::Event::RequestInCancel { .. } => {
                    guarded.to_process_pre_event = None;
                }

                // Successfully opened block announces substream.
                // The block announces substream is the main substream that determines whether
                // a "chain" is open.
                peers::Event::NotificationsOutResult {
                    peer_id,
                    notifications_protocol_index,
                    result: Ok(remote_handshake),
                } if *notifications_protocol_index % NOTIFICATIONS_PROTOCOLS_PER_CHAIN == 0 => {
                    let chain_index =
                        *notifications_protocol_index / NOTIFICATIONS_PROTOCOLS_PER_CHAIN;

                    // Check validity of the handshake.
                    let remote_handshake =
                        match protocol::decode_block_announces_handshake(remote_handshake) {
                            Ok(hs) => hs,
                            Err(err) => {
                                // TODO: must close the substream and unassigned the slot
                                return Event::ProtocolError {
                                    error: ProtocolError::BadBlockAnnouncesHandshake(err),
                                    peer_id: match guarded.to_process_pre_event.take().unwrap() {
                                        peers::Event::NotificationsOutResult {
                                            peer_id, ..
                                        } => peer_id,
                                        _ => unreachable!(),
                                    },
                                };
                            }
                        };

                    // The desirability of the transactions and grandpa substreams is always equal
                    // to whether the block announces substream is open.
                    self.inner
                        .set_peer_notifications_out_desired(
                            peer_id,
                            chain_index * NOTIFICATIONS_PROTOCOLS_PER_CHAIN + 1,
                            peers::DesiredState::DesiredReset,
                        )
                        .await;
                    self.inner
                        .set_peer_notifications_out_desired(
                            peer_id,
                            chain_index * NOTIFICATIONS_PROTOCOLS_PER_CHAIN + 2,
                            peers::DesiredState::DesiredReset,
                        )
                        .await;

                    let slot_ty = {
                        let mut ephemeral_guarded = self.ephemeral_guarded.lock().await;
                        let local_genesis = ephemeral_guarded.chains[chain_index]
                            .chain_config
                            .genesis_hash;
                        let remote_genesis = *remote_handshake.genesis_hash;

                        if remote_genesis != local_genesis {
                            let unassigned_slot_ty = self
                                .unassign_slot(&mut *ephemeral_guarded, chain_index, peer_id)
                                .await
                                .unwrap();

                            return match guarded.to_process_pre_event.take().unwrap() {
                                peers::Event::NotificationsOutResult { peer_id, .. } => {
                                    Event::ChainConnectAttemptFailed {
                                        peer_id,
                                        chain_index,
                                        unassigned_slot_ty,
                                        error: NotificationsOutErr::GenesisMismatch {
                                            local_genesis,
                                            remote_genesis,
                                        },
                                    }
                                }
                                _ => unreachable!(),
                            };
                        }

                        // Update the k-buckets.
                        if let Some(mut entry) = ephemeral_guarded.chains[chain_index]
                            .kbuckets
                            .entry(peer_id)
                            .into_occupied()
                        {
                            entry.set_state(&now, kademlia::kbuckets::PeerState::Connected);
                        }

                        if ephemeral_guarded.chains[chain_index]
                            .in_peers
                            .contains(peer_id)
                        {
                            SlotTy::Inbound
                        } else {
                            debug_assert!(ephemeral_guarded.chains[chain_index]
                                .out_peers
                                .contains(peer_id));
                            SlotTy::Outbound
                        }
                    };

                    let _was_inserted = guarded.open_chains.insert((peer_id.clone(), chain_index));
                    debug_assert!(_was_inserted);

                    let best_hash = *remote_handshake.best_hash;
                    let best_number = remote_handshake.best_number;
                    let role = remote_handshake.role;

                    return match guarded.to_process_pre_event.take().unwrap() {
                        peers::Event::NotificationsOutResult { peer_id, .. } => {
                            Event::ChainConnected {
                                peer_id,
                                chain_index,
                                slot_ty,
                                best_hash,
                                best_number,
                                role,
                            }
                        }
                        _ => unreachable!(),
                    };
                }

                // Successfully opened transactions substream.
                peers::Event::NotificationsOutResult {
                    notifications_protocol_index,
                    result: Ok(_),
                    ..
                } if *notifications_protocol_index % NOTIFICATIONS_PROTOCOLS_PER_CHAIN == 1 => {
                    // Nothing to do.
                    guarded.to_process_pre_event = None;
                }

                // Successfully opened Grandpa substream.
                // Need to send a Grandpa neighbor packet in response.
                peers::Event::NotificationsOutResult {
                    peer_id,
                    notifications_protocol_index,
                    result: Ok(_),
                    ..
                } if *notifications_protocol_index % NOTIFICATIONS_PROTOCOLS_PER_CHAIN == 2 => {
                    let chain_index =
                        *notifications_protocol_index / NOTIFICATIONS_PROTOCOLS_PER_CHAIN;
                    let ephemeral_guarded = self.ephemeral_guarded.lock().await;

                    let notification = {
                        let grandpa_config = *ephemeral_guarded.chains[chain_index]
                            .chain_config
                            .grandpa_protocol_config
                            .as_ref()
                            .unwrap();

                        protocol::GrandpaNotificationRef::Neighbor(protocol::NeighborPacket {
                            round_number: grandpa_config.round_number,
                            set_id: grandpa_config.set_id,
                            commit_finalized_height: grandpa_config.commit_finalized_height,
                        })
                        .scale_encoding()
                        .fold(Vec::new(), |mut a, b| {
                            a.extend_from_slice(b.as_ref());
                            a
                        })
                    };

                    let _ = self
                        .inner
                        .queue_notification(
                            peer_id,
                            *notifications_protocol_index,
                            notification.clone(),
                        )
                        .await;

                    guarded.to_process_pre_event = None;
                }

                // Unrecognized protocol.
                peers::Event::NotificationsOutResult { result: Ok(_), .. } => unreachable!(),

                // The underlying state machine is requesting our local handshake in order to
                // send it out.
                // This is a purely local event that isn't related to any networking activity.
                peers::Event::DesiredOutNotification {
                    id,
                    notifications_protocol_index,
                    ..
                } => {
                    let ephemeral_guarded = self.ephemeral_guarded.lock().await;
                    let chain_config = &ephemeral_guarded.chains
                        [*notifications_protocol_index / NOTIFICATIONS_PROTOCOLS_PER_CHAIN]
                        .chain_config;

                    let handshake = if *notifications_protocol_index
                        % NOTIFICATIONS_PROTOCOLS_PER_CHAIN
                        == 0
                    {
                        protocol::encode_block_announces_handshake(
                            protocol::BlockAnnouncesHandshakeRef {
                                best_hash: &chain_config.best_hash,
                                best_number: chain_config.best_number,
                                genesis_hash: &chain_config.genesis_hash,
                                role: chain_config.role,
                            },
                        )
                        .fold(Vec::new(), |mut a, b| {
                            a.extend_from_slice(b.as_ref());
                            a
                        })
                    } else if *notifications_protocol_index % NOTIFICATIONS_PROTOCOLS_PER_CHAIN == 1
                    {
                        Vec::new()
                    } else if *notifications_protocol_index % NOTIFICATIONS_PROTOCOLS_PER_CHAIN == 2
                    {
                        chain_config.role.scale_encoding().to_vec()
                    } else {
                        unreachable!()
                    };

                    self.inner
                        .open_out_notification(*id, now.clone(), handshake)
                        .await;

                    guarded.to_process_pre_event = None;
                }

                // Failed to open block announces substream.
                peers::Event::NotificationsOutResult {
                    notifications_protocol_index,
                    peer_id,
                    result: Err(_),
                } if *notifications_protocol_index % NOTIFICATIONS_PROTOCOLS_PER_CHAIN == 0 => {
                    let chain_index =
                        *notifications_protocol_index / NOTIFICATIONS_PROTOCOLS_PER_CHAIN;

                    let unassigned_slot_ty = self
                        .unassign_slot(
                            &mut *self.ephemeral_guarded.lock().await,
                            chain_index,
                            peer_id,
                        )
                        .await
                        .unwrap();

                    // As a slot has been unassigned, wake up the discovery process in order for
                    // it to be filled.
                    // TODO: correct?
                    // TODO: if necessary, mark another peer+substream tuple as desired to fill a slot
                    self.start_connect_needed.notify_additional(1);

                    match guarded.to_process_pre_event.take().unwrap() {
                        peers::Event::NotificationsOutResult {
                            peer_id,
                            result: Err(error),
                            ..
                        } => {
                            return Event::ChainConnectAttemptFailed {
                                peer_id,
                                chain_index,
                                unassigned_slot_ty,
                                error: NotificationsOutErr::Substream(error),
                            };
                        }
                        _ => unreachable!(),
                    }
                }

                // Other protocol.
                peers::Event::NotificationsOutResult { result: Err(_), .. } => {
                    guarded.to_process_pre_event = None;
                }

                // Remote closes our outbound block announces substream.
                peers::Event::NotificationsOutClose {
                    notifications_protocol_index,
                    peer_id,
                } if *notifications_protocol_index % NOTIFICATIONS_PROTOCOLS_PER_CHAIN == 0 => {
                    let chain_index =
                        *notifications_protocol_index / NOTIFICATIONS_PROTOCOLS_PER_CHAIN;

                    // The desirability of the transactions and grandpa substreams is always equal
                    // to whether the block announces substream is open.
                    //
                    // These two calls modify `self.inner`, but they are still cancellation-safe
                    // as they can be repeated multiple times.
                    self.inner
                        .set_peer_notifications_out_desired(
                            peer_id,
                            chain_index * NOTIFICATIONS_PROTOCOLS_PER_CHAIN + 1,
                            peers::DesiredState::NotDesired,
                        )
                        .await;
                    self.inner
                        .set_peer_notifications_out_desired(
                            peer_id,
                            chain_index * NOTIFICATIONS_PROTOCOLS_PER_CHAIN + 2,
                            peers::DesiredState::NotDesired,
                        )
                        .await;

                    // The chain is now considered as closed.
                    let was_open = guarded.open_chains.remove(&(peer_id.clone(), chain_index)); // TODO: cloning :(

                    if was_open {
                        // Update the k-buckets, marking the peer as disconnected.
                        let unassigned_slot_ty = {
                            let mut ephemeral_guarded = self.ephemeral_guarded.lock().await;

                            let unassigned_slot_ty = self
                                .unassign_slot(&mut *ephemeral_guarded, chain_index, peer_id)
                                .await
                                .unwrap();

                            if let Some(mut entry) = ephemeral_guarded.chains[chain_index]
                                .kbuckets
                                .entry(peer_id)
                                .into_occupied()
                            {
                                entry.set_state(&now, kademlia::kbuckets::PeerState::Disconnected);
                            }

                            unassigned_slot_ty
                        };

                        // As a slot has been unassigned, wake up the discovery process in order for
                        // it to be filled.
                        // TODO: correct?
                        // TODO: if necessary, mark another peer+substream tuple as desired to fill a slot
                        self.start_connect_needed.notify_additional(1);

                        return Event::ChainDisconnected {
                            chain_index,
                            peer_id: match guarded.to_process_pre_event.take().unwrap() {
                                peers::Event::NotificationsOutClose { peer_id, .. } => peer_id,
                                _ => unreachable!(),
                            },
                            unassigned_slot_ty,
                        };
                    }
                    guarded.to_process_pre_event = None;
                }

                // Other protocol.
                peers::Event::NotificationsOutClose {
                    peer_id,
                    notifications_protocol_index,
                    ..
                } => {
                    let chain_index =
                        *notifications_protocol_index / NOTIFICATIONS_PROTOCOLS_PER_CHAIN;

                    // The state of notification substreams other than block announces must
                    // always match the state of the block announces.
                    // Therefore, if the peer is considered open, try to reopen the substream that
                    // has just been closed.
                    // TODO: cloning of peer_id :-/
                    if guarded
                        .open_chains
                        .contains(&(peer_id.clone(), chain_index))
                    {
                        self.inner
                            .set_peer_notifications_out_desired(
                                peer_id,
                                *notifications_protocol_index,
                                peers::DesiredState::DesiredReset,
                            )
                            .await;
                    }

                    guarded.to_process_pre_event = None;
                }

                // Remote closes a substream.
                // There isn't anything to do as long as the remote doesn't close our local
                // outbound substream.
                peers::Event::NotificationsInClose { .. } => {
                    guarded.to_process_pre_event = None;
                }

                // Received a block announce.
                peers::Event::NotificationsIn {
                    notifications_protocol_index,
                    peer_id,
                    notification,
                } if *notifications_protocol_index % NOTIFICATIONS_PROTOCOLS_PER_CHAIN == 0 => {
                    let chain_index =
                        *notifications_protocol_index / NOTIFICATIONS_PROTOCOLS_PER_CHAIN;

                    // Don't report events about nodes we don't have an outbound substream with.
                    // TODO: think about possible race conditions regarding missing block
                    // announcements, as the remote will think we know it's at a certain block
                    // while we ignored its announcement ; it isn't problematic as long as blocks
                    // are generated continuously, as announcements will be generated periodically
                    // as well and the state will no longer mismatch
                    // TODO: cloning of peer_id :(
                    if !guarded
                        .open_chains
                        .contains(&(peer_id.clone(), chain_index))
                    {
                        guarded.to_process_pre_event = None;
                        continue;
                    }

                    // Check the format of the block announce.
                    if let Err(err) = protocol::decode_block_announce(notification) {
                        return Event::ProtocolError {
                            error: ProtocolError::BadBlockAnnounce(err),
                            peer_id: match guarded.to_process_pre_event.take().unwrap() {
                                peers::Event::NotificationsIn { peer_id, .. } => peer_id,
                                _ => unreachable!(),
                            },
                        };
                    }

                    return match guarded.to_process_pre_event.take().unwrap() {
                        peers::Event::NotificationsIn {
                            peer_id,
                            notification,
                            ..
                        } => Event::BlockAnnounce {
                            chain_index,
                            peer_id,
                            announce: EncodedBlockAnnounce(notification),
                        },
                        _ => unreachable!(),
                    };
                }

                // Received transaction notification.
                peers::Event::NotificationsIn {
                    peer_id,
                    notifications_protocol_index,
                    ..
                } if *notifications_protocol_index % NOTIFICATIONS_PROTOCOLS_PER_CHAIN == 1 => {
                    let chain_index =
                        *notifications_protocol_index / NOTIFICATIONS_PROTOCOLS_PER_CHAIN;

                    // Don't report events about nodes we don't have an outbound substream with.
                    // TODO: cloning of peer_id :(
                    if !guarded
                        .open_chains
                        .contains(&(peer_id.clone(), chain_index))
                    {
                        guarded.to_process_pre_event = None;
                        continue;
                    }

                    // TODO: this is unimplemented
                    guarded.to_process_pre_event = None;
                }

                // Received Grandpa notification.
                peers::Event::NotificationsIn {
                    notifications_protocol_index,
                    peer_id,
                    notification,
                } if *notifications_protocol_index % NOTIFICATIONS_PROTOCOLS_PER_CHAIN == 2 => {
                    let chain_index =
                        *notifications_protocol_index / NOTIFICATIONS_PROTOCOLS_PER_CHAIN;

                    // Don't report events about nodes we don't have an outbound substream with.
                    // TODO: cloning of peer_id :(
                    if !guarded
                        .open_chains
                        .contains(&(peer_id.clone(), chain_index))
                    {
                        guarded.to_process_pre_event = None;
                        continue;
                    }

                    let decoded_notif = match protocol::decode_grandpa_notification(notification) {
                        Ok(n) => n,
                        Err(err) => {
                            return Event::ProtocolError {
                                error: ProtocolError::BadGrandpaNotification(err),
                                peer_id: match guarded.to_process_pre_event.take().unwrap() {
                                    peers::Event::NotificationsIn { peer_id, .. } => peer_id,
                                    _ => unreachable!(),
                                },
                            };
                        }
                    };

                    // Commit messages are the only type of message that is important for
                    // light clients. Anything else is presently ignored.
                    if let protocol::GrandpaNotificationRef::Commit(_) = decoded_notif {
                        let notification = mem::take(notification);
                        guarded.to_process_pre_event = None;
                        return Event::GrandpaCommitMessage {
                            chain_index,
                            message: EncodedGrandpaCommitMessage(notification),
                        };
                    }

                    guarded.to_process_pre_event = None;
                }

                peers::Event::NotificationsIn { .. } => {
                    // Unrecognized notifications protocol.
                    unreachable!()
                }

                // Remote wants to open a block announces substream.
                // The block announces substream is the main substream that determines whether
                // a "chain" is open.
                peers::Event::DesiredInNotification {
                    peer_id,
                    handshake,
                    id: desired_in_notification_id,
                    notifications_protocol_index,
                } if (*notifications_protocol_index % NOTIFICATIONS_PROTOCOLS_PER_CHAIN) == 0 => {
                    let chain_index =
                        *notifications_protocol_index / NOTIFICATIONS_PROTOCOLS_PER_CHAIN;

                    // Immediately reject the substream if the handshake fails to parse.
                    if let Err(err) = protocol::decode_block_announces_handshake(handshake) {
                        self.inner
                            .in_notification_refuse(*desired_in_notification_id)
                            .await;

                        return Event::ProtocolError {
                            error: ProtocolError::BadBlockAnnouncesHandshake(err),
                            peer_id: match guarded.to_process_pre_event.take().unwrap() {
                                peers::Event::DesiredInNotification { peer_id, .. } => peer_id,
                                _ => unreachable!(),
                            },
                        };
                    }

                    let mut ephemeral_guarded = self.ephemeral_guarded.lock().await;

                    // If the peer doesn't already have an outbound slot, check whether we can
                    // allocate an inbound slot for it.
                    let has_out_slot = ephemeral_guarded.chains[chain_index]
                        .out_peers
                        .contains(peer_id);
                    if !has_out_slot
                        && ephemeral_guarded.chains[chain_index].in_peers.len()
                            >= usize::try_from(
                                ephemeral_guarded.chains[chain_index].chain_config.in_slots,
                            )
                            .unwrap_or(usize::max_value())
                    {
                        // All in slots are occupied. Refuse the substream.
                        drop(ephemeral_guarded);
                        self.inner
                            .in_notification_refuse(*desired_in_notification_id)
                            .await;
                        guarded.to_process_pre_event = None;
                        continue;
                    }

                    // At this point, accept the node can no longer fail.

                    // Generate the handshake to send back.
                    let handshake = {
                        let chain_config = &ephemeral_guarded.chains[chain_index].chain_config;
                        protocol::encode_block_announces_handshake(
                            protocol::BlockAnnouncesHandshakeRef {
                                best_hash: &chain_config.best_hash,
                                best_number: chain_config.best_number,
                                genesis_hash: &chain_config.genesis_hash,
                                role: chain_config.role,
                            },
                        )
                        .fold(Vec::new(), |mut a, b| {
                            a.extend_from_slice(b.as_ref());
                            a
                        })
                    };

                    if self
                        .inner
                        .in_notification_accept(*desired_in_notification_id, handshake)
                        .await
                        .is_ok()
                        && !has_out_slot
                    {
                        // TODO: future cancellation issue; if this future is cancelled, then trying to do the `in_notification_accept` again next time will panic
                        self.inner
                            .set_peer_notifications_out_desired(
                                peer_id,
                                *notifications_protocol_index,
                                peers::DesiredState::DesiredReset,
                            )
                            .await;

                        // The state modification is done at the very end, to not have any
                        // future cancellation issue.
                        let _was_inserted = ephemeral_guarded.chains[chain_index]
                            .in_peers
                            .insert(peer_id.clone());
                        debug_assert!(_was_inserted);

                        return match guarded.to_process_pre_event.take().unwrap() {
                            peers::Event::DesiredInNotification { peer_id, .. } => {
                                Event::InboundSlotAssigned {
                                    chain_index,
                                    peer_id,
                                }
                            }
                            _ => unreachable!(),
                        };
                    }
                    guarded.to_process_pre_event = None;
                }

                // Remote wants to open a transactions substream.
                peers::Event::DesiredInNotification {
                    peer_id,
                    id: desired_in_notification_id,
                    notifications_protocol_index,
                    ..
                } if (*notifications_protocol_index % NOTIFICATIONS_PROTOCOLS_PER_CHAIN) == 1 => {
                    let chain_index =
                        *notifications_protocol_index / NOTIFICATIONS_PROTOCOLS_PER_CHAIN;

                    // Accept the substream only if the peer is "chain connected".
                    if guarded
                        .open_chains // TODO: clone :-/
                        .contains(&(peer_id.clone(), chain_index))
                    {
                        // It doesn't matter if the substream is obsolete.
                        let _ = self
                            .inner
                            .in_notification_accept(*desired_in_notification_id, Vec::new())
                            .await;
                    } else {
                        self.inner
                            .in_notification_refuse(*desired_in_notification_id)
                            .await;
                    }
                    guarded.to_process_pre_event = None;
                }

                // Remote wants to open a grandpa substream.
                peers::Event::DesiredInNotification {
                    peer_id,
                    id: desired_in_notification_id,
                    notifications_protocol_index,
                    ..
                } if (*notifications_protocol_index % NOTIFICATIONS_PROTOCOLS_PER_CHAIN) == 2 => {
                    let ephemeral_guarded = self.ephemeral_guarded.lock().await;
                    let chain_index =
                        *notifications_protocol_index / NOTIFICATIONS_PROTOCOLS_PER_CHAIN;

                    // Reject the substream if the this peer isn't "chain connected".
                    if !guarded
                        .open_chains // TODO: clone :-/
                        .contains(&(peer_id.clone(), chain_index))
                    {
                        self.inner
                            .in_notification_refuse(*desired_in_notification_id)
                            .await;
                        guarded.to_process_pre_event = None;
                        continue;
                    }

                    // Peer is indeed connected. Accept the substream.

                    // Build the handshake to send back.
                    let handshake = {
                        ephemeral_guarded.chains[chain_index]
                            .chain_config
                            .role
                            .scale_encoding()
                            .to_vec()
                    };

                    // It doesn't matter if the substream is obsolete.
                    let _ = self
                        .inner
                        .in_notification_accept(*desired_in_notification_id, handshake)
                        .await;

                    guarded.to_process_pre_event = None;
                }

                peers::Event::DesiredInNotification { .. } => {
                    // Unrecognized notifications protocol.
                    unreachable!()
                }

                peers::Event::DesiredInNotificationCancel { .. } => {
                    guarded.to_process_pre_event = None;
                }
            }

            debug_assert!(guarded.to_process_pre_event.is_none());
        }
    }

    /// Performs a round of Kademlia discovery.
    ///
    /// This future yields once a list of nodes on the network has been discovered, or a problem
    /// happened.
    pub async fn kademlia_discovery_round(
        &'_ self,
        now: TNow,
        chain_index: usize,
    ) -> Result<DiscoveryInsert<'_, TNow>, DiscoveryError> {
        let random_peer_id = {
            let mut randomness = self.randomness.lock().await;
            let pub_key = randomness.sample(rand::distributions::Standard);
            peer_id::PeerId::from_public_key(&peer_id::PublicKey::Ed25519(pub_key))
        };

        let queried_peer = {
            let ephemeral_guarded = self.ephemeral_guarded.lock().await;
            let peer_id = ephemeral_guarded.chains[chain_index]
                .kbuckets
                .closest_entries(&random_peer_id)
                // TODO: instead of filtering by connectd only, connect to nodes if not connected
                .find(|(_, addresses)| addresses.iter_connected().count() != 0)
                .map(|(peer_id, _)| peer_id.clone());
            peer_id
        };

        if let Some(queried_peer) = queried_peer {
            let outcome = self
                .kademlia_find_node(&queried_peer, now, chain_index, random_peer_id.as_bytes())
                .await
                .map_err(DiscoveryError::FindNode)?;
            Ok(DiscoveryInsert {
                service: self,
                outcome,
                chain_index,
            })
        } else {
            Err(DiscoveryError::NoPeer)
        }
    }

    /// Sends a Kademlia "find node" request to a single peer, and waits for it to answer.
    ///
    /// Returns an error if there is no active connection with that peer.
    pub async fn kademlia_find_node(
        &'_ self,
        target: &PeerId,
        now: TNow,
        chain_index: usize,
        close_to_key: &[u8],
    ) -> Result<Vec<(peer_id::PeerId, Vec<multiaddr::Multiaddr>)>, KademliaFindNodeError> {
        let request_data = kademlia::build_find_node_request(close_to_key);
        // The timeout needs to be long enough to potentially download the maximum
        // response size of 1 MiB. Assuming a 128 kiB/sec connection, that's 8 seconds.
        let timeout = now + Duration::from_secs(8);
        let response = self
            .inner
            .request(
                target,
                self.protocol_index(chain_index, 2),
                request_data,
                timeout,
            )
            .await
            .map_err(KademliaFindNodeError::RequestFailed)?;
        let decoded = kademlia::decode_find_node_response(&response)
            .map_err(KademliaFindNodeError::DecodeError)?;
        Ok(decoded)
    }

    /// Allocates a [`PendingId`] and returns a [`StartConnect`] indicating a multiaddress that
    /// the API user must try to dial.
    ///
    /// Later, the API user must use [`ChainNetwork::pending_outcome_ok`] or
    /// [`ChainNetwork::pending_outcome_err`] to report how the connection attempt went.
    ///
    /// The returned [`StartConnect`] contains the [`StartConnect::timeout`] field. It is the
    /// responsibility of the API user to ensure that [`ChainNetwork::pending_outcome_err`] is
    /// called if this timeout is reached.
    ///
    /// If no outgoing connection is desired, the method waits until there is one.
    // TODO: give more control, with number of slots and node choice
    // TODO: this API with now is a bit hacky?
    pub async fn next_start_connect<'a>(&self, now: impl FnOnce() -> TNow) -> StartConnect<TNow> {
        loop {
            let mut pending_lock = self.ephemeral_guarded.lock().await;
            let pending = &mut *pending_lock; // Prevents borrow checker issues.

            // Ask the underlying state machine which nodes are desired but don't have any
            // associated connection attempt yet.
            // Since the underlying state machine is only made aware of connections in
            // `pending_outcome_ok`, we must filter out nodes that already have an associated
            // `PendingId`.
            let unfulfilled_desired_peers = self.inner.unfulfilled_desired_peers().await;

            for peer_id in unfulfilled_desired_peers {
                // TODO: allow more than one simultaneous dial per peer, and distribute the dials so that we don't just return the same peer multiple times in a row while there are other peers waiting
                let entry = match pending.num_pending_per_peer.entry(peer_id) {
                    hashbrown::hash_map::Entry::Occupied(_) => continue,
                    hashbrown::hash_map::Entry::Vacant(entry) => entry,
                };

                // TODO: O(n)
                let multiaddr: multiaddr::Multiaddr = {
                    let potential = pending
                        .chains
                        .iter_mut()
                        .flat_map(|chain| chain.kbuckets.iter_mut())
                        .find(|(p, _)| **p == *entry.key())
                        .and_then(|(_, addrs)| addrs.addr_to_pending());
                    match potential {
                        Some(a) => a.clone(),
                        None => continue,
                    }
                };

                // TODO: O(n)
                for chain in &mut pending.chains {
                    if let Some(_) = chain.kbuckets.get_mut(entry.key()) {
                        // TODO: mark address as pending
                    }
                }

                let now = now();
                let pending_id = PendingId(pending.pending_ids.insert((
                    entry.key().clone(),
                    multiaddr.clone(),
                    now.clone(),
                )));

                let start_connect = StartConnect {
                    expected_peer_id: entry.key().clone(),
                    id: pending_id,
                    multiaddr,
                    timeout: now + self.handshake_timeout,
                };

                entry.insert(NonZeroUsize::new(1).unwrap());

                return start_connect;
            }

            // No valid desired peer has been found.
            // We start listening for an event, unlock the mutex, and wait until the event is
            // notified. This needs to be done in this order, in particular the mutex needs to be
            // unlocked after we start listening for events, to avoid race conditions.
            // The rest of the code of this state machine makes sure to notify the event when
            // there is a potential new desired peer or known address.
            let event_listener = self.start_connect_needed.listen();
            drop::<MutexGuard<_>>(pending_lock);
            event_listener.await;
        }
    }

    /// Reads data coming from the connection, updates the internal state machine, and writes data
    /// destined to the connection through the [`ReadWrite`].
    ///
    /// If an error is returned, the connection should be destroyed altogether and the
    /// [`ConnectionId`] is no longer valid. You should continue calling this function until
    /// an error is returned, even if the [`ReadWrite`] indicates a full shutdown.
    ///
    /// # Panic
    ///
    /// Panics if the [`ConnectionId`] isn't a valid connection. Once this function returns an
    /// error, is no longer valid to call this function with this [`ConnectionId`].
    ///
    pub async fn read_write(
        &self,
        connection_id: ConnectionId,
        read_write: &'_ mut ReadWrite<'_, TNow>,
    ) -> Result<(), peers::ConnectionError> {
        self.inner.read_write(connection_id, read_write).await
    }

    /// Returns an iterator to the list of [`PeerId`]s that we have an established connection
    /// with.
    pub async fn peers_list(&self) -> impl Iterator<Item = PeerId> {
        self.inner.peers_list().await
    }

    ///
    ///
    /// Returns the [`PeerId`] that now has an outbound slot. This information can be used for
    /// logging purposes. Keep in mind, however, that [`ChainNetwork::next_event`] might unassign
    /// slots only for them to be immediately assigned again in this function. If you naively
    /// log the value returned by this function and the value returned by `next_event`, you might
    /// see the slot assignments and de-assignments in the wrong order.
    // TODO: docs
    // TODO: when to call this?
    pub async fn assign_slots(&self, chain_index: usize) -> Option<PeerId> {
        let mut lock = self.ephemeral_guarded.lock().await;
        let chain = &mut lock.chains[chain_index];

        let list = {
            let mut list = chain.kbuckets.iter().collect::<Vec<_>>();
            let mut randomness = self.randomness.lock().await;
            list.shuffle(&mut *randomness);
            list
        };

        for (peer_id, _) in list {
            // Check if maximum number of slots is reached.
            if chain.out_peers.len()
                >= usize::try_from(chain.chain_config.out_slots).unwrap_or(usize::max_value())
            {
                break;
            }

            // Don't assign slots to peers that already have a slot.
            if chain.out_peers.contains(peer_id) || chain.in_peers.contains(peer_id) {
                continue;
            }

            // It is now guaranteed that this peer will be assigned an outbound slot.

            // The peer is marked as desired before inserting it in `out_peers`, to handle
            // potential future cancellation issues.
            self.inner
                .set_peer_notifications_out_desired(
                    peer_id,
                    chain_index * NOTIFICATIONS_PROTOCOLS_PER_CHAIN,
                    peers::DesiredState::DesiredReset, // TODO: ?
                )
                .await;
            chain.out_peers.insert(peer_id.clone());

            self.start_connect_needed.notify_additional(1);
            return Some(peer_id.clone());
        }

        None
    }

    /// Removes the slot assignment of the given peer, if any.
    async fn unassign_slot(
        &self,
        ephemeral_guarded: &mut EphemeralGuarded<TNow>,
        chain_index: usize,
        peer_id: &PeerId,
    ) -> Option<SlotTy> {
        self.inner
            .set_peer_notifications_out_desired(
                peer_id,
                chain_index * NOTIFICATIONS_PROTOCOLS_PER_CHAIN,
                peers::DesiredState::NotDesired,
            )
            .await;

        let was_in_out = ephemeral_guarded.chains[chain_index]
            .out_peers
            .remove(peer_id);
        let was_in_in = ephemeral_guarded.chains[chain_index]
            .in_peers
            .remove(peer_id);

        match (was_in_in, was_in_out) {
            (true, false) => Some(SlotTy::Inbound),
            (false, true) => Some(SlotTy::Outbound),
            (false, false) => None,
            (true, true) => {
                unreachable!()
            }
        }
    }
}

/// User must start connecting to the given multiaddress.
///
/// Either [`ChainNetwork::pending_outcome_ok`] or [`ChainNetwork::pending_outcome_err`] must
/// later be called in order to inform of the outcome of the connection.
#[derive(Debug)]
#[must_use]
pub struct StartConnect<TNow> {
    /// Identifier of this connection request. Must be passed back later.
    pub id: PendingId,
    /// Address to attempt to connect to.
    pub multiaddr: multiaddr::Multiaddr,
    /// [`PeerId`] that is expected to be reached with this connection attempt.
    pub expected_peer_id: PeerId,
    /// When the attempt should be considered as a failure. You must call
    /// [`ChainNetwork::pending_outcome_err`] if this moment is reached.
    pub timeout: TNow,
}

/// Event generated by [`ChainNetwork::next_event`].
#[derive(Debug)]
pub enum Event<'a, TNow> {
    /// Established a transport-level connection (e.g. a TCP socket) with the given peer.
    Connected(peer_id::PeerId),

    /// A transport-level connection (e.g. a TCP socket) has been closed.
    ///
    /// This event is called unconditionally when a connection with the given peer has been
    /// closed. If `chain_indices` isn't empty, this event is also equivalent to one or more
    /// [`Event::ChainDisconnected`] events.
    Disconnected {
        peer_id: peer_id::PeerId,
        chain_indices: Vec<usize>,
    },

    ChainConnected {
        chain_index: usize,
        peer_id: peer_id::PeerId,
        /// Type of the slot that the peer has.
        slot_ty: SlotTy,
        /// Role the node reports playing on the network.
        role: protocol::Role,
        /// Height of the best block according to this node.
        best_number: u64,
        /// Hash of the best block according to this node.
        best_hash: [u8; 32],
    },
    ChainDisconnected {
        peer_id: peer_id::PeerId,
        chain_index: usize,
        /// Type of the slot that the peer had and no longer has.
        unassigned_slot_ty: SlotTy,
    },

    /// An attempt has been made to open the given chain, but a problem happened.
    ChainConnectAttemptFailed {
        chain_index: usize,
        peer_id: peer_id::PeerId,
        /// Problem that happened.
        error: NotificationsOutErr,
        /// Type of the slot that the peer had and no longer has.
        unassigned_slot_ty: SlotTy,
    },

    /// The given peer has opened a block announces substream with the local node, and an inbound
    /// slot has been assigned locally to this peer.
    ///
    /// A [`Event::ChainConnected`] or [`Event::ChainConnectAttemptFailed`] will later be
    /// generated for this peer.
    InboundSlotAssigned {
        chain_index: usize,
        peer_id: peer_id::PeerId,
    },

    /// Received a new block announce from a peer.
    ///
    /// Can only happen after a [`Event::ChainConnected`] with the given `PeerId` and chain index
    /// combination has happened.
    BlockAnnounce {
        /// Identity of the sender of the block announce.
        peer_id: peer_id::PeerId,
        /// Index of the chain the block relates to.
        chain_index: usize,
        announce: EncodedBlockAnnounce,
    },

    /// Received a GrandPa commit message from the network.
    GrandpaCommitMessage {
        /// Index of the chain the commit message relates to.
        chain_index: usize,
        message: EncodedGrandpaCommitMessage,
    },

    /// Error in the protocol in a connection, such as failure to decode a message. This event
    /// doesn't have any consequence on the health of the connection, and is purely for diagnostic
    /// purposes.
    ProtocolError {
        /// Peer that has caused the protocol error.
        peer_id: peer_id::PeerId,
        /// Error that happened.
        error: ProtocolError,
    },

    /// A remote has sent a request for identification information.
    ///
    /// You are strongly encouraged to call [`IdentifyRequestIn::respond`].
    IdentifyRequestIn {
        /// Remote that has sent the request.
        peer_id: PeerId,
        /// Object allowing sending back the answer.
        request: IdentifyRequestIn<'a, TNow>,
    },
    /// A remote has sent a request for blocks.
    ///
    /// Can only happen for chains where [`ChainConfig::allow_inbound_block_requests`] is `true`.
    ///
    /// You are strongly encouraged to call [`BlocksRequestIn::respond`].
    BlocksRequestIn {
        /// Remote that has sent the request.
        peer_id: PeerId,
        /// Index of the chain concerned by the request.
        chain_index: usize,
        /// Information about the request.
        config: protocol::BlocksRequestConfig,
        /// Object allowing sending back the answer.
        request: BlocksRequestIn<'a, TNow>,
    },
    /*Transactions {
        peer_id: peer_id::PeerId,
        transactions: EncodedTransactions,
    }*/
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SlotTy {
    Inbound,
    Outbound,
}

/// Error that can happen when trying to open an outbound notifications substream.
#[derive(Debug, Clone, derive_more::Display)]
pub enum NotificationsOutErr {
    /// Error in the underlying protocol.
    Substream(peers::NotificationsOutErr),
    /// Mismatch between the genesis hash of the remote and the local genesis hash.
    #[display(fmt = "Mismatch between the genesis hash of the remote and the local genesis hash")]
    GenesisMismatch {
        /// Hash of the genesis block of the chain according to the local node.
        local_genesis: [u8; 32],
        /// Hash of the genesis block of the chain according to the remote node.
        remote_genesis: [u8; 32],
    },
}

/// Undecoded but valid block announce handshake.
pub struct EncodedBlockAnnounceHandshake(Vec<u8>);

impl EncodedBlockAnnounceHandshake {
    /// Returns the decoded version of the handshake.
    pub fn decode(&self) -> protocol::BlockAnnouncesHandshakeRef {
        protocol::decode_block_announces_handshake(&self.0).unwrap()
    }
}

impl fmt::Debug for EncodedBlockAnnounceHandshake {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.decode(), f)
    }
}

/// Undecoded but valid block announce.
#[derive(Clone)]
pub struct EncodedBlockAnnounce(Vec<u8>);

impl EncodedBlockAnnounce {
    /// Returns the decoded version of the announcement.
    pub fn decode(&self) -> protocol::BlockAnnounceRef {
        protocol::decode_block_announce(&self.0).unwrap()
    }
}

impl fmt::Debug for EncodedBlockAnnounce {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.decode(), f)
    }
}

/// Undecoded but valid GrandPa commit message.
#[derive(Clone)]
pub struct EncodedGrandpaCommitMessage(Vec<u8>);

impl EncodedGrandpaCommitMessage {
    /// Returns the encoded bytes of the commit message.
    pub fn as_encoded(&self) -> &[u8] {
        // Skip the first byte because `self.0` is a `GrandpaNotificationRef`.
        &self.0[1..]
    }

    /// Returns the decoded version of the commit message.
    pub fn decode(&self) -> protocol::CommitMessageRef {
        match protocol::decode_grandpa_notification(&self.0) {
            Ok(protocol::GrandpaNotificationRef::Commit(msg)) => msg,
            _ => unreachable!(),
        }
    }
}

impl fmt::Debug for EncodedGrandpaCommitMessage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.decode(), f)
    }
}

/// Successfull outcome to [`ChainNetwork::kademlia_discovery_round`].
#[must_use]
pub struct DiscoveryInsert<'a, TNow> {
    service: &'a ChainNetwork<TNow>,
    outcome: Vec<(peer_id::PeerId, Vec<multiaddr::Multiaddr>)>,

    /// Index within [`Config::chains`] corresponding to the chain the nodes belong to.
    chain_index: usize,
}

impl<'a, TNow> DiscoveryInsert<'a, TNow>
where
    TNow: Clone + Add<Duration, Output = TNow> + Sub<TNow, Output = Duration> + Ord,
{
    /// Returns the list of [`peer_id::PeerId`]s that will be inserted and their addresses.
    pub fn discovered(
        &self,
    ) -> impl Iterator<
        Item = (
            &peer_id::PeerId,
            impl Iterator<Item = &multiaddr::Multiaddr>,
        ),
    > {
        self.outcome
            .iter()
            .map(|(peer_id, addrs)| (peer_id, addrs.iter()))
    }

    /// Insert the results in the [`ChainNetwork`].
    pub async fn insert(self, now: &TNow) {
        self.service
            .discover(now, self.chain_index, self.outcome)
            .await
    }
}

/// See [`Event::IdentifyRequestIn`].
#[must_use]
pub struct IdentifyRequestIn<'a, TNow> {
    service: &'a ChainNetwork<TNow>,
    request_id: peers::RequestId,
    observed_addr: multiaddr::Multiaddr,
}

impl<'a, TNow> IdentifyRequestIn<'a, TNow>
where
    TNow: Clone + Add<Duration, Output = TNow> + Sub<TNow, Output = Duration> + Ord,
{
    /// Queue the response to send back. The future provided by [`ChainNetwork::read_write`] will
    /// automatically be woken up.
    ///
    /// Has no effect if the connection that sends the request no longer exists.
    pub async fn respond(self, agent_version: &str) {
        let response = {
            protocol::build_identify_response(protocol::IdentifyResponse {
                protocol_version: "/substrate/1.0".into(), // TODO: same value as in Substrate
                agent_version: agent_version.into(),
                ed25519_public_key: Cow::Borrowed(
                    self.service.inner.noise_key().libp2p_public_ed25519_key(),
                ),
                listen_addrs: iter::empty(), // TODO:
                observed_addr: Cow::Borrowed(&self.observed_addr),
                protocols: self
                    .service
                    .inner
                    .request_response_protocols()
                    .filter(|p| p.inbound_allowed)
                    .map(|p| &p.name[..])
                    .chain(
                        self.service
                            .inner
                            .notification_protocols()
                            .map(|p| &p.protocol_name[..]),
                    ),
            })
            .fold(Vec::new(), |mut a, b| {
                a.extend_from_slice(b.as_ref());
                a
            })
        };

        let _ = self
            .service
            .inner
            .respond(self.request_id, Ok(response))
            .await;
    }
}

impl<'a, TNow> fmt::Debug for IdentifyRequestIn<'a, TNow> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("IdentifyRequestIn").finish()
    }
}

/// See [`Event::BlocksRequestIn`].
#[must_use]
pub struct BlocksRequestIn<'a, TNow> {
    service: &'a ChainNetwork<TNow>,
    request_id: peers::RequestId,
}

impl<'a, TNow> BlocksRequestIn<'a, TNow>
where
    TNow: Clone + Add<Duration, Output = TNow> + Sub<TNow, Output = Duration> + Ord,
{
    /// Queue the response to send back. The future provided by [`ChainNetwork::read_write`] will
    /// automatically be woken up.
    ///
    /// Pass `None` in order to deny the request. Do this if blocks aren't available locally.
    ///
    /// Has no effect if the connection that sends the request no longer exists.
    pub async fn respond(self, response: Option<Vec<protocol::BlockData>>) {
        let response = if let Some(response) = response {
            Ok(
                protocol::build_block_response(response).fold(Vec::new(), |mut a, b| {
                    a.extend_from_slice(b.as_ref());
                    a
                }),
            )
        } else {
            Err(())
        };

        let _ = self.service.inner.respond(self.request_id, response).await;
    }
}

impl<'a, TNow> fmt::Debug for BlocksRequestIn<'a, TNow> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("BlocksRequestIn").finish()
    }
}

/// Error during [`ChainNetwork::kademlia_discovery_round`].
#[derive(Debug, derive_more::Display)]
pub enum DiscoveryError {
    NoPeer,
    FindNode(KademliaFindNodeError),
}

/// Error during [`ChainNetwork::kademlia_find_node`].
#[derive(Debug, derive_more::Display)]
pub enum KademliaFindNodeError {
    RequestFailed(peers::RequestError),
    DecodeError(kademlia::DecodeFindNodeResponseError),
}

/// Error returned by [`ChainNetwork::blocks_request`].
#[derive(Debug, derive_more::Display)]
pub enum BlocksRequestError {
    /// Error while waiting for the response from the peer.
    Request(peers::RequestError),
    /// Error while decoding the response returned by the peer.
    Decode(protocol::DecodeBlockResponseError),
    /// Block request doesn't request headers, and as such its validity cannot be verified.
    NotVerifiable,
    /// Response returned by the remote doesn't contain any entry.
    EmptyResponse,
    /// Start of the response doesn't correspond to the requested start.
    InvalidStart,
    /// Error at a specific index in the response.
    #[display(fmt = "Error in response at offset {}: {}", index, error)]
    Entry {
        /// Index in the response where the problem happened.
        index: usize,
        /// Problem in question.
        error: BlocksRequestResponseEntryError,
    },
}

/// See [`BlocksRequestError`].
#[derive(Debug, derive_more::Display)]
pub enum BlocksRequestResponseEntryError {
    /// One of the requested fields is missing from the block.
    MissingField,
    /// The header has an extrinsics root that doesn't match the body. Can only happen if both the
    /// header and body were requested.
    #[display(fmt = "The header has an extrinsics root that doesn't match the body")]
    InvalidExtrinsicsRoot {
        /// Extrinsics root that was calculated from the body.
        calculated: [u8; 32],
        /// Extrinsics root found in the header.
        in_header: [u8; 32],
    },
    /// The header has an invalid format.
    InvalidHeader,
    /// The hash of the header doesn't match the hash provided by the remote.
    InvalidHash,
}

/// Error returned by [`ChainNetwork::storage_proof_request`].
#[derive(Debug, derive_more::Display, Clone)]
pub enum StorageProofRequestError {
    Request(peers::RequestError),
    Decode(protocol::DecodeStorageProofResponseError),
}

/// Error returned by [`ChainNetwork::call_proof_request`].
#[derive(Debug, Clone, derive_more::Display)]
pub enum CallProofRequestError {
    Request(peers::RequestError),
    Decode(protocol::DecodeCallProofResponseError),
}

impl CallProofRequestError {
    /// Returns `true` if this is caused by networking issues, as opposed to a consensus-related
    /// issue.
    pub fn is_network_problem(&self) -> bool {
        match self {
            CallProofRequestError::Request(_) => true,
            CallProofRequestError::Decode(_) => false,
        }
    }
}

/// Error returned by [`ChainNetwork::grandpa_warp_sync_request`].
#[derive(Debug, derive_more::Display)]
pub enum GrandpaWarpSyncRequestError {
    Request(peers::RequestError),
    Decode(protocol::DecodeGrandpaWarpSyncResponseError),
}

/// Error returned by [`ChainNetwork::state_request_unchecked`].
#[derive(Debug, derive_more::Display)]
pub enum StateRequestError {
    Request(peers::RequestError),
    Decode(protocol::DecodeStateResponseError),
}

/// See [`Event::ProtocolError`].
#[derive(Debug, derive_more::Display)]
pub enum ProtocolError {
    /// Error in an incoming substream.
    InboundError(InboundError),
    /// Error while decoding the handshake of the block announces substream.
    BadBlockAnnouncesHandshake(protocol::BlockAnnouncesHandshakeDecodeError),
    /// Error while decoding a received block announce.
    BadBlockAnnounce(protocol::DecodeBlockAnnounceError),
    /// Error while decoding a received Grandpa notification.
    BadGrandpaNotification(protocol::DecodeGrandpaNotificationError),
    /// Received an invalid identify request.
    BadIdentifyRequest,
    /// Error while decoding a received blocks request.
    BadBlocksRequest(protocol::DecodeBlockRequestError),
}
