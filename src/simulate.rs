use std::collections::{BTreeMap};
use std::sync::Arc;

use pallet_staking::ValidatorPrefs;
use serde::Deserialize;
use sp_core::{crypto::Ss58Codec, Get, H256};
use sp_npos_elections::Support;
use pallet_election_provider_multi_block::{unsigned::miner::{BaseMiner, MineInput}};
use pallet_election_provider_multi_block::unsigned::miner::MinerConfig;
use futures::future::join_all;
use sp_runtime::Perbill;
use tracing::info;
use frame_support::BoundedVec;
use mockall::automock;
use crate::{miner_config, models::StakingStats, multi_block_state_client::{MultiBlockClientTrait, StorageTrait, VoterData, VoterSnapshotPage}, primitives::Storage, snapshot::SnapshotService};

use crate::{models::{Validator, ValidatorNomination, SimulationResult, RunParameters}, multi_block_state_client::ChainClientTrait, primitives::AccountId};

#[derive(Debug, Deserialize, Clone)]
pub struct Override {
    pub voters: Vec<(String, u64, Vec<String>)>,
    pub voters_remove: Vec<String>,
    pub candidates: Vec<String>,
    pub candidates_remove: Vec<String>,
}

// Service trait - application port for handlers
#[automock]
#[async_trait::async_trait]
pub trait SimulateService: Send + Sync {
    async fn simulate(
        &self,
        block: Option<H256>,
        desired_validators: Option<u32>,    
        apply_reduce: bool,
        manual_override: Option<Override>,
        min_nominator_bond: Option<u128>,   
        min_validator_bond: Option<u128>,
    ) -> Result<SimulationResult, Box<dyn std::error::Error + Send + Sync>>;
}

pub struct SimulateServiceImpl<
    CC: ChainClientTrait + Send + Sync + 'static,
    S: StorageTrait + From<Storage> + Clone + 'static,
    MC: MinerConfig + Send + Sync + 'static,
    MBC: MultiBlockClientTrait<CC, MC, S> + Send + Sync + 'static,
    Snap: SnapshotService<MC, S> + Send + Sync + 'static,
> {
    multi_block_state_client: Arc<MBC>,
    snapshot_service: Arc<Snap>,
    _phantom: std::marker::PhantomData<(CC, S, MC)>,
}

impl<
    CC: ChainClientTrait + Send + Sync + 'static,
    S: StorageTrait + From<Storage> + Clone + 'static,
    MC: MinerConfig + Send + Sync + Clone + 'static,
    MBC: MultiBlockClientTrait<CC, MC, S> + Send + Sync + 'static,
    Snap: SnapshotService<MC, S> + Send + Sync + 'static,
> SimulateServiceImpl<CC, S, MC, MBC, Snap> {
    pub fn new(multi_block_state_client: Arc<MBC>, snapshot_service: Arc<Snap>) -> Self {
        Self {
            multi_block_state_client,
            snapshot_service,
            _phantom: std::marker::PhantomData,
        }
    }
}

#[async_trait::async_trait]
impl<
    CC: ChainClientTrait + Send + Sync + 'static,
    S: StorageTrait + From<Storage> + Clone + 'static,
    MC: MinerConfig + Send + Sync + 'static,
    MBC: MultiBlockClientTrait<CC, MC, S> + Send + Sync + 'static,
    Snap: SnapshotService<MC, S> + Send + Sync + 'static,
> SimulateService for SimulateServiceImpl<CC, S, MC, MBC, Snap>
where
    MC: MinerConfig<AccountId = AccountId> + Send,
    MC::TargetSnapshotPerBlock: Send,
    MC::VoterSnapshotPerBlock: Send,
    MC::Pages: Send,
    MC::MaxVotesPerVoter: Send,
    MC::Solution: Send,
    MC::MaxBackersPerWinner: Send,
    MC::MaxWinnersPerPage: Send,
{
    async fn simulate(
        &self,
        block: Option<H256>,
        desired_validators: Option<u32>,
        apply_reduce: bool,
        manual_override: Option<Override>,
        min_nominator_bond: Option<u128>,
        min_validator_bond: Option<u128>,
    ) -> Result<SimulationResult, Box<dyn std::error::Error + Send + Sync>> {
        let multi_block_state_client = self.multi_block_state_client.as_ref();
        let block_details = multi_block_state_client.get_block_details(block).await?;
        let phase = multi_block_state_client.get_phase(&block_details.storage).await?;
        info!("Phase: {:?}", phase);
        let balancing_iter = miner_config::BalancingIterations::get();
        let algorithm = miner_config::get_current_algorithm();
        let max_nominations = miner_config::MaxVotesPerVoter::get();
        let run_parameters = RunParameters {
            algorithm: algorithm,
            iterations: balancing_iter.unwrap_or(sp_npos_elections::BalancingConfig { iterations: 0, tolerance: 0 }).iterations,
            reduce: apply_reduce,
            max_nominations: max_nominations,
            min_nominator_bond: min_nominator_bond.unwrap_or(0),
            min_validator_bond: min_validator_bond.unwrap_or(0),
            desired_validators: desired_validators.unwrap_or(block_details.desired_targets),
        };        

        info!("Fetching snapshot data for election...");
        let (mut snapshot, staking_config) = self.snapshot_service.get_snapshot_data_from_multi_block(&block_details).await?;

        // Apply min_nominator_bond filter if provided > 0
        let effective_min_nominator_bond = min_nominator_bond.unwrap_or(0);
        if effective_min_nominator_bond > 0 {
            info!("Filtering voters by min_nominator_bond: {}", effective_min_nominator_bond);
            let mut filtered_voter_pages = Vec::new();
            for voter_page in snapshot.voters.iter() {
                let filtered_page: Vec<_> = voter_page.iter()
                    .filter(|voter| voter.1 as u128 >= effective_min_nominator_bond)
                    .cloned()
                    .collect();
                if !filtered_page.is_empty() {
                    let bounded_page = BoundedVec::try_from(filtered_page)
                        .map_err(|_| "Failed to create bounded voter page")?;
                    filtered_voter_pages.push(bounded_page);
                }
            }
            snapshot.voters = filtered_voter_pages.try_into()
                .map_err(|_| "Failed to create AllVoterPagesOf")?;
        }
        
        // Apply min_validator_bond filter if provided > 0
        let effective_min_validator_bond = min_validator_bond.unwrap_or(0);
        if effective_min_validator_bond > 0 {
            info!("Filtering validators by min_validator_bond: {}", effective_min_validator_bond);
            let validator_futures: Vec<_> = snapshot.targets.iter().map(|validator| {
                let validator = validator.clone();
                let storage = block_details.storage.clone();
                async move {
                    let controller = multi_block_state_client.get_controller_from_stash(&storage, validator.clone()).await
                        .map_err(|e| format!("Error getting controller: {}", e))?;
                    if controller.is_none() {
                        return Ok::<Option<AccountId>, String>(None);
                    }
                    let controller = controller.unwrap();
                    let ledger = multi_block_state_client.ledger(&storage, controller).await
                        .map_err(|e| format!("Error getting ledger: {}", e))?;
                    let has_sufficient_bond = ledger.map_or(false, |l| l.active >= effective_min_validator_bond);
                    Ok(has_sufficient_bond.then_some(validator))
                }
            }).collect();
            
            let filtered_validators: Vec<_> = join_all(validator_futures)
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("Error filtering validators: {}", e))?;
            
            let filtered_validators: Vec<_> = filtered_validators.into_iter().filter_map(|x| x).collect();
            snapshot.targets = BoundedVec::try_from(filtered_validators)
                .map_err(|_| "Failed to create bounded target page")?;
        }
        
        // Manual override
        if let Some(manual) = manual_override {
            // Convert targets to Vec for manipulation
            let mut targets: Vec<AccountId> = snapshot.targets.iter().cloned().collect();

            // Add any additional candidates
            for c in &manual.candidates {
                let candidate_id: AccountId = AccountId::from_ss58check(c)?;
                if targets.contains(&candidate_id) {
                    info!("manual override: {:?} is already a candidate.", c);
                } else {
                    info!("manual override: {:?} is added as candidate.", c);
                    targets.push(candidate_id);
                }
            }

            // Remove candidates in the removal list
            for c in &manual.candidates_remove {
                let candidate_id: AccountId = AccountId::from_ss58check(c)?;
                if targets.contains(&candidate_id) {
                    info!("manual override: {:?} is removed as candidate.", c);
                    targets.retain(|x| x != &candidate_id);
                }
            }

            // Convert back to BoundedVec
            snapshot.targets = BoundedVec::try_from(targets)
                .map_err(|_| "Failed to create bounded target page")?;

            // Collect all voters from pages into a flat Vec for manipulation
            let mut all_voters: Vec<VoterData<MC>> = Vec::new();
            for voter_page in snapshot.voters.iter() {
                for voter in voter_page.iter() {
                    all_voters.push(voter.clone());
                }
            }

            // Add or override voters
            for v in &manual.voters {
                let voter_id: AccountId = AccountId::from_ss58check(&v.0)?;
                let stake = v.1;
                let votes: Vec<AccountId> = v.2.iter()
                    .map(|vote| AccountId::from_ss58check(vote).map(|id| id.into()))
                    .collect::<Result<_, _>>()?;
                let bounded_votes = BoundedVec::try_from(votes)
                    .map_err(|_| "Too many nominations")?;

                let voter_data: VoterData<MC> = (voter_id.clone(), stake, bounded_votes);
                if let Some(existing_voter) = all_voters.iter_mut().find(|vv| vv.0 == voter_data.0) {
                    info!("manual override: {:?} is already a voter. Overriding votes.", v.0);
                    *existing_voter = voter_data;
                } else {
                    info!("manual override: {:?} is added as voter.", v.0);
                    all_voters.push(voter_data);
                }
            }

            // Remove voters in the removal list
            for v in &manual.voters_remove {
                let voter_id: AccountId = AccountId::from_ss58check(v)?;
                if all_voters.iter().any(|vv| vv.0 == voter_id) {
                    info!("manual override: {:?} is removed as voter.", v);
                    all_voters.retain(|vv| vv.0 != voter_id);
                }
            }

            // Repage voters back into AllVoterPagesOf
            let voters_vec: Vec<BoundedVec<VoterData<MC>, MC::VoterSnapshotPerBlock>> = all_voters
                .chunks(MC::VoterSnapshotPerBlock::get() as usize)
                .map(|chunk| BoundedVec::try_from(chunk.to_vec()).map_err(|_| "Too many voters in chunk"))
                .collect::<Result<Vec<_>, _>>()?;
            snapshot.voters = voters_vec.try_into()
                .map_err(|_| "Failed to create AllVoterPagesOf")?;
        }

        let desired_targets = if let Some(desired_validators) = desired_validators {
            desired_validators
        } else {
            staking_config.desired_validators
        };

        let voter_pages: BoundedVec<VoterSnapshotPage<MC>, MC::Pages> = BoundedVec::truncate_from(snapshot.voters);

        // Use actual voter pages for mining solution when snapshot is not available and is created from staking
        let actual_voter_pages = voter_pages.len() as u32;
        
        let mine_input = MineInput {
            desired_targets: desired_targets,
            all_targets: snapshot.targets.clone(),
            voter_pages: voter_pages.clone(),
            pages: actual_voter_pages,
            do_reduce: apply_reduce,
            round: block_details.round,
        };
        info!("Mining solution for election...");

        let (paged_solution, winners_sorted_by_slot) = BaseMiner::<MC>::mine_solution(mine_input).map_err(|e| format!("Error mining solution: {:?}", e))?;
        
        // Convert each solution page to supports and combine them
        let mut total_supports: BTreeMap<AccountId, Support<AccountId>> = BTreeMap::new();

        let paged_supports = BaseMiner::<MC>::check_feasibility(
            &paged_solution, &voter_pages, &snapshot.targets, desired_targets)
            .map_err(|e| format!("Error checking feasibility: {:?}", e))?;

        for page in paged_supports.iter() {
            for (winner, support) in page.iter() {
                let entry = total_supports.entry(winner.clone()).or_insert_with(|| Support {
                    total: 0,
                    voters: Vec::new(),
                });
                entry.total = entry.total.saturating_add(support.total);
                entry.voters.extend(support.voters.clone().into_iter());
            }
        }

        let winner_stashes_sorted = winners_sorted_by_slot.into_iter().map(|e| e.0).collect::<Vec<_>>();

        let validator_futures: Vec<_> = total_supports.into_iter().map(|(winner, support)| {
            let winners_sorted = winner_stashes_sorted.clone();
            let storage = block_details.storage.clone();
            async move {
                let validator_prefs = multi_block_state_client.get_validator_prefs(&storage, winner.clone()).await
                    .unwrap_or(ValidatorPrefs {
                        commission: Perbill::from_parts(0),
                        blocked: false,
                    });

                let self_stake = support.voters.iter()
                    .find(|voter| voter.0 == winner)
                    .unwrap_or(&(winner.clone(), 0))
                    .1;
                
                let nominations: Vec<ValidatorNomination> = support.voters.iter()
                    .filter(|voter| voter.0 != winner)
                    .map(|voter| {
                    ValidatorNomination {
                        nominator: voter.0.to_ss58check(),
                        stake: voter.1 as u128,
                    }
                }).collect();

                let slot = winners_sorted.clone().iter().enumerate().find_map(|(i, s)| {
                    if s == &winner {
                        return Some(i as u32);
                    }
                    None
                }).expect("Validator needs to be in winners; qed");

                Ok::<Validator, String>(Validator {
                    stash: winner.to_ss58check(),
                    slot: slot,
                    self_stake: self_stake as u128,
                    total_stake: support.total as u128,
                    commission: validator_prefs.commission.deconstruct() as f64 / 1_000_000_000.0,
                    blocked: validator_prefs.blocked,
                    nominations_count: nominations.len(),
                    nominations: nominations,
                })
            }
        }).collect();
        
        let active_validators: Vec<Validator> = join_all(validator_futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;

        let total_staked = active_validators.iter().map(|v| v.total_stake).sum();
        let lowest_staked = active_validators.iter().map(|v| v.total_stake).min().unwrap_or(0);
        let avg_staked = total_staked / active_validators.len() as u128;

        let simulation_result = crate::models::SimulationResult {
            run_parameters: run_parameters.clone(),
            active_validators,
            staking_stats: StakingStats {
                total_staked: total_staked,
                lowest_staked: lowest_staked,
                avg_staked: avg_staked,
            },
        };

        Ok(simulation_result)
    }
}

#[cfg(target_os = "linux")]
#[cfg(test)]
mod tests {
    use super::*;
    use mockall::{mock};
    use mockall::predicate::*;
    use subxt::utils::Yes;
    use subxt::storage::Address;
    use crate::miner_config::polkadot::MinerConfig as PolkadotMinerConfig;
    use crate::multi_block_state_client::{BlockDetails, ElectionSnapshotPage, MockMultiBlockClientTrait};
    use crate::multi_block_state_client::MockChainClientTrait;
    use crate::raw_state_client::StakingLedger;
    use crate::models::StakingConfig;
    use crate::snapshot::MockSnapshotService;
    use crate::primitives::Hash;
    use crate::multi_block_state_client::Phase;
    use crate::miner_config::initialize_runtime_constants;

    mock! {
        pub DummyStorage {}
        
        #[async_trait::async_trait]
        impl StorageTrait for DummyStorage {
            async fn fetch<Addr>(
                &self,
                address: &Addr,
            ) -> Result<Option<<Addr as Address>::Target>, Box<dyn std::error::Error + Send + Sync>>
            where
                Addr: Address<IsFetchable = Yes> + Sync + 'static;

            async fn fetch_or_default<Addr>(
                &self,
                address: &Addr,
            ) -> Result<<Addr as Address>::Target, Box<dyn std::error::Error + Send + Sync>>
            where
                Addr: Address<IsFetchable = Yes, IsDefaultable = Yes> + Sync + 'static;
        }
    }

    // Implement Clone for MockDummyStorage
    impl Clone for MockDummyStorage {
        fn clone(&self) -> Self {
            MockDummyStorage::new()
        }
    }

    // Implement From<Storage> for DummyStorage to satisfy the trait bound in tests
    // This allows MockDummyStorage to be used with get_storage/get_block_details
    impl From<crate::primitives::Storage> for MockDummyStorage {
        fn from(_storage: crate::primitives::Storage) -> Self {
            MockDummyStorage::new()
        }
    }

    #[tokio::test]
    async fn test_simulate() {
        initialize_runtime_constants();
        type MockMBC = MockMultiBlockClientTrait<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage>;
        type MockBD = BlockDetails<MockDummyStorage>;
        
        let mut mock_client = MockMBC::new();
        let block_details = MockBD {
            block_hash: Some(Hash::zero()),
            phase: Phase::Snapshot(0),
            round: 1,
            n_pages: 1,
            desired_targets: 10,
            storage: MockDummyStorage::new(),
            _block_number: 100,
        };

        mock_client.expect_get_phase()
            .returning(|_storage: &MockDummyStorage| Ok(Phase::Snapshot(0)));
        
        let block_details_clone = block_details.clone();
        mock_client.expect_get_block_details()
            .with(eq(None))
            .returning(move |_block: Option<H256>| Ok(block_details_clone.clone()));

        mock_client
            .expect_get_validator_prefs()
            .returning(|_storage: &MockDummyStorage, _validator: AccountId| Ok(ValidatorPrefs {
                commission: Perbill::from_parts(0),
                blocked: false,
            }));

        let mut snapshot_service = MockSnapshotService::new();
        snapshot_service.expect_get_snapshot_data_from_multi_block().returning(move |_block_details: &BlockDetails<MockDummyStorage>| {
            Ok((ElectionSnapshotPage::<PolkadotMinerConfig> {
                voters: vec![BoundedVec::try_from(vec![(
                    AccountId::from_ss58check("5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty").unwrap(),
                    100,
                    BoundedVec::try_from(vec![AccountId::from_ss58check("5DLAjiZbVGBG1w5xNTaPuHXXVpvzEqWFhw4kwWt7YcNQnKQ2").unwrap()]).unwrap()
                )]).unwrap()],
                targets: BoundedVec::try_from(vec![AccountId::from_ss58check("5DLAjiZbVGBG1w5xNTaPuHXXVpvzEqWFhw4kwWt7YcNQnKQ2").unwrap()]).unwrap()
            }, StakingConfig {
                desired_validators: 10,
                max_nominations: 16,
                min_nominator_bond: 0,
                min_validator_bond: 0,
            }))
        });
        let simulate_service = SimulateServiceImpl::new(Arc::new(mock_client), Arc::new(snapshot_service));
        let result = simulate_service.simulate(None, None, false, None, None, None).await;
        assert!(result.is_ok());
        let simulation_result = result.unwrap();
        assert_eq!(simulation_result.active_validators, vec![Validator {
            stash: "5DLAjiZbVGBG1w5xNTaPuHXXVpvzEqWFhw4kwWt7YcNQnKQ2".to_string(),
            self_stake: 0,
            total_stake: 100,
            commission: 0.0,
            blocked: false,
            nominations_count: 1,
            nominations: vec![ValidatorNomination {
                nominator: "5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty".to_string(),
                stake: 100,
            }],
        }]);
    }

    #[tokio::test]
    async fn test_simulate_with_min_bonds() {
        initialize_runtime_constants();
        type MockMBC = MockMultiBlockClientTrait<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage>;
        type MockBD = BlockDetails<MockDummyStorage>;
        
        let mut mock_client = MockMBC::new();
        let block_details = MockBD {
            block_hash: Some(Hash::zero()),
            phase: Phase::Snapshot(0),
            round: 1,
            n_pages: 1,
            desired_targets: 10,
            storage: MockDummyStorage::new(),
            _block_number: 100,
        };

        mock_client.expect_get_phase()
            .returning(|_storage: &MockDummyStorage| Ok(Phase::Snapshot(0)));
        
        let block_details_clone = block_details.clone();
        mock_client.expect_get_block_details()
            .with(eq(None))
            .returning(move |_block: Option<H256>| Ok(block_details_clone.clone()));

        // Validator 1
        mock_client.expect_get_controller_from_stash()
            .returning(|_storage: &MockDummyStorage, _stash: AccountId| Ok(Some(AccountId::from_ss58check("5DLAjiZbVGBG1w5xNTaPuHXXVpvzEqWFhw4kwWt7YcNQnKQ2").unwrap())));

        mock_client.expect_ledger()
            .returning(|_storage: &MockDummyStorage, _account: AccountId| Ok(Some(StakingLedger {
                stash: AccountId::from_ss58check("5DLAjiZbVGBG1w5xNTaPuHXXVpvzEqWFhw4kwWt7YcNQnKQ2").unwrap(),
                total: 100,
                active: 100,
                unlocking: vec![],
            })));

        // Validator 2
        mock_client.expect_get_controller_from_stash()
            .returning(|_storage: &MockDummyStorage, _stash: AccountId| Ok(Some(AccountId::from_ss58check("3DLAjiZbVGBG1w5xNTaPuHXXVpvzEqWFhw4kwWt7YcNQnKQ2").unwrap())));

        mock_client.expect_ledger()
            .returning(|_storage: &MockDummyStorage, _account: AccountId| Ok(Some(StakingLedger {
                stash: AccountId::from_ss58check("3DLAjiZbVGBG1w5xNTaPuHXXVpvzEqWFhw4kwWt7YcNQnKQ2").unwrap(),
                total: 0,
                active: 0,
                unlocking: vec![],
            })));

        mock_client
            .expect_get_validator_prefs()
            .returning(|_storage: &MockDummyStorage, _validator: AccountId| Ok(ValidatorPrefs {
                commission: Perbill::from_parts(0),
                blocked: false,
            }));
       
        let mut snapshot_service = MockSnapshotService::new();
        snapshot_service.expect_get_snapshot_data_from_multi_block().returning(move |_block_details: &BlockDetails<MockDummyStorage>| {
            Ok((ElectionSnapshotPage::<PolkadotMinerConfig> {
                voters: vec![BoundedVec::try_from(vec![(
                    AccountId::from_ss58check("5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty").unwrap(),
                    100,
                    BoundedVec::try_from(vec![AccountId::from_ss58check("5DLAjiZbVGBG1w5xNTaPuHXXVpvzEqWFhw4kwWt7YcNQnKQ2").unwrap()]).unwrap()
                )]).unwrap()],
                targets: BoundedVec::try_from(vec![AccountId::from_ss58check("5DLAjiZbVGBG1w5xNTaPuHXXVpvzEqWFhw4kwWt7YcNQnKQ2").unwrap()]).unwrap()
            }, StakingConfig {
                desired_validators: 10,
                max_nominations: 16,
                min_nominator_bond: 100,
                min_validator_bond: 100,
            }))
        });
        let simulate_service = SimulateServiceImpl::new(Arc::new(mock_client), Arc::new(snapshot_service));
        let result = simulate_service.simulate(None, None, false, None, Some(100), Some(100)).await;
        assert!(result.is_ok());
        let simulation_result = result.unwrap();
        assert_eq!(simulation_result.active_validators, vec![Validator {
            stash: "5DLAjiZbVGBG1w5xNTaPuHXXVpvzEqWFhw4kwWt7YcNQnKQ2".to_string(),
            self_stake: 0,
            total_stake: 100,
            commission: 0.0,
            blocked: false,
            nominations_count: 1,
            nominations: vec![ValidatorNomination {
                nominator: "5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty".to_string(),
                stake: 100,
            }],
        }]);
    }

    #[tokio::test]
    async fn test_simulate_with_manual_override() {
        initialize_runtime_constants();
        type MockMBC = MockMultiBlockClientTrait<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage>;
        type MockBD = BlockDetails<MockDummyStorage>;
        
        let mut mock_client = MockMBC::new();
        let block_details = MockBD {
            block_hash: Some(Hash::zero()),
            phase: Phase::Snapshot(0),
            round: 1,
            n_pages: 1,
            desired_targets: 10,
            storage: MockDummyStorage::new(),
            _block_number: 100,
        };

        mock_client.expect_get_phase()
            .returning(|_storage: &MockDummyStorage| Ok(Phase::Snapshot(0)));
        
        let block_details_clone = block_details.clone();
        mock_client.expect_get_block_details()
            .with(eq(None))
            .returning(move |_block: Option<H256>| Ok(block_details_clone.clone()));

        mock_client
            .expect_get_validator_prefs()
            .returning(|_storage: &MockDummyStorage, _validator: AccountId| Ok(ValidatorPrefs {
                commission: Perbill::from_parts(0),
                blocked: false,
            }));

        let manual_override = Override {
            voters: vec![(
                "5GE5XFDHirGGeYNNUCwCBks1rsSWMomj2AqNyZVFsKVUqWZD".to_string(),
                100,
                vec!["5E9yWMxT1CoRPo7CxXQ4uLpHBmwzjFfJDV87dDMGxDo6WuMa".to_string()]
            )],
            voters_remove: vec!["5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty".to_string()],
            candidates: vec!["5E9yWMxT1CoRPo7CxXQ4uLpHBmwzjFfJDV87dDMGxDo6WuMa".to_string()],
            candidates_remove: vec!["5DLAjiZbVGBG1w5xNTaPuHXXVpvzEqWFhw4kwWt7YcNQnKQ2".to_string()],
        };

        let mut snapshot_service = MockSnapshotService::new();
        snapshot_service.expect_get_snapshot_data_from_multi_block().returning(move |_block_details: &BlockDetails<MockDummyStorage>| {
            Ok((ElectionSnapshotPage::<PolkadotMinerConfig> {
                voters: vec![BoundedVec::try_from(vec![(
                    AccountId::from_ss58check("5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty").unwrap(),
                    100,
                    BoundedVec::try_from(vec![AccountId::from_ss58check("5DLAjiZbVGBG1w5xNTaPuHXXVpvzEqWFhw4kwWt7YcNQnKQ2").unwrap()]).unwrap()
                )]).unwrap()],
                targets: BoundedVec::try_from(vec![AccountId::from_ss58check("5DLAjiZbVGBG1w5xNTaPuHXXVpvzEqWFhw4kwWt7YcNQnKQ2").unwrap()]).unwrap()
            }, StakingConfig {
                desired_validators: 10,
                max_nominations: 16,
                min_nominator_bond: 0,
                min_validator_bond: 0,
            }))
        });
        let simulate_service = SimulateServiceImpl::new(Arc::new(mock_client), Arc::new(snapshot_service));
        let result = simulate_service.simulate(None, None, false, Some(manual_override), None, None).await;
        assert!(result.is_ok());
        let simulation_result = result.unwrap();
        assert_eq!(simulation_result.active_validators, vec![Validator {
            stash: "5E9yWMxT1CoRPo7CxXQ4uLpHBmwzjFfJDV87dDMGxDo6WuMa".to_string(),
            self_stake: 0,
            total_stake: 100,
            commission: 0.0,
            blocked: false,
            nominations_count: 1,
            nominations: vec![ValidatorNomination {
                nominator: "5GE5XFDHirGGeYNNUCwCBks1rsSWMomj2AqNyZVFsKVUqWZD".to_string(),
                stake: 100,
            }],
        }]);
    }
}