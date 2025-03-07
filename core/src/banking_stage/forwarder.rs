use {
    super::{BankingStageStats, ForwardOption},
    crate::{
        forward_packet_batches_by_accounts::ForwardPacketBatchesByAccounts,
        leader_slot_banking_stage_metrics::LeaderSlotMetricsTracker,
        next_leader::{next_leader_tpu_forwards, next_leader_tpu_vote},
        tracer_packet_stats::TracerPacketStats,
        unprocessed_transaction_storage::UnprocessedTransactionStorage,
    },
    solana_client::connection_cache::ConnectionCache,
    solana_gossip::cluster_info::ClusterInfo,
    solana_measure::measure::Measure,
    solana_perf::{data_budget::DataBudget, packet::Packet},
    solana_poh::poh_recorder::PohRecorder,
    solana_runtime::bank_forks::BankForks,
    solana_sdk::{pubkey::Pubkey, transport::TransportError},
    solana_streamer::sendmmsg::batch_send,
    std::{
        iter::repeat,
        net::UdpSocket,
        sync::{atomic::Ordering, Arc, RwLock},
    },
};

pub struct Forwarder;

impl Forwarder {
    #[allow(clippy::too_many_arguments)]
    pub fn handle_forwarding(
        cluster_info: &ClusterInfo,
        unprocessed_transaction_storage: &mut UnprocessedTransactionStorage,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        socket: &UdpSocket,
        hold: bool,
        data_budget: &DataBudget,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
        banking_stage_stats: &BankingStageStats,
        connection_cache: &ConnectionCache,
        tracer_packet_stats: &mut TracerPacketStats,
        bank_forks: &Arc<RwLock<BankForks>>,
    ) {
        let forward_option = unprocessed_transaction_storage.forward_option();

        // get current root bank from bank_forks, use it to sanitize transaction and
        // load all accounts from address loader;
        let current_bank = bank_forks.read().unwrap().root_bank();

        let mut forward_packet_batches_by_accounts =
            ForwardPacketBatchesByAccounts::new_with_default_batch_limits();

        // sanitize and filter packets that are no longer valid (could be too old, a duplicate of something
        // already processed), then add to forwarding buffer.
        let filter_forwarding_result = unprocessed_transaction_storage
            .filter_forwardable_packets_and_add_batches(
                current_bank,
                &mut forward_packet_batches_by_accounts,
            );
        slot_metrics_tracker.increment_transactions_from_packets_us(
            filter_forwarding_result.total_packet_conversion_us,
        );
        banking_stage_stats.packet_conversion_elapsed.fetch_add(
            filter_forwarding_result.total_packet_conversion_us,
            Ordering::Relaxed,
        );
        banking_stage_stats
            .filter_pending_packets_elapsed
            .fetch_add(
                filter_forwarding_result.total_filter_packets_us,
                Ordering::Relaxed,
            );

        forward_packet_batches_by_accounts
            .iter_batches()
            .filter(|&batch| !batch.is_empty())
            .for_each(|forward_batch| {
                slot_metrics_tracker.increment_forwardable_batches_count(1);

                let batched_forwardable_packets_count = forward_batch.len();
                let (_forward_result, sucessful_forwarded_packets_count, leader_pubkey) =
                    Self::forward_buffered_packets(
                        connection_cache,
                        &forward_option,
                        cluster_info,
                        poh_recorder,
                        socket,
                        forward_batch.get_forwardable_packets(),
                        data_budget,
                        banking_stage_stats,
                    );

                if let Some(leader_pubkey) = leader_pubkey {
                    tracer_packet_stats.increment_total_forwardable_tracer_packets(
                        filter_forwarding_result.total_forwardable_tracer_packets,
                        leader_pubkey,
                    );
                }
                let failed_forwarded_packets_count = batched_forwardable_packets_count
                    .saturating_sub(sucessful_forwarded_packets_count);

                if failed_forwarded_packets_count > 0 {
                    slot_metrics_tracker.increment_failed_forwarded_packets_count(
                        failed_forwarded_packets_count as u64,
                    );
                    slot_metrics_tracker.increment_packet_batch_forward_failure_count(1);
                }

                if sucessful_forwarded_packets_count > 0 {
                    slot_metrics_tracker.increment_successful_forwarded_packets_count(
                        sucessful_forwarded_packets_count as u64,
                    );
                }
            });

        if !hold {
            slot_metrics_tracker.increment_cleared_from_buffer_after_forward_count(
                filter_forwarding_result.total_forwardable_packets as u64,
            );
            tracer_packet_stats.increment_total_cleared_from_buffer_after_forward(
                filter_forwarding_result.total_tracer_packets_in_buffer,
            );
            unprocessed_transaction_storage.clear_forwarded_packets();
        }
    }

    /// Forwards all valid, unprocessed packets in the buffer, up to a rate limit. Returns
    /// the number of successfully forwarded packets in second part of tuple
    fn forward_buffered_packets<'a>(
        connection_cache: &ConnectionCache,
        forward_option: &ForwardOption,
        cluster_info: &ClusterInfo,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        socket: &UdpSocket,
        forwardable_packets: impl Iterator<Item = &'a Packet>,
        data_budget: &DataBudget,
        banking_stage_stats: &BankingStageStats,
    ) -> (
        std::result::Result<(), TransportError>,
        usize,
        Option<Pubkey>,
    ) {
        let leader_and_addr = match forward_option {
            ForwardOption::NotForward => return (Ok(()), 0, None),
            ForwardOption::ForwardTransaction => {
                next_leader_tpu_forwards(cluster_info, poh_recorder)
            }

            ForwardOption::ForwardTpuVote => next_leader_tpu_vote(cluster_info, poh_recorder),
        };
        let (leader_pubkey, addr) = match leader_and_addr {
            Some(leader_and_addr) => leader_and_addr,
            None => return (Ok(()), 0, None),
        };

        const INTERVAL_MS: u64 = 100;
        // 12 MB outbound limit per second
        const MAX_BYTES_PER_SECOND: usize = 12_000_000;
        const MAX_BYTES_PER_INTERVAL: usize = MAX_BYTES_PER_SECOND * INTERVAL_MS as usize / 1000;
        const MAX_BYTES_BUDGET: usize = MAX_BYTES_PER_INTERVAL * 5;
        data_budget.update(INTERVAL_MS, |bytes| {
            std::cmp::min(
                bytes.saturating_add(MAX_BYTES_PER_INTERVAL),
                MAX_BYTES_BUDGET,
            )
        });

        let packet_vec: Vec<_> = forwardable_packets
            .filter_map(|p| {
                if !p.meta().forwarded() && data_budget.take(p.meta().size) {
                    Some(p.data(..)?.to_vec())
                } else {
                    None
                }
            })
            .collect();

        let packet_vec_len = packet_vec.len();
        // TODO: see https://github.com/solana-labs/solana/issues/23819
        // fix this so returns the correct number of succeeded packets
        // when there's an error sending the batch. This was left as-is for now
        // in favor of shipping Quic support, which was considered higher-priority
        if !packet_vec.is_empty() {
            inc_new_counter_info!("banking_stage-forwarded_packets", packet_vec_len);

            let mut measure = Measure::start("banking_stage-forward-us");

            let res = if let ForwardOption::ForwardTpuVote = forward_option {
                // The vote must be forwarded using only UDP.
                banking_stage_stats
                    .forwarded_vote_count
                    .fetch_add(packet_vec_len, Ordering::Relaxed);
                let pkts: Vec<_> = packet_vec.into_iter().zip(repeat(addr)).collect();
                batch_send(socket, &pkts).map_err(|err| err.into())
            } else {
                // All other transactions can be forwarded using QUIC, get_connection() will use
                // system wide setting to pick the correct connection object.
                banking_stage_stats
                    .forwarded_transaction_count
                    .fetch_add(packet_vec_len, Ordering::Relaxed);
                let conn = connection_cache.get_connection(&addr);
                conn.send_data_batch_async(packet_vec)
            };

            measure.stop();
            inc_new_counter_info!(
                "banking_stage-forward-us",
                measure.as_us() as usize,
                1000,
                1000
            );

            if let Err(err) = res {
                inc_new_counter_info!("banking_stage-forward_packets-failed-batches", 1);
                return (Err(err), 0, Some(leader_pubkey));
            }
        }

        (Ok(()), packet_vec_len, Some(leader_pubkey))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            banking_stage::tests::{create_slow_genesis_config_with_leader, new_test_cluster_info},
            unprocessed_packet_batches::{DeserializedPacket, UnprocessedPacketBatches},
            unprocessed_transaction_storage::ThreadType,
        },
        solana_ledger::{
            blockstore::Blockstore, genesis_utils::GenesisConfigInfo,
            get_tmp_ledger_path_auto_delete,
        },
        solana_perf::packet::PacketFlags,
        solana_poh::poh_recorder::create_test_recorder,
        solana_runtime::bank::Bank,
        solana_sdk::{
            hash::Hash, poh_config::PohConfig, signature::Keypair, signer::Signer,
            system_transaction, transaction::VersionedTransaction,
        },
        solana_streamer::recvmmsg::recv_mmsg,
    };

    #[test]
    #[ignore]
    fn test_forwarder_budget() {
        solana_logger::setup();
        // Create `PacketBatch` with 1 unprocessed packet
        let tx = system_transaction::transfer(
            &Keypair::new(),
            &solana_sdk::pubkey::new_rand(),
            1,
            Hash::new_unique(),
        );
        let packet = Packet::from_data(None, tx).unwrap();
        let deserialized_packet = DeserializedPacket::new(packet).unwrap();

        let validator_keypair = Arc::new(Keypair::new());
        let genesis_config_info =
            create_slow_genesis_config_with_leader(10_000, &validator_keypair.pubkey());
        let GenesisConfigInfo { genesis_config, .. } = &genesis_config_info;

        let bank = Bank::new_no_wallclock_throttle_for_tests(genesis_config);
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let bank = Arc::new(bank_forks.read().unwrap().get(0).unwrap());
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        {
            let blockstore = Arc::new(
                Blockstore::open(ledger_path.path())
                    .expect("Expected to be able to open database ledger"),
            );
            let poh_config = PohConfig {
                // limit tick count to avoid clearing working_bank at
                // PohRecord then PohRecorderError(MaxHeightReached) at BankingStage
                target_tick_count: Some(bank.max_tick_height() - 1),
                ..PohConfig::default()
            };

            let (exit, poh_recorder, poh_service, _entry_receiver) =
                create_test_recorder(&bank, &blockstore, Some(poh_config), None);

            let (local_node, cluster_info) = new_test_cluster_info(Some(validator_keypair));
            let recv_socket = &local_node.sockets.tpu_forwards[0];

            let test_cases = vec![
                ("budget-restricted", DataBudget::restricted(), 0),
                ("budget-available", DataBudget::default(), 1),
            ];

            let connection_cache = ConnectionCache::default();
            let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
            for (name, data_budget, expected_num_forwarded) in test_cases {
                let unprocessed_packet_batches: UnprocessedPacketBatches =
                    UnprocessedPacketBatches::from_iter(
                        vec![deserialized_packet.clone()].into_iter(),
                        1,
                    );
                let stats = BankingStageStats::default();
                Forwarder::handle_forwarding(
                    &cluster_info,
                    &mut UnprocessedTransactionStorage::new_transaction_storage(
                        unprocessed_packet_batches,
                        ThreadType::Transactions,
                    ),
                    &poh_recorder,
                    &socket,
                    true,
                    &data_budget,
                    &mut LeaderSlotMetricsTracker::new(0),
                    &stats,
                    &connection_cache,
                    &mut TracerPacketStats::new(0),
                    &bank_forks,
                );

                recv_socket
                    .set_nonblocking(expected_num_forwarded == 0)
                    .unwrap();

                let mut packets = vec![Packet::default(); 2];
                let num_received = recv_mmsg(recv_socket, &mut packets[..]).unwrap_or_default();
                assert_eq!(num_received, expected_num_forwarded, "{name}");
            }

            exit.store(true, Ordering::Relaxed);
            poh_service.join().unwrap();
        }
        Blockstore::destroy(ledger_path.path()).unwrap();
    }

    #[test]
    #[ignore]
    fn test_handle_forwarding() {
        solana_logger::setup();
        // packets are deserialized upon receiving, failed packets will not be
        // forwarded; Therefore need to create real packets here.
        let keypair = Keypair::new();
        let pubkey = solana_sdk::pubkey::new_rand();

        let fwd_block_hash = Hash::new_unique();
        let forwarded_packet = {
            let transaction = system_transaction::transfer(&keypair, &pubkey, 1, fwd_block_hash);
            let mut packet = Packet::from_data(None, transaction).unwrap();
            packet.meta_mut().flags |= PacketFlags::FORWARDED;
            DeserializedPacket::new(packet).unwrap()
        };

        let normal_block_hash = Hash::new_unique();
        let normal_packet = {
            let transaction = system_transaction::transfer(&keypair, &pubkey, 1, normal_block_hash);
            let packet = Packet::from_data(None, transaction).unwrap();
            DeserializedPacket::new(packet).unwrap()
        };

        let mut unprocessed_packet_batches = UnprocessedTransactionStorage::new_transaction_storage(
            UnprocessedPacketBatches::from_iter(
                vec![forwarded_packet, normal_packet].into_iter(),
                2,
            ),
            ThreadType::Transactions,
        );

        let validator_keypair = Arc::new(Keypair::new());
        let genesis_config_info =
            create_slow_genesis_config_with_leader(10_000, &validator_keypair.pubkey());
        let GenesisConfigInfo { genesis_config, .. } = &genesis_config_info;
        let bank = Bank::new_no_wallclock_throttle_for_tests(genesis_config);
        let bank_forks = Arc::new(RwLock::new(BankForks::new(bank)));
        let bank = Arc::new(bank_forks.read().unwrap().get(0).unwrap());
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        {
            let blockstore = Arc::new(
                Blockstore::open(ledger_path.path())
                    .expect("Expected to be able to open database ledger"),
            );
            let poh_config = PohConfig {
                // limit tick count to avoid clearing working_bank at
                // PohRecord then PohRecorderError(MaxHeightReached) at BankingStage
                target_tick_count: Some(bank.max_tick_height() - 1),
                ..PohConfig::default()
            };

            let (exit, poh_recorder, poh_service, _entry_receiver) =
                create_test_recorder(&bank, &blockstore, Some(poh_config), None);

            let (local_node, cluster_info) = new_test_cluster_info(Some(validator_keypair));
            let recv_socket = &local_node.sockets.tpu_forwards[0];
            let connection_cache = ConnectionCache::default();

            let test_cases = vec![
                ("fwd-normal", true, vec![normal_block_hash], 2),
                ("fwd-no-op", true, vec![], 2),
                ("fwd-no-hold", false, vec![], 0),
            ];

            let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
            for (name, hold, expected_ids, expected_num_unprocessed) in test_cases {
                let stats = BankingStageStats::default();
                Forwarder::handle_forwarding(
                    &cluster_info,
                    &mut unprocessed_packet_batches,
                    &poh_recorder,
                    &socket,
                    hold,
                    &DataBudget::default(),
                    &mut LeaderSlotMetricsTracker::new(0),
                    &stats,
                    &connection_cache,
                    &mut TracerPacketStats::new(0),
                    &bank_forks,
                );

                recv_socket
                    .set_nonblocking(expected_ids.is_empty())
                    .unwrap();

                let mut packets = vec![Packet::default(); 2];
                let num_received = recv_mmsg(recv_socket, &mut packets[..]).unwrap_or_default();
                assert_eq!(num_received, expected_ids.len(), "{name}");
                for (i, expected_id) in expected_ids.iter().enumerate() {
                    assert_eq!(packets[i].meta().size, 215);
                    let recv_transaction: VersionedTransaction =
                        packets[i].deserialize_slice(..).unwrap();
                    assert_eq!(
                        recv_transaction.message.recent_blockhash(),
                        expected_id,
                        "{name}"
                    );
                }

                let num_unprocessed_packets: usize = unprocessed_packet_batches.len();
                assert_eq!(num_unprocessed_packets, expected_num_unprocessed, "{name}");
            }

            exit.store(true, Ordering::Relaxed);
            poh_service.join().unwrap();
        }
        Blockstore::destroy(ledger_path.path()).unwrap();
    }
}
