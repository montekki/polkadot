// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use std::time::Duration;
use std::sync::Arc;

use futures::{executor, future};
use futures_timer::Delay;
use assert_matches::assert_matches;
use smallvec::smallvec;

use super::*;

use polkadot_primitives::v1::{
	AuthorityDiscoveryId, PersistedValidationData, PoV, BlockData, HeadData,
};
use polkadot_erasure_coding::{branches, obtain_chunks_v1 as obtain_chunks};
use polkadot_node_subsystem_util::TimeoutExt;
use polkadot_subsystem_testhelpers as test_helpers;
use polkadot_subsystem::messages::{RuntimeApiMessage, RuntimeApiRequest};

type VirtualOverseer = test_helpers::TestSubsystemContextHandle<AvailabilityRecoveryMessage>;

struct TestHarness {
	virtual_overseer: VirtualOverseer,
}

fn test_harness<T: Future<Output = ()>>(
	test: impl FnOnce(TestHarness) -> T,
) {
	let _ = env_logger::builder()
		.is_test(true)
		.filter(
			Some("polkadot_availability_recovery"),
			log::LevelFilter::Trace,
		)
		.try_init();

	let pool = sp_core::testing::TaskExecutor::new();

	let (context, virtual_overseer) = test_helpers::make_subsystem_context(pool.clone());

	let subsystem = AvailabilityRecoverySubsystem::new();
	let subsystem = subsystem.run(context);

	let test_fut = test(TestHarness { virtual_overseer });

	futures::pin_mut!(test_fut);
	futures::pin_mut!(subsystem);

	executor::block_on(future::select(test_fut, subsystem));
}

const TIMEOUT: Duration = Duration::from_millis(100);

macro_rules! delay {
	($delay:expr) => {
		Delay::new(Duration::from_millis($delay)).await;
	};
}

async fn overseer_signal(
	overseer: &mut test_helpers::TestSubsystemContextHandle<AvailabilityRecoveryMessage>,
	signal: OverseerSignal,
) {
	delay!(50);
	overseer
		.send(FromOverseer::Signal(signal))
		.timeout(TIMEOUT)
		.await
		.expect("10ms is more than enough for sending signals.");
}

async fn overseer_send(
	overseer: &mut test_helpers::TestSubsystemContextHandle<AvailabilityRecoveryMessage>,
	msg: AvailabilityRecoveryMessage,
) {
	tracing::trace!(msg = ?msg, "sending message");
	overseer
		.send(FromOverseer::Communication { msg })
		.timeout(TIMEOUT)
		.await
		.expect("10ms is more than enough for sending messages.");
}

async fn overseer_recv(
	overseer: &mut test_helpers::TestSubsystemContextHandle<AvailabilityRecoveryMessage>,
) -> AllMessages {
	tracing::trace!("waiting for message ...");
	let msg = overseer
		.recv()
		.timeout(TIMEOUT)
		.await
		.expect("TIMEOUT is enough to recv.");
	tracing::trace!(msg = ?msg, "received message");
	msg
}


use sp_keyring::Sr25519Keyring;

#[derive(Clone)]
struct TestState {
	validators: Vec<Sr25519Keyring>,
	validator_public: Vec<ValidatorId>,
	validator_authority_id: Vec<AuthorityDiscoveryId>,
	validator_peer_id: Vec<PeerId>,
	current: Hash,
	candidate: CandidateReceipt,
	session_index: SessionIndex,


	persisted_validation_data: PersistedValidationData,

	available_data: AvailableData,
	chunks: Vec<ErasureChunk>,
}

impl TestState {
	async fn test_runtime_api(
		&self,
		virtual_overseer: &mut VirtualOverseer,
	) {
		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				relay_parent,
				RuntimeApiRequest::SessionIndexForChild(tx),
			)) => {
				assert_eq!(relay_parent, self.current);
				tx.send(Ok(self.session_index)).unwrap();
			}
		);

		assert_matches!(
			overseer_recv(virtual_overseer).await,
			AllMessages::RuntimeApi(RuntimeApiMessage::Request(
				relay_parent,
				RuntimeApiRequest::SessionInfo(
					session_index,
					tx,
				)
			)) => {
				assert_eq!(relay_parent, self.current);
				assert_eq!(session_index, self.session_index);

				tx.send(Ok(Some(SessionInfo {
					validators: self.validator_public.clone(),
					discovery_keys: self.validator_authority_id.clone(),
					..Default::default()
				}))).unwrap();
			}
		);
	}

	async fn test_connect_to_validators(
		&self,
		virtual_overseer: &mut VirtualOverseer,
	) {
		// Indexes of validators subsystem has attempted to connect to.
		let mut attempted_to_connect_to = Vec::new();

		for _ in 0..self.validator_public.len() {
			self.test_runtime_api(virtual_overseer).await;

			// Connect to shuffled validators one by one.
			assert_matches!(
				overseer_recv(virtual_overseer).await,
				AllMessages::NetworkBridge(
					NetworkBridgeMessage::ConnectToValidators {
						validator_ids,
						mut connected,
						..
					}
				) => {
					for validator_id in validator_ids {
						let idx = self.validator_authority_id
							.iter()
							.position(|x| *x == validator_id)
							.unwrap();

						attempted_to_connect_to.push(idx);

						let result = (
							self.validator_authority_id[idx].clone(),
							self.validator_peer_id[idx].clone(),
						);

						connected.try_send(result).unwrap();
					}
				}
			);
		}
	}

	async fn test_chunk_requests(
		&self,
		candidate_hash: CandidateHash,
		virtual_overseer: &mut VirtualOverseer,
	) {
		for _ in 0..self.validator_public.len() {
			// Receive a request for a chunk.
			assert_matches!(
				overseer_recv(virtual_overseer).await,
				AllMessages::NetworkBridge(
					NetworkBridgeMessage::SendValidationMessage(
						_peers,
						protocol_v1::ValidationProtocol::AvailabilityRecovery(wire_message),
					)
				) => {
					let (request_id, validator_index) = assert_matches!(
						wire_message,
						protocol_v1::AvailabilityRecoveryMessage::RequestChunk(
							request_id,
							candidate_hash_recvd,
							validator_index,
						) => {
							assert_eq!(candidate_hash_recvd, candidate_hash);
							(request_id, validator_index)
						}
					);

					overseer_send(
						virtual_overseer,
						AvailabilityRecoveryMessage::NetworkBridgeUpdateV1(
							NetworkBridgeEvent::PeerMessage(
								self.validator_peer_id[validator_index as usize].clone(),
								protocol_v1::AvailabilityRecoveryMessage::Chunk(
									request_id,
									Some(self.chunks[validator_index as usize].clone()),
								)
							)
						)
					).await;
				}
			);
		}
	}
}

fn validator_pubkeys(val_ids: &[Sr25519Keyring]) -> Vec<ValidatorId> {
	val_ids.iter().map(|v| v.public().into()).collect()
}

fn validator_authority_id(val_ids: &[Sr25519Keyring]) -> Vec<AuthorityDiscoveryId> {
	val_ids.iter().map(|v| v.public().into()).collect()
}

fn derive_erasure_chunks_with_proofs_and_root(
	n_validators: usize,
	available_data: &AvailableData,
) -> (Vec<ErasureChunk>, Hash) {
	let chunks: Vec<Vec<u8>> = obtain_chunks(n_validators, available_data).unwrap();

	// create proofs for each erasure chunk
	let branches = branches(chunks.as_ref());

	let root = branches.root();
	let erasure_chunks = branches
		.enumerate()
		.map(|(index, (proof, chunk))| ErasureChunk {
			chunk: chunk.to_vec(),
			index: index as _,
			proof,
		})
		.collect::<Vec<ErasureChunk>>();

	(erasure_chunks, root)
}

impl Default for TestState {
	fn default() -> Self {
		let validators = vec![
			Sr25519Keyring::Ferdie, // <- this node, role: validator
			Sr25519Keyring::Alice,
			Sr25519Keyring::Bob,
			Sr25519Keyring::Charlie,
			Sr25519Keyring::Dave,
		];

		let validator_public = validator_pubkeys(&validators);
		let validator_authority_id = validator_authority_id(&validators);
		let validator_peer_id = std::iter::repeat_with(|| PeerId::random())
			.take(validator_public.len())
			.collect();

		let current = Hash::repeat_byte(1);

		let mut candidate = CandidateReceipt::default();

		let session_index = 10;

		let persisted_validation_data = PersistedValidationData {
			parent_head: HeadData(vec![7, 8, 9]),
			block_number: Default::default(),
			hrmp_mqc_heads: Vec::new(),
			dmq_mqc_head: Default::default(),
			max_pov_size: 1024,
		};

		let pov = PoV {
			block_data: BlockData(vec![42; 64]),
		};

		let available_data = AvailableData {
			validation_data: persisted_validation_data.clone(),
			pov: Arc::new(pov),
		};

		let (chunks, erasure_root) = derive_erasure_chunks_with_proofs_and_root(
			validators.len(),
			&available_data,
		);

		candidate.descriptor.erasure_root = erasure_root;

		Self {
			validators,
			validator_public,
			validator_authority_id,
			validator_peer_id,
			current,
			candidate,
			session_index,
			persisted_validation_data,
			available_data,
			chunks,
		}
	}
}

#[test]
fn availability_is_recovered() {
	let test_state = TestState::default();

	test_harness(|test_harness| async move {
		let TestHarness { mut virtual_overseer } = test_harness;

		overseer_signal(
			&mut virtual_overseer,
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate {
				activated: smallvec![test_state.current.clone()],
				deactivated: smallvec![],
			}),
		).await;

		let (tx, rx) = oneshot::channel();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				test_state.candidate.clone(),
				tx,
			)
		).await;

		test_state.test_runtime_api(&mut virtual_overseer).await;

		test_state.test_connect_to_validators(&mut virtual_overseer).await;

		let candidate_hash = test_state.candidate.hash();

		test_state.test_chunk_requests(candidate_hash, &mut virtual_overseer).await;

		// Recovered data should match the original one.
		assert_eq!(rx.await.unwrap().unwrap(), test_state.available_data);

		let (tx, rx) = oneshot::channel();

		// Test another candidate, send no chunks.
		let new_candidate = CandidateReceipt::default();

		overseer_send(
			&mut virtual_overseer,
			AvailabilityRecoveryMessage::RecoverAvailableData(
				new_candidate,
				tx,
			)
		).await;

		test_state.test_runtime_api(&mut virtual_overseer).await;

		test_state.test_connect_to_validators(&mut virtual_overseer).await;

		// A request times out with `Unavailable` error.
		assert_eq!(rx.await.unwrap().unwrap_err(), RecoveryError::Unavailable);
	});
}
