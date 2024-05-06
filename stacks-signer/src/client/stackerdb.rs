// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2024 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
//
use blockstack_lib::chainstate::stacks::StacksTransaction;
use blockstack_lib::net::api::poststackerdbchunk::StackerDBErrorCodes;
use hashbrown::HashMap;
use libsigner::{MessageSlotID, SignerMessage, SignerSession, StackerDBMessage, StackerDBSession};
use libstackerdb::{StackerDBChunkAckData, StackerDBChunkData};
use slog::{slog_debug, slog_error, slog_warn};
use stacks_common::codec::{read_next, StacksMessageCodec};
use stacks_common::types::chainstate::StacksPrivateKey;
use stacks_common::{debug, error, warn};
use wsts::net::Packet;

use super::ClientError;
use crate::client::retry_with_exponential_backoff;
use crate::config::SignerConfig;
use crate::signer::SignerSlotID;

/// The StackerDB client for communicating with the .signers contract
pub struct StackerDB {
    /// The stacker-db sessions for each signer set and message type.
    /// Maps message ID to the DB session.
    signers_message_stackerdb_sessions: HashMap<MessageSlotID, StackerDBSession>,
    /// The private key used in all stacks node communications
    stacks_private_key: StacksPrivateKey,
    /// A map of a message ID to last chunk version for each session
    slot_versions: HashMap<MessageSlotID, HashMap<SignerSlotID, u32>>,
    /// The signer slot ID -- the index into the signer list for this signer daemon's signing key.
    signer_slot_id: SignerSlotID,
    /// The reward cycle of the connecting signer
    reward_cycle: u64,
    /// The stacker-db transaction msg session for the NEXT reward cycle
    next_transaction_session: StackerDBSession,
}

impl From<&SignerConfig> for StackerDB {
    fn from(config: &SignerConfig) -> Self {
        Self::new(
            &config.node_host,
            config.stacks_private_key,
            config.mainnet,
            config.reward_cycle,
            config.signer_slot_id,
        )
    }
}
impl StackerDB {
    /// Create a new StackerDB client
    pub fn new(
        host: &str,
        stacks_private_key: StacksPrivateKey,
        is_mainnet: bool,
        reward_cycle: u64,
        signer_slot_id: SignerSlotID,
    ) -> Self {
        let mut signers_message_stackerdb_sessions = HashMap::new();
        for msg_id in MessageSlotID::ALL {
            signers_message_stackerdb_sessions.insert(
                *msg_id,
                StackerDBSession::new(host, msg_id.stacker_db_contract(is_mainnet, reward_cycle)),
            );
        }
        let next_transaction_session = StackerDBSession::new(
            host,
            MessageSlotID::Transactions
                .stacker_db_contract(is_mainnet, reward_cycle.wrapping_add(1)),
        );

        Self {
            signers_message_stackerdb_sessions,
            stacks_private_key,
            slot_versions: HashMap::new(),
            signer_slot_id,
            reward_cycle,
            next_transaction_session,
        }
    }

    /// Sends messages to the .signers stacker-db with an exponential backoff retry
    pub fn send_message_with_retry(
        &mut self,
        message: StackerDBMessage,
    ) -> Result<StackerDBChunkAckData, ClientError> {
        let msg_id = message.msg_id();
        let signer_message = SignerMessage {
            reward_cycle: self.reward_cycle,
            message,
        };
        let message_bytes = signer_message.serialize_to_vec();
        self.send_message_bytes_with_retry(&msg_id, message_bytes)
    }

    /// Sends message (as a raw msg ID and bytes) to the .signers stacker-db with an
    /// exponential backoff retry
    pub fn send_message_bytes_with_retry(
        &mut self,
        msg_id: &MessageSlotID,
        message_bytes: Vec<u8>,
    ) -> Result<StackerDBChunkAckData, ClientError> {
        let slot_id = self.signer_slot_id;
        loop {
            let mut slot_version = if let Some(versions) = self.slot_versions.get_mut(msg_id) {
                if let Some(version) = versions.get(&slot_id) {
                    *version
                } else {
                    versions.insert(slot_id, 0);
                    1
                }
            } else {
                let mut versions = HashMap::new();
                versions.insert(slot_id, 0);
                self.slot_versions.insert(*msg_id, versions);
                1
            };

            let mut chunk = StackerDBChunkData::new(slot_id.0, slot_version, message_bytes.clone());
            chunk.sign(&self.stacks_private_key)?;

            let Some(session) = self.signers_message_stackerdb_sessions.get_mut(msg_id) else {
                panic!("FATAL: would loop forever trying to send a message with ID {}, for which we don't have a session", msg_id);
            };

            debug!(
                "Sending a chunk to stackerdb slot ID {slot_id} with version {slot_version} and message ID {msg_id} to contract {:?}!\n{chunk:?}",
                &session.stackerdb_contract_id
            );

            let send_request = || session.put_chunk(&chunk).map_err(backoff::Error::transient);
            let chunk_ack: StackerDBChunkAckData = retry_with_exponential_backoff(send_request)?;

            if let Some(versions) = self.slot_versions.get_mut(msg_id) {
                // NOTE: per the above, this is always executed
                versions.insert(slot_id, slot_version.saturating_add(1));
            } else {
                return Err(ClientError::NotConnected);
            }

            if chunk_ack.accepted {
                debug!("Chunk accepted by stackerdb: {chunk_ack:?}");
                return Ok(chunk_ack);
            } else {
                warn!("Chunk rejected by stackerdb: {chunk_ack:?}");
            }
            if let Some(code) = chunk_ack.code {
                match StackerDBErrorCodes::from_code(code) {
                    Some(StackerDBErrorCodes::DataAlreadyExists) => {
                        if let Some(slot_metadata) = chunk_ack.metadata {
                            warn!("Failed to send message to stackerdb due to wrong version number. Attempted {}. Expected {}. Retrying...", slot_version, slot_metadata.slot_version);
                            slot_version = slot_metadata.slot_version;
                        } else {
                            warn!("Failed to send message to stackerdb due to wrong version number. Attempted {}. Expected unknown version number. Incrementing and retrying...", slot_version);
                        }
                        if let Some(versions) = self.slot_versions.get_mut(msg_id) {
                            // NOTE: per the above, this is always executed
                            versions.insert(slot_id, slot_version.saturating_add(1));
                        } else {
                            return Err(ClientError::NotConnected);
                        }
                    }
                    _ => {
                        warn!("Failed to send message to stackerdb: {:?}", chunk_ack);
                        return Err(ClientError::PutChunkRejected(
                            chunk_ack
                                .reason
                                .unwrap_or_else(|| "No reason given".to_string()),
                        ));
                    }
                }
            }
        }
    }

    /// Get all signer messages from stackerdb for the given slot IDs
    /// and reward cycle number
    fn get_messages(
        session: &mut StackerDBSession,
        slot_ids: &[u32],
        reward_cycle: u64,
    ) -> Result<Vec<SignerMessage>, ClientError> {
        let mut messages = vec![];
        let send_request = || {
            session
                .get_latest_chunks(slot_ids)
                .map_err(backoff::Error::transient)
        };
        let chunk_ack = retry_with_exponential_backoff(send_request)?;
        for (i, chunk) in chunk_ack.iter().enumerate() {
            let Some(data) = chunk else {
                continue;
            };
            let Ok(message) = read_next::<SignerMessage, _>(&mut &data[..]) else {
                if !data.is_empty() {
                    warn!("Failed to deserialize chunk data into a StackerDBMessage");
                    debug!("slot #{i}: Failed chunk ({}): {data:?}", &data.len(),);
                }
                continue;
            };
            if message.reward_cycle != reward_cycle {
                continue;
            }
            messages.push(message);
        }
        Ok(messages)
    }

    /// Get the ordered DKG packets from stackerdb for the signer slot IDs.
    pub fn get_dkg_packets(
        &mut self,
        signer_ids: &[SignerSlotID],
    ) -> Result<Vec<Packet>, ClientError> {
        let packet_slots = &[
            MessageSlotID::DkgBegin,
            MessageSlotID::DkgPublicShares,
            MessageSlotID::DkgPrivateBegin,
            MessageSlotID::DkgPrivateShares,
            MessageSlotID::DkgEndBegin,
            MessageSlotID::DkgEnd,
        ];
        let slot_ids = signer_ids.iter().map(|id| id.0).collect::<Vec<_>>();
        let mut packets = vec![];
        for packet_slot in packet_slots {
            let session = self
                .signers_message_stackerdb_sessions
                .get_mut(packet_slot)
                .ok_or(ClientError::NotConnected)?;
            let signer_messages = Self::get_messages(session, &slot_ids, self.reward_cycle)?;
            for signer_message in signer_messages {
                let StackerDBMessage::Packet(packet) = signer_message.message else {
                    warn!("Found an unexpected type in a packet slot {packet_slot}");
                    continue;
                };
                packets.push(packet);
            }
        }
        Ok(packets)
    }

    /// Get the transactions from stackerdb for the signers and the given reward cycle
    fn get_transactions(
        transactions_session: &mut StackerDBSession,
        signer_ids: &[SignerSlotID],
        reward_cycle: u64,
    ) -> Result<Vec<StacksTransaction>, ClientError> {
        let slot_ids = signer_ids.iter().map(|id| id.0).collect::<Vec<_>>();
        let signer_messages = Self::get_messages(transactions_session, &slot_ids, reward_cycle)?;
        let mut transactions = vec![];
        for signer_message in signer_messages {
            let StackerDBMessage::Transactions(chunk_transactions) = signer_message.message else {
                warn!("Signer wrote an unexpected type to the transactions slot");
                continue;
            };

            transactions.extend(chunk_transactions);
        }
        Ok(transactions)
    }

    /// Get this signer's latest transactions from stackerdb
    pub fn get_current_transactions(&mut self) -> Result<Vec<StacksTransaction>, ClientError> {
        let Some(transactions_session) = self
            .signers_message_stackerdb_sessions
            .get_mut(&MessageSlotID::Transactions)
        else {
            return Err(ClientError::NotConnected);
        };
        Self::get_transactions(
            transactions_session,
            &[self.signer_slot_id],
            self.reward_cycle,
        )
    }

    /// Get the latest signer transactions from signer ids for the next reward cycle
    pub fn get_next_transactions(
        &mut self,
        signer_ids: &[SignerSlotID],
    ) -> Result<Vec<StacksTransaction>, ClientError> {
        debug!("Getting latest chunks from stackerdb for the following signers: {signer_ids:?}",);
        Self::get_transactions(
            &mut self.next_transaction_session,
            signer_ids,
            self.reward_cycle.wrapping_add(1),
        )
    }

    /// Get the encrypted state for the given signer
    pub fn get_encrypted_signer_state(
        &mut self,
        signer_id: SignerSlotID,
    ) -> Result<Option<Vec<u8>>, ClientError> {
        debug!("Getting the persisted encrypted state for signer {signer_id}");
        let Some(state_session) = self
            .signers_message_stackerdb_sessions
            .get_mut(&MessageSlotID::EncryptedSignerState)
        else {
            return Err(ClientError::NotConnected);
        };

        let send_request = || {
            state_session
                .get_latest_chunks(&[signer_id.0])
                .map_err(backoff::Error::transient)
        };

        let Some(chunk) = retry_with_exponential_backoff(send_request)?.pop().ok_or(
            ClientError::UnexpectedResponseFormat(format!(
                "Missing response for state session request for signer {}",
                signer_id
            )),
        )?
        else {
            debug!("No persisted state for signer {signer_id}");
            return Ok(None);
        };

        if chunk.is_empty() {
            debug!("Empty persisted state for signer {signer_id}");
            return Ok(None);
        }

        let StackerDBMessage::EncryptedSignerState(state) =
            read_next::<StackerDBMessage, _>(&mut chunk.as_slice())?
        else {
            error!("Wrong message type stored in signer state slot for signer {signer_id}");
            return Ok(None);
        };

        Ok(Some(state))
    }

    /// Retrieve the signer set this stackerdb client is attached to
    pub fn get_signer_set(&self) -> u32 {
        u32::try_from(self.reward_cycle % 2).expect("FATAL: reward cycle % 2 exceeds u32::MAX")
    }

    /// Retrieve the signer slot ID
    pub fn get_signer_slot_id(&mut self) -> SignerSlotID {
        self.signer_slot_id
    }
}

#[cfg(test)]
mod tests {
    use std::thread::spawn;
    use std::time::Duration;

    use blockstack_lib::chainstate::stacks::{
        TransactionAnchorMode, TransactionAuth, TransactionPayload, TransactionPostConditionMode,
        TransactionSmartContract, TransactionVersion,
    };
    use blockstack_lib::util_lib::strings::StacksString;

    use super::*;
    use crate::client::tests::{generate_signer_config, mock_server_from_config, write_response};
    use crate::config::GlobalConfig;

    #[test]
    fn get_signer_transactions_should_succeed() {
        let config = GlobalConfig::load_from_file("./src/tests/conf/signer-0.toml").unwrap();
        let signer_config = generate_signer_config(&config, 5, 20);
        let mut stackerdb = StackerDB::from(&signer_config);
        let sk = StacksPrivateKey::new();
        let tx = StacksTransaction {
            version: TransactionVersion::Testnet,
            chain_id: 0,
            auth: TransactionAuth::from_p2pkh(&sk).unwrap(),
            anchor_mode: TransactionAnchorMode::Any,
            post_condition_mode: TransactionPostConditionMode::Allow,
            post_conditions: vec![],
            payload: TransactionPayload::SmartContract(
                TransactionSmartContract {
                    name: "test-contract".into(),
                    code_body: StacksString::from_str("(/ 1 0)").unwrap(),
                },
                None,
            ),
        };

        let reward_cycle = stackerdb.reward_cycle;
        let signer_message = SignerMessage {
            reward_cycle: reward_cycle.wrapping_add(1),
            message: vec![tx.clone()].into(),
        };
        let message = signer_message.serialize_to_vec();

        let signer_slot_ids = vec![SignerSlotID(0), SignerSlotID(1)];
        let h = spawn(move || stackerdb.get_next_transactions(&signer_slot_ids));
        let mut response_bytes = b"HTTP/1.1 200 OK\n\n".to_vec();
        response_bytes.extend(message);
        let mock_server = mock_server_from_config(&config);
        write_response(mock_server, response_bytes.as_slice());

        let signer_message = SignerMessage {
            reward_cycle,
            message: vec![].into(),
        };
        let message = signer_message.serialize_to_vec();
        let mut response_bytes = b"HTTP/1.1 200 OK\n\n".to_vec();
        response_bytes.extend(message);
        let mock_server = mock_server_from_config(&config);
        write_response(mock_server, response_bytes.as_slice());

        let transactions = h.join().unwrap().unwrap();
        assert_eq!(transactions, vec![tx]);
    }

    #[test]
    fn send_signer_message_should_succeed() {
        let config = GlobalConfig::load_from_file("./src/tests/conf/signer-1.toml").unwrap();
        let signer_config = generate_signer_config(&config, 5, 20);
        let mut stackerdb = StackerDB::from(&signer_config);

        let sk = StacksPrivateKey::new();
        let tx = StacksTransaction {
            version: TransactionVersion::Testnet,
            chain_id: 0,
            auth: TransactionAuth::from_p2pkh(&sk).unwrap(),
            anchor_mode: TransactionAnchorMode::Any,
            post_condition_mode: TransactionPostConditionMode::Allow,
            post_conditions: vec![],
            payload: TransactionPayload::SmartContract(
                TransactionSmartContract {
                    name: "test-contract".into(),
                    code_body: StacksString::from_str("(/ 1 0)").unwrap(),
                },
                None,
            ),
        };

        let ack = StackerDBChunkAckData {
            accepted: true,
            reason: None,
            metadata: None,
            code: None,
        };
        let mock_server = mock_server_from_config(&config);
        let h = spawn(move || stackerdb.send_message_with_retry(vec![tx].into()));
        let mut response_bytes = b"HTTP/1.1 200 OK\n\n".to_vec();
        let payload = serde_json::to_string(&ack).expect("Failed to serialize ack");
        response_bytes.extend(payload.as_bytes());
        std::thread::sleep(Duration::from_millis(500));
        write_response(mock_server, response_bytes.as_slice());
        assert_eq!(ack, h.join().unwrap().unwrap());
    }
}
