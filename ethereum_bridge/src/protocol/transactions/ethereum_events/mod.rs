//! Code for handling Ethereum events protocol txs.

mod eth_msgs;
mod events;

use std::collections::{BTreeSet, HashMap, HashSet};

use borsh::BorshDeserialize;
use eth_msgs::EthMsgUpdate;
use eyre::Result;
use namada_core::ledger::storage::traits::StorageHasher;
use namada_core::ledger::storage::{DBIter, WlStorage, DB};
use namada_core::ledger::storage_api::StorageRead;
use namada_core::types::address::Address;
use namada_core::types::ethereum_events::EthereumEvent;
use namada_core::types::storage::{BlockHeight, Epoch, Key};
use namada_core::types::transaction::TxResult;
use namada_core::types::vote_extensions::ethereum_events::MultiSignedEthEvent;
use namada_core::types::voting_power::FractionalVotingPower;
use namada_proof_of_stake::pos_queries::PosQueries;

use super::ChangedKeys;
use crate::protocol::transactions::utils;
use crate::protocol::transactions::votes::update::NewVotes;
use crate::protocol::transactions::votes::{self, calculate_new};
use crate::storage::vote_tallies::{self, Keys};

impl utils::GetVoters for HashSet<EthMsgUpdate> {
    #[inline]
    fn get_voters(
        &self,
        _epoch_start_height: BlockHeight,
    ) -> HashSet<(Address, BlockHeight)> {
        self.iter().fold(HashSet::new(), |mut voters, update| {
            voters.extend(update.seen_by.clone().into_iter());
            voters
        })
    }
}

/// Applies derived state changes to storage, based on Ethereum `events` which
/// were newly seen by some active validator(s). For `events`
/// which have been seen by enough voting power ( > 2/3 ), extra state changes
/// may take place, such as minting of wrapped ERC20s.
///
/// This function is deterministic based on some existing blockchain state and
/// the passed `events`.
pub fn apply_derived_tx<D, H>(
    wl_storage: &mut WlStorage<D, H>,
    events: Vec<MultiSignedEthEvent>,
) -> Result<TxResult>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    if events.is_empty() {
        return Ok(TxResult::default());
    }
    tracing::info!(
        ethereum_events = events.len(),
        "Applying state updates derived from Ethereum events found in \
         protocol transaction"
    );

    let updates = events.into_iter().map(Into::<EthMsgUpdate>::into).collect();

    let voting_powers = utils::get_voting_powers(wl_storage, &updates)?;

    let mut changed_keys = apply_updates(wl_storage, updates, voting_powers)?;

    changed_keys.extend(timeout_events(wl_storage)?);

    Ok(TxResult {
        changed_keys,
        ..Default::default()
    })
}

/// Apply votes to Ethereum events in storage and act on any events which are
/// confirmed.
///
/// The `voting_powers` map must contain a voting power for all
/// `(Address, BlockHeight)`s that occur in any of the `updates`.
pub(super) fn apply_updates<D, H>(
    wl_storage: &mut WlStorage<D, H>,
    updates: HashSet<EthMsgUpdate>,
    voting_powers: HashMap<(Address, BlockHeight), FractionalVotingPower>,
) -> Result<ChangedKeys>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    tracing::debug!(
        updates.len = updates.len(),
        ?voting_powers,
        "Applying Ethereum state update transaction"
    );

    let mut changed_keys = BTreeSet::default();
    let mut confirmed = vec![];
    for update in updates {
        // The order in which updates are applied to storage does not matter.
        // The final storage state will be the same regardless.
        let (mut changed, newly_confirmed) =
            apply_update(wl_storage, update.clone(), &voting_powers)?;
        changed_keys.append(&mut changed);
        if newly_confirmed {
            confirmed.push(update.body);
        }
    }
    if confirmed.is_empty() {
        tracing::debug!("No events were newly confirmed");
        return Ok(changed_keys);
    }
    tracing::debug!(n = confirmed.len(), "Events were newly confirmed",);

    // Right now, the order in which events are acted on does not matter.
    // For `TransfersToNamada` events, they can happen in any order.
    for event in &confirmed {
        let mut changed = events::act_on(wl_storage, event)?;
        changed_keys.append(&mut changed);
    }
    Ok(changed_keys)
}

/// Apply an [`EthMsgUpdate`] to storage. Returns any keys changed and whether
/// the event was newly seen.
///
/// The `voting_powers` map must contain a voting power for all
/// `(Address, BlockHeight)`s that occur in `update`.
fn apply_update<D, H>(
    wl_storage: &mut WlStorage<D, H>,
    update: EthMsgUpdate,
    voting_powers: &HashMap<(Address, BlockHeight), FractionalVotingPower>,
) -> Result<(ChangedKeys, bool)>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let eth_msg_keys = vote_tallies::Keys::from(&update.body);

    // we arbitrarily look at whether the seen key is present to
    // determine if the /eth_msg already exists in storage, but maybe there
    // is a less arbitrary way to do this
    let exists_in_storage = wl_storage.has_key(&eth_msg_keys.seen())?;

    let (vote_tracking, changed, confirmed, already_present) =
        if !exists_in_storage {
            tracing::debug!(%eth_msg_keys.prefix, "Ethereum event not seen before by any validator");
            let vote_tracking = calculate_new(update.seen_by, voting_powers)?;
            let changed = eth_msg_keys.into_iter().collect();
            let confirmed = vote_tracking.seen;
            (vote_tracking, changed, confirmed, false)
        } else {
            tracing::debug!(
                %eth_msg_keys.prefix,
                "Ethereum event already exists in storage",
            );
            let new_votes =
                NewVotes::new(update.seen_by.clone(), voting_powers)?;
            let (vote_tracking, changed) =
                votes::update::calculate(wl_storage, &eth_msg_keys, new_votes)?;
            if changed.is_empty() {
                return Ok((changed, false));
            }
            let confirmed =
                vote_tracking.seen && changed.contains(&eth_msg_keys.seen());
            (vote_tracking, changed, confirmed, true)
        };

    votes::storage::write(
        wl_storage,
        &eth_msg_keys,
        &update.body,
        &vote_tracking,
        already_present,
    )?;

    Ok((changed, confirmed))
}

fn timeout_events<D, H>(wl_storage: &mut WlStorage<D, H>) -> Result<ChangedKeys>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let mut changed = ChangedKeys::new();
    for keys in get_timed_out_eth_events(wl_storage)? {
        tracing::debug!(
            %keys.prefix,
            "Ethereum event timed out",
        );
        votes::storage::delete(wl_storage, &keys)?;
        changed.extend(keys.clone().into_iter());
    }

    Ok(changed)
}

fn get_timed_out_eth_events<D, H>(
    wl_storage: &mut WlStorage<D, H>,
) -> Result<Vec<Keys<EthereumEvent>>>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    let unbonding_len = wl_storage.pos_queries().get_pos_params().unbonding_len;
    let current_epoch = wl_storage.storage.last_epoch;
    if current_epoch.0 <= unbonding_len {
        return Ok(Vec::new());
    }

    let timeout_epoch = Epoch(current_epoch.0 - unbonding_len);
    let prefix = vote_tallies::eth_msgs_prefix();
    let mut cur_keys: Option<Keys<EthereumEvent>> = None;
    let mut is_timed_out = false;
    let mut is_seen = false;
    let mut results = Vec::new();
    for (key, val, _) in votes::storage::iter_prefix(wl_storage, &prefix)? {
        let key = Key::parse(key).expect("The key should be parsable");
        if let Some(keys) = vote_tallies::eth_event_keys(&key) {
            match &cur_keys {
                Some(prev_keys) => {
                    if *prev_keys != keys {
                        // check the previous keys since we found new keys
                        if is_timed_out && !is_seen {
                            results.push(prev_keys.clone());
                        }
                        is_timed_out = false;
                        is_seen = false;
                        cur_keys = Some(keys);
                    }
                }
                None => cur_keys = Some(keys),
            }

            if vote_tallies::is_epoch_key(&key) {
                let inserted_epoch = Epoch::try_from_slice(&val[..])
                    .expect("Decoding Epoch failed");
                if inserted_epoch <= timeout_epoch {
                    is_timed_out = true;
                }
            }

            if vote_tallies::is_seen_key(&key) {
                is_seen = bool::try_from_slice(&val[..])
                    .expect("Decoding boolean failed");
            }
        }
    }
    // check the last one
    if let Some(cur_keys) = cur_keys {
        if is_timed_out && !is_seen {
            results.push(cur_keys);
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap, HashSet};

    use borsh::BorshDeserialize;
    use namada_core::ledger::eth_bridge::storage::wrapped_erc20s;
    use namada_core::ledger::storage::testing::TestWlStorage;
    use namada_core::types::address;
    use namada_core::types::erc20tokens::Erc20Amount;
    use namada_core::types::ethereum_events::testing::{
        arbitrary_eth_address, arbitrary_nonce, arbitrary_single_transfer,
        DAI_ERC20_ETH_ADDRESS,
    };
    use namada_core::types::ethereum_events::{
        EthereumEvent, TransferToNamada,
    };
    use namada_core::types::token::Amount;

    use super::*;
    use crate::protocol::transactions::utils::GetVoters;
    use crate::protocol::transactions::votes::Votes;
    use crate::test_utils;

    #[test]
    /// Test applying a `TransfersToNamada` batch containing a single transfer
    fn test_apply_single_transfer() -> Result<()> {
        let sole_validator = address::testing::gen_established_address();
        let receiver = address::testing::established_address_2();

        let amount = Erc20Amount::from_int(10u64, 10).expect("Test failed");
        let asset = arbitrary_eth_address();
        let body = EthereumEvent::TransfersToNamada {
            nonce: arbitrary_nonce(),
            transfers: vec![TransferToNamada {
                amount,
                asset,
                receiver: receiver.clone(),
            }],
        };
        let update = EthMsgUpdate {
            body: body.clone(),
            seen_by: Votes::from([(sole_validator.clone(), BlockHeight(100))]),
        };
        let updates = HashSet::from_iter(vec![update]);
        let voting_powers = HashMap::from_iter(vec![(
            (sole_validator.clone(), BlockHeight(100)),
            FractionalVotingPower::new(1, 1).unwrap(),
        )]);
        let mut wl_storage = TestWlStorage::default();
        test_utils::bootstrap_ethereum_bridge(&mut wl_storage);

        let changed_keys =
            apply_updates(&mut wl_storage, updates, voting_powers)?;

        let eth_msg_keys: vote_tallies::Keys<EthereumEvent> = (&body).into();
        let wrapped_erc20_keys: wrapped_erc20s::Keys = (&asset).into();
        assert_eq!(
            BTreeSet::from_iter(vec![
                eth_msg_keys.body(),
                eth_msg_keys.seen(),
                eth_msg_keys.seen_by(),
                eth_msg_keys.voting_power(),
                eth_msg_keys.epoch(),
                wrapped_erc20_keys.balance(&receiver),
                wrapped_erc20_keys.supply(),
            ]),
            changed_keys
        );

        let body_bytes = wl_storage.read_bytes(&eth_msg_keys.body())?;
        let body_bytes = body_bytes.unwrap();
        assert_eq!(EthereumEvent::try_from_slice(&body_bytes)?, body);

        let seen_bytes = wl_storage.read_bytes(&eth_msg_keys.seen())?;
        let seen_bytes = seen_bytes.unwrap();
        assert!(bool::try_from_slice(&seen_bytes)?);

        let seen_by_bytes = wl_storage.read_bytes(&eth_msg_keys.seen_by())?;
        let seen_by_bytes = seen_by_bytes.unwrap();
        assert_eq!(
            Votes::try_from_slice(&seen_by_bytes)?,
            Votes::from([(sole_validator, BlockHeight(100))])
        );

        let voting_power_bytes =
            wl_storage.read_bytes(&eth_msg_keys.voting_power())?;
        let voting_power_bytes = voting_power_bytes.unwrap();
        assert_eq!(<(u64, u64)>::try_from_slice(&voting_power_bytes)?, (1, 1));

        let epoch_bytes = wl_storage.read_bytes(&eth_msg_keys.epoch())?;
        let epoch_bytes = epoch_bytes.unwrap();
        assert_eq!(Epoch::try_from_slice(&epoch_bytes)?, Epoch(0));

        let wrapped_erc20_balance_bytes =
            wl_storage.read_bytes(&wrapped_erc20_keys.balance(&receiver))?;
        let wrapped_erc20_balance_bytes = wrapped_erc20_balance_bytes.unwrap();
        assert_eq!(
            Erc20Amount::try_from_slice(&wrapped_erc20_balance_bytes)?,
            amount
        );

        let wrapped_erc20_supply_bytes =
            wl_storage.read_bytes(&wrapped_erc20_keys.supply())?;
        let wrapped_erc20_supply_bytes = wrapped_erc20_supply_bytes.unwrap();
        assert_eq!(
            Erc20Amount::try_from_slice(&wrapped_erc20_supply_bytes)?,
            amount
        );

        Ok(())
    }

    #[test]
    /// Test applying a single transfer via `apply_derived_tx`, where an event
    /// has enough voting power behind it for it to be applied at the same time
    /// that it is recorded in storage
    fn test_apply_derived_tx_new_event_mint_immediately() {
        let sole_validator = address::testing::established_address_2();
        let (mut wl_storage, _) = test_utils::setup_storage_with_validators(
            HashMap::from_iter(vec![(sole_validator.clone(), 100_u64.into())]),
        );
        test_utils::bootstrap_ethereum_bridge(&mut wl_storage);
        let receiver = address::testing::established_address_1();

        let event = EthereumEvent::TransfersToNamada {
            nonce: 1.into(),
            transfers: vec![TransferToNamada {
                amount: Amount::from(100).into(),
                asset: DAI_ERC20_ETH_ADDRESS,
                receiver: receiver.clone(),
            }],
        };

        let result = apply_derived_tx(
            &mut wl_storage,
            vec![MultiSignedEthEvent {
                event: event.clone(),
                signers: BTreeSet::from([(sole_validator, BlockHeight(100))]),
            }],
        );

        let tx_result = match result {
            Ok(tx_result) => tx_result,
            Err(err) => panic!("unexpected error: {:#?}", err),
        };

        assert_eq!(
            tx_result.gas_used, 0,
            "No gas should be used for a derived transaction"
        );
        let eth_msg_keys = vote_tallies::Keys::from(&event);
        let dai_keys = wrapped_erc20s::Keys::from(&DAI_ERC20_ETH_ADDRESS);
        assert_eq!(
            tx_result.changed_keys,
            BTreeSet::from_iter(vec![
                eth_msg_keys.body(),
                eth_msg_keys.seen(),
                eth_msg_keys.seen_by(),
                eth_msg_keys.voting_power(),
                eth_msg_keys.epoch(),
                dai_keys.balance(&receiver),
                dai_keys.supply(),
            ])
        );
        assert!(tx_result.vps_result.accepted_vps.is_empty());
        assert!(tx_result.vps_result.rejected_vps.is_empty());
        assert!(tx_result.vps_result.errors.is_empty());
        assert!(tx_result.initialized_accounts.is_empty());
        assert!(tx_result.ibc_event.is_none());
    }

    /// Test calling apply_derived_tx for an event that isn't backed by enough
    /// voting power to be acted on immediately
    #[test]
    fn test_apply_derived_tx_new_event_dont_mint() {
        let validator_a = address::testing::established_address_2();
        let validator_b = address::testing::established_address_3();
        let (mut wl_storage, _) = test_utils::setup_storage_with_validators(
            HashMap::from_iter(vec![
                (validator_a.clone(), 100_u64.into()),
                (validator_b, 100_u64.into()),
            ]),
        );
        test_utils::bootstrap_ethereum_bridge(&mut wl_storage);
        let receiver = address::testing::established_address_1();

        let event = EthereumEvent::TransfersToNamada {
            nonce: 1.into(),
            transfers: vec![TransferToNamada {
                amount: Amount::from(100).into(),
                asset: DAI_ERC20_ETH_ADDRESS,
                receiver,
            }],
        };

        let result = apply_derived_tx(
            &mut wl_storage,
            vec![MultiSignedEthEvent {
                event: event.clone(),
                signers: BTreeSet::from([(validator_a, BlockHeight(100))]),
            }],
        );
        let tx_result = match result {
            Ok(tx_result) => tx_result,
            Err(err) => panic!("unexpected error: {:#?}", err),
        };

        let eth_msg_keys = vote_tallies::Keys::from(&event);
        assert_eq!(
            tx_result.changed_keys,
            BTreeSet::from_iter(vec![
                eth_msg_keys.body(),
                eth_msg_keys.seen(),
                eth_msg_keys.seen_by(),
                eth_msg_keys.voting_power(),
                eth_msg_keys.epoch(),
            ]),
            "The Ethereum event should have been recorded, but no minting \
             should have happened yet as it has only been seen by 1/2 the \
             voting power so far"
        );
    }

    #[test]
    /// Test that attempts made to apply duplicate
    /// [`MultiSignedEthEvent`]s in a single [`apply_derived_tx`] call don't
    /// result in duplicate votes in storage.
    pub fn test_apply_derived_tx_duplicates() -> Result<()> {
        let validator_a = address::testing::established_address_2();
        let validator_b = address::testing::established_address_3();
        let (mut wl_storage, _) = test_utils::setup_storage_with_validators(
            HashMap::from_iter(vec![
                (validator_a.clone(), 100_u64.into()),
                (validator_b, 100_u64.into()),
            ]),
        );

        let event = EthereumEvent::TransfersToNamada {
            nonce: 1.into(),
            transfers: vec![TransferToNamada {
                amount: Amount::from(100).into(),
                asset: DAI_ERC20_ETH_ADDRESS,
                receiver: address::testing::established_address_1(),
            }],
        };
        // two votes for the same event from validator A
        let signers = BTreeSet::from([(validator_a.clone(), BlockHeight(100))]);
        let multisigned = MultiSignedEthEvent {
            event: event.clone(),
            signers,
        };

        let multisigneds = vec![multisigned.clone(), multisigned];

        let result = apply_derived_tx(&mut wl_storage, multisigneds);
        let tx_result = match result {
            Ok(tx_result) => tx_result,
            Err(err) => panic!("unexpected error: {:#?}", err),
        };

        let eth_msg_keys = vote_tallies::Keys::from(&event);
        assert_eq!(
            tx_result.changed_keys,
            BTreeSet::from_iter(vec![
                eth_msg_keys.body(),
                eth_msg_keys.seen(),
                eth_msg_keys.seen_by(),
                eth_msg_keys.voting_power(),
                eth_msg_keys.epoch(),
            ]),
            "One vote for the Ethereum event should have been recorded",
        );

        let seen_by_bytes = wl_storage.read_bytes(&eth_msg_keys.seen_by())?;
        let seen_by_bytes = seen_by_bytes.unwrap();
        assert_eq!(
            Votes::try_from_slice(&seen_by_bytes)?,
            Votes::from([(validator_a, BlockHeight(100))])
        );

        let voting_power_bytes =
            wl_storage.read_bytes(&eth_msg_keys.voting_power())?;
        let voting_power_bytes = voting_power_bytes.unwrap();
        assert_eq!(<(u64, u64)>::try_from_slice(&voting_power_bytes)?, (1, 2));

        Ok(())
    }

    #[test]
    /// Assert we don't return anything if we try to get the votes for an empty
    /// set of updates
    pub fn test_get_votes_for_updates_empty() {
        let updates = HashSet::new();
        assert!(updates.get_voters(0.into()).is_empty());
    }

    #[test]
    /// Test that we correctly get the votes from a set of updates
    pub fn test_get_votes_for_events() {
        let updates = HashSet::from([
            EthMsgUpdate {
                body: arbitrary_single_transfer(
                    1.into(),
                    address::testing::established_address_1(),
                ),
                seen_by: Votes::from([
                    (
                        address::testing::established_address_1(),
                        BlockHeight(100),
                    ),
                    (
                        address::testing::established_address_2(),
                        BlockHeight(102),
                    ),
                ]),
            },
            EthMsgUpdate {
                body: arbitrary_single_transfer(
                    2.into(),
                    address::testing::established_address_2(),
                ),
                seen_by: Votes::from([
                    (
                        address::testing::established_address_1(),
                        BlockHeight(101),
                    ),
                    (
                        address::testing::established_address_3(),
                        BlockHeight(100),
                    ),
                ]),
            },
        ]);
        let voters = updates.get_voters(0.into());
        assert_eq!(
            voters,
            HashSet::from([
                (address::testing::established_address_1(), BlockHeight(100)),
                (address::testing::established_address_1(), BlockHeight(101)),
                (address::testing::established_address_2(), BlockHeight(102)),
                (address::testing::established_address_3(), BlockHeight(100))
            ])
        )
    }

    #[test]
    /// Test that timed out events are deleted
    pub fn test_timeout_events() {
        let validator_a = address::testing::established_address_2();
        let validator_b = address::testing::established_address_3();
        let (mut wl_storage, _) = test_utils::setup_storage_with_validators(
            HashMap::from_iter(vec![
                (validator_a.clone(), 100_u64.into()),
                (validator_b, 100_u64.into()),
            ]),
        );
        test_utils::bootstrap_ethereum_bridge(&mut wl_storage);
        let receiver = address::testing::established_address_1();

        let event = EthereumEvent::TransfersToNamada {
            nonce: 1.into(),
            transfers: vec![TransferToNamada {
                amount: Amount::from(100).into(),
                asset: DAI_ERC20_ETH_ADDRESS,
                receiver: receiver.clone(),
            }],
        };
        let _result = apply_derived_tx(
            &mut wl_storage,
            vec![MultiSignedEthEvent {
                event: event.clone(),
                signers: BTreeSet::from([(
                    validator_a.clone(),
                    BlockHeight(100),
                )]),
            }],
        );
        let prev_keys = vote_tallies::Keys::from(&event);

        // commit then update the epoch
        wl_storage.storage.commit_block().unwrap();
        let unbonding_len = namada_proof_of_stake::read_pos_params(&wl_storage)
            .expect("Test failed")
            .unbonding_len
            + 1;
        wl_storage.storage.last_epoch =
            wl_storage.storage.last_epoch + unbonding_len;
        wl_storage.storage.block.epoch = wl_storage.storage.last_epoch + 1_u64;

        let new_event = EthereumEvent::TransfersToNamada {
            nonce: 2.into(),
            transfers: vec![TransferToNamada {
                amount: Amount::from(100).into(),
                asset: DAI_ERC20_ETH_ADDRESS,
                receiver,
            }],
        };
        let result = apply_derived_tx(
            &mut wl_storage,
            vec![MultiSignedEthEvent {
                event: new_event.clone(),
                signers: BTreeSet::from([(validator_a, BlockHeight(100))]),
            }],
        );
        let tx_result = match result {
            Ok(tx_result) => tx_result,
            Err(err) => panic!("unexpected error: {:#?}", err),
        };

        let new_keys = vote_tallies::Keys::from(&new_event);
        assert_eq!(
            tx_result.changed_keys,
            BTreeSet::from_iter(vec![
                prev_keys.body(),
                prev_keys.seen(),
                prev_keys.seen_by(),
                prev_keys.voting_power(),
                prev_keys.epoch(),
                new_keys.body(),
                new_keys.seen(),
                new_keys.seen_by(),
                new_keys.voting_power(),
                new_keys.epoch(),
            ]),
            "New event should be inserted and the previous one should be \
             deleted",
        );
        assert!(wl_storage.read_bytes(&prev_keys.body()).unwrap().is_none());
        assert!(wl_storage.read_bytes(&new_keys.body()).unwrap().is_some());
    }
}
