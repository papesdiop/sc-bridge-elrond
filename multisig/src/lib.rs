#![no_std]
#![allow(non_snake_case)]
#![allow(clippy::too_many_arguments)]

mod action;
mod user_role;

use action::Action;
use esdt_safe::{EsdtSafeTxBatch, EsdtSafeTxBatchSplitInFields};
use transaction::*;
use user_role::UserRole;

mod multisig_general;
mod setup;
mod storage;
mod util;

elrond_wasm::imports!();

/// Multi-signature smart contract implementation.
/// Acts like a wallet that needs multiple signers for any action performed.
#[elrond_wasm_derive::contract]
pub trait Multisig:
    multisig_general::MultisigGeneralModule
    + setup::SetupModule
    + storage::StorageModule
    + util::UtilModule
{
    /// Owner claims accumulated fees and distributes them to relayers
    /// Only centralized until we have a finalized fee distribution algorithm in place
    /// Not using get_owner API function, as the owner may also be a multisig SC
    #[endpoint(claimAccumulatedFees)]
    fn claim_accumulated_fees(&self, dest_address: Address) -> SCResult<()> {
        self.require_caller_owner()?;
        require!(
            !self.blockchain().is_smart_contract(&dest_address),
            "Cannot have SC as destination"
        );

        self.ethereum_fee_prepay_proxy(self.ethereum_fee_prepay_address().get())
            .claim_accumulated_fees(dest_address)
            .execute_on_dest_context();

        Ok(())
    }

    #[payable("EGLD")]
    #[endpoint]
    fn stake(&self, #[payment] payment: Self::BigUint) -> SCResult<()> {
        let caller = self.blockchain().get_caller();
        let caller_role = self.user_role(&caller);
        require!(
            caller_role == UserRole::BoardMember,
            "Only board members can stake"
        );

        self.amount_staked(&caller)
            .update(|amount_staked| *amount_staked += payment);

        Ok(())
    }

    #[endpoint]
    fn unstake(&self, amount: Self::BigUint) -> SCResult<()> {
        let caller = self.blockchain().get_caller();
        let amount_staked = self.amount_staked(&caller).get();
        require!(
            amount <= amount_staked,
            "can't unstake more than amount staked"
        );

        let remaining_stake = &amount_staked - &amount;
        if self.user_role(&caller) == UserRole::BoardMember {
            let required_stake_amount = self.required_stake_amount().get();
            require!(
                remaining_stake >= required_stake_amount,
                "can't unstake, must keep minimum amount as insurance"
            );
        }

        self.amount_staked(&caller).set(&remaining_stake);
        self.send().direct_egld(&caller, &amount, &[]);

        Ok(())
    }

    // ESDT Safe SC calls

    #[endpoint(getNextTransactionBatch)]
    fn get_next_transaction_batch(&self) -> SCResult<EsdtSafeTxBatchSplitInFields<Self::BigUint>> {
        self.require_esdt_safe_deployed()?;
        require!(
            self.current_tx_batch().is_empty(),
            "Must execute and set status for current tx batch first"
        );

        let caller = self.blockchain().get_caller();
        let caller_role = self.user_role(&caller);
        require!(
            caller_role == UserRole::BoardMember,
            "Only board members can call this function"
        );

        let esdt_safe_tx_batch = self
            .esdt_safe_proxy(self.esdt_safe_address().get())
            .get_next_transaction_batch()
            .execute_on_dest_context();
        let esdt_safe_batch_id = esdt_safe_tx_batch.batch_id;
        let batch_len = esdt_safe_tx_batch.transactions.len();

        if batch_len > 0 {
            self.current_tx_batch().set(&esdt_safe_tx_batch);
        }

        // convert into MultiResult for easier parsing
        let mut result_vec = Vec::with_capacity(batch_len);
        for tx in esdt_safe_tx_batch.transactions {
            result_vec.push(tx.into_multiresult());
        }

        Ok((esdt_safe_batch_id, result_vec.into()).into())
    }

    #[endpoint(proposeEsdtSafeSetCurrentTransactionBatchStatus)]
    fn propose_esdt_safe_set_current_transaction_batch_status(
        &self,
        esdt_safe_batch_id: usize,
        #[var_args] tx_batch_status: VarArgs<TransactionStatus>,
    ) -> SCResult<usize> {
        self.require_esdt_safe_deployed()?;
        require!(
            !self.current_tx_batch().is_empty(),
            "There is no transaction to set status for"
        );
        require!(
            self.action_id_for_set_current_transaction_batch_status(esdt_safe_batch_id)
                .get(&tx_batch_status.0)
                == None,
            "Action already proposed"
        );

        let esdt_safe_tx_batch = self.current_tx_batch().get();
        let current_batch_len = esdt_safe_tx_batch.transactions.len();
        let status_batch_len = tx_batch_status.len();
        require!(
            current_batch_len == status_batch_len,
            "Number of statuses provided must be equal to number of transactions in current batch"
        );
        require!(
            esdt_safe_batch_id == esdt_safe_tx_batch.batch_id,
            "Current EsdtSafe tx batch does not have the provided ID"
        );

        let action_id = self.propose_action(Action::SetCurrentTransactionBatchStatus {
            esdt_safe_batch_id,
            tx_batch_status: tx_batch_status.0.clone(),
        })?;

        self.action_id_for_set_current_transaction_batch_status(esdt_safe_batch_id)
            .insert(tx_batch_status.0, action_id);

        Ok(action_id)
    }

    // Multi-transfer ESDT SC calls

    #[endpoint(proposeMultiTransferEsdtBatch)]
    fn propose_multi_transfer_esdt_batch(
        &self,
        batch_id: u64,
        #[var_args] transfers: MultiArgVec<MultiArg3<Address, TokenIdentifier, Self::BigUint>>,
    ) -> SCResult<usize> {
        self.require_multi_transfer_esdt_deployed()?;

        let transfers_as_tuples = self.transfers_multiarg_to_tuples_vec(transfers);

        require!(
            self.batch_id_to_action_id_mapping(batch_id)
                .get(&transfers_as_tuples)
                == None,
            "This batch was already proposed"
        );

        let action_id = self.propose_action(Action::BatchTransferEsdtToken {
            batch_id,
            transfers: transfers_as_tuples.clone(),
        })?;

        self.batch_id_to_action_id_mapping(batch_id)
            .insert(transfers_as_tuples, action_id);

        Ok(action_id)
    }

    /// Proposers and board members use this to launch signed actions.
    #[endpoint(performAction)]
    fn perform_action_endpoint(
        &self,
        action_id: usize,
    ) -> SCResult<OptionalResult<MultiResultVec<TransactionStatus>>> {
        let caller_address = self.blockchain().get_caller();
        let caller_id = self.user_mapper().get_user_id(&caller_address);
        let caller_role = self.get_user_id_to_role(caller_id);
        require!(
            caller_role.can_perform_action(),
            "only board members and proposers can perform actions"
        );
        require!(
            self.quorum_reached(action_id),
            "quorum has not been reached"
        );

        self.perform_action(action_id)
    }

    #[view(getCurrentTxBatch)]
    fn get_current_tx_batch(&self) -> EsdtSafeTxBatchSplitInFields<Self::BigUint> {
        let esdt_safe_tx_batch = if !self.current_tx_batch().is_empty() {
            self.current_tx_batch().get()
        } else {
            EsdtSafeTxBatch::default()
        };
        let batch_len = esdt_safe_tx_batch.transactions.len();

        let mut result_vec = Vec::with_capacity(batch_len);
        for tx in esdt_safe_tx_batch.transactions {
            result_vec.push(tx.into_multiresult());
        }

        (esdt_safe_tx_batch.batch_id, result_vec.into()).into()
    }

    #[view(isValidActionId)]
    fn is_valid_action_id(&self, action_id: usize) -> bool {
        let min_id = 1;
        let max_id = self.action_mapper().len();

        action_id >= min_id && action_id <= max_id
    }

    /// Actions are cleared after execution, so an empty entry means the action was executed already
    /// Returns "false" if the action ID is invalid
    #[view(wasActionExecuted)]
    fn was_action_executed(&self, action_id: usize) -> bool {
        if self.is_valid_action_id(action_id) {
            self.action_mapper().item_is_empty(action_id)
        } else {
            false
        }
    }

    /// If the mapping was made, it means that the transfer action was proposed in the past
    /// To check if it was executed as well, use the wasActionExecuted view
    #[view(wasTransferActionProposed)]
    fn was_transfer_action_proposed(
        &self,
        batch_id: u64,
        #[var_args] transfers: MultiArgVec<MultiArg3<Address, TokenIdentifier, Self::BigUint>>,
    ) -> bool {
        let action_id = self.get_action_id_for_transfer_batch(batch_id, transfers);

        self.is_valid_action_id(action_id)
    }

    #[view(getActionIdForTransferBatch)]
    fn get_action_id_for_transfer_batch(
        &self,
        batch_id: u64,
        #[var_args] transfers: MultiArgVec<MultiArg3<Address, TokenIdentifier, Self::BigUint>>,
    ) -> usize {
        let transfers_as_tuples = self.transfers_multiarg_to_tuples_vec(transfers);

        self.batch_id_to_action_id_mapping(batch_id)
            .get(&transfers_as_tuples)
            .unwrap_or(0)
    }

    #[view(wasSetCurrentTransactionBatchStatusActionProposed)]
    fn was_set_current_transaction_batch_status_action_proposed(
        &self,
        esdt_safe_batch_id: usize,
        #[var_args] expected_tx_batch_status: VarArgs<TransactionStatus>,
    ) -> bool {
        self.is_valid_action_id(self.get_action_id_for_set_current_transaction_batch_status(
            esdt_safe_batch_id,
            expected_tx_batch_status,
        ))
    }

    #[view(getActionIdForSetCurrentTransactionBatchStatus)]
    fn get_action_id_for_set_current_transaction_batch_status(
        &self,
        esdt_safe_batch_id: usize,
        #[var_args] expected_tx_batch_status: VarArgs<TransactionStatus>,
    ) -> usize {
        self.action_id_for_set_current_transaction_batch_status(esdt_safe_batch_id)
            .get(&expected_tx_batch_status.0)
            .unwrap_or(0)
    }

    // private

    fn perform_action(
        &self,
        action_id: usize,
    ) -> SCResult<OptionalResult<MultiResultVec<TransactionStatus>>> {
        let action = self.action_mapper().get(action_id);

        if self.pause_status().get() {
            require!(
                action.is_slash_user(),
                "Only slash user action may be performed while paused"
            );
        }

        // clean up storage
        // happens before actual execution, because the match provides the return on each branch
        // syntax aside, the async_call_raw kills contract execution so cleanup cannot happen afterwards
        self.clear_action(action_id);

        // only used when the action is batch transfer from Ethereum -> Elrond
        let mut return_statuses = OptionalResult::None;

        match action {
            Action::Nothing => {}
            Action::AddBoardMember(board_member_address) => {
                self.change_user_role(board_member_address, UserRole::BoardMember);
            }
            Action::AddProposer(proposer_address) => {
                self.change_user_role(proposer_address, UserRole::Proposer);

                // validation required for the scenario when a board member becomes a proposer
                require!(
                    self.quorum().get() <= self.num_board_members().get(),
                    "quorum cannot exceed board size"
                );
            }
            Action::RemoveUser(user_address) => {
                self.change_user_role(user_address, UserRole::None);
                let num_board_members = self.num_board_members().get();
                let num_proposers = self.num_proposers().get();
                require!(
                    num_board_members + num_proposers > 0,
                    "cannot remove all board members and proposers"
                );
                require!(
                    self.quorum().get() <= num_board_members,
                    "quorum cannot exceed board size"
                );
            }
            Action::SlashUser(user_address) => {
                self.change_user_role(user_address.clone(), UserRole::None);
                let num_board_members = self.num_board_members().get();
                let num_proposers = self.num_proposers().get();

                require!(
                    num_board_members + num_proposers > 0,
                    "cannot remove all board members and proposers"
                );
                require!(
                    self.quorum().get() <= num_board_members,
                    "quorum cannot exceed board size"
                );

                let slash_amount = self.slash_amount().get();

                // remove slashed amount from user stake amount
                let mut user_stake = self.amount_staked(&user_address).get();
                user_stake -= &slash_amount;
                self.amount_staked(&user_address).set(&user_stake);

                // add it to total slashed amount pool
                let mut total_slashed_amount = self.slashed_tokens_amount().get();
                total_slashed_amount += &slash_amount;
                self.slashed_tokens_amount().set(&total_slashed_amount);
            }
            Action::ChangeQuorum(new_quorum) => {
                require!(
                    new_quorum <= self.num_board_members().get(),
                    "quorum cannot exceed board size"
                );
                self.quorum().set(&new_quorum);
            }
            Action::SetCurrentTransactionBatchStatus {
                esdt_safe_batch_id,
                tx_batch_status,
            } => {
                let esdt_safe_tx_batch = self.current_tx_batch().get();
                let current_tx_batch = &esdt_safe_tx_batch.transactions;

                self.current_tx_batch().clear();

                let mut action_ids_mapper =
                    self.action_id_for_set_current_transaction_batch_status(esdt_safe_batch_id);

                // if there's only one proposed action,
                // the action was already cleared at the beginning of this function
                if action_ids_mapper.len() > 1 {
                    for act_id in action_ids_mapper.values() {
                        self.clear_action(act_id);
                    }
                }

                action_ids_mapper.clear();

                let mut args = Vec::new();
                for (tx, tx_status) in current_tx_batch.iter().zip(tx_batch_status.iter()) {
                    args.push((tx.from.clone(), tx.nonce, *tx_status));
                }

                self.esdt_safe_proxy(self.esdt_safe_address().get())
                    .set_transaction_batch_status(VarArgs::from(args))
                    .execute_on_dest_context();
            }
            Action::BatchTransferEsdtToken {
                batch_id,
                transfers,
            } => {
                let mut action_ids_mapper = self.batch_id_to_action_id_mapping(batch_id);

                // if there's only one proposed action,
                // the action was already cleared at the beginning of this function
                if action_ids_mapper.len() > 1 {
                    for act_id in action_ids_mapper.values() {
                        self.clear_action(act_id);
                    }
                }

                action_ids_mapper.clear();

                let statuses = self
                    .multi_transfer_esdt_proxy(self.multi_transfer_esdt_address().get())
                    .batch_transfer_esdt_token(transfers.into())
                    .execute_on_dest_context();

                return_statuses = OptionalResult::Some(statuses);
            }
        }

        Ok(return_statuses)
    }

    // proxies

    #[proxy]
    fn egld_esdt_swap_proxy(&self, sc_address: Address) -> egld_esdt_swap::Proxy<Self::SendApi>;

    #[proxy]
    fn esdt_safe_proxy(&self, sc_address: Address) -> esdt_safe::Proxy<Self::SendApi>;

    #[proxy]
    fn multi_transfer_esdt_proxy(
        &self,
        sc_address: Address,
    ) -> multi_transfer_esdt::Proxy<Self::SendApi>;

    #[proxy]
    fn ethereum_fee_prepay_proxy(
        &self,
        sc_address: Address,
    ) -> ethereum_fee_prepay::Proxy<Self::SendApi>;
}
