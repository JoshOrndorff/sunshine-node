#![allow(clippy::string_lit_as_bytes)]
#![allow(clippy::redundant_closure_call)]
#![allow(clippy::type_complexity)]
#![cfg_attr(not(feature = "std"), no_std)]
//! back to [`util`](../util/index.html) for all object and trait definitions

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

use util::{
    bank::{
        BankState, DepositInfo, OnChainTreasuryID,
        PaymentConfirmation, WithdrawalPermissions, ReservationInfo, InternalTransferInfo,
    },
    court::Evidence,
    organization::{FormedOrganization, ShareID},
    traits::{
        CheckBankBalances, DepositSpendOps, GenerateUniqueID,
        GetInnerOuterShareGroups, IDIsAvailable, OffChainBank,
        OnChainBank, DepositInformation, TransferInformation, OrgChecks, OrganizationDNS, BankSpends, BankReservations, WithdrawFromBank, DepositIntoBank, BankDepositsAndSpends,
        RegisterOffChainBankAccount, RegisterBankAccount, RegisterShareGroup,
        ShareGroupChecks, SupervisorPermissions, SupportedOrganizationShapes,
        WeightedShareIssuanceWrapper, WeightedShareWrapper,
    },
};

use frame_support::{
    decl_error, decl_event, decl_module, decl_storage, ensure,
    storage::IterableStorageDoubleMap,
    traits::{Currency, Get, ExistenceRequirement},
};
use frame_system::{self as system, ensure_signed};
use sp_runtime::{
    traits::{AccountIdConversion, Zero},
    DispatchError, DispatchResult, Permill,
};
use sp_std::prelude::*;

/// Common ipfs type alias for our modules
pub type IpfsReference = Vec<u8>;
/// The organization identfier
pub type OrgId = u32;
/// The deposit identifier
pub type DepositId = u32;
/// The spend reservation identifier
pub type ReservationId = u32;
/// The weighted shares
pub type SharesOf<T> = <<T as Trait>::Organization as WeightedShareWrapper<
    u32,
    u32,
    <T as frame_system::Trait>::AccountId,
>>::Shares;
/// The balances type for this module
type BalanceOf<T> =
    <<T as Trait>::Currency as Currency<<T as frame_system::Trait>::AccountId>>::Balance;
/// This is the governance config used for things in this prototype
/// - it reads `(OrgId, ShareID) s.t. ShareID = { Flat(u32), Weighted(u32)}` such that each refers to records in separate inherited modules in `Organization`
pub type GovernanceConfig<T> = (u32, <<T as Trait>::Organization as ShareGroupChecks<u32, <T as frame_system::Trait>::AccountId>>::MultiShareIdentifier);

pub trait Trait: system::Trait {
    type Event: From<Event<Self>> + Into<<Self as frame_system::Trait>::Event>;

    /// The currency type for on-chain transactions
    type Currency: Currency<Self::AccountId>;

    type Organization: OrgChecks<u32, Self::AccountId>
        + ShareGroupChecks<u32, Self::AccountId>
        + GetInnerOuterShareGroups<u32, Self::AccountId>
        + SupervisorPermissions<u32, Self::AccountId>
        + WeightedShareWrapper<u32, u32, Self::AccountId>
        + WeightedShareIssuanceWrapper<u32, u32, Self::AccountId, Permill>
        + RegisterShareGroup<u32, u32, Self::AccountId, SharesOf<Self>>
        + OrganizationDNS<u32, Self::AccountId, IpfsReference>;
    
    type MinimumInitialDeposit: Get<BalanceOf<Self>>;
}

decl_event!(
    pub enum Event<T>
    where
        <T as frame_system::Trait>::AccountId,
        Balance = BalanceOf<T>,
    {
        NewOnChainTreasuryRegisteredWithSudoPermissions(OnChainTreasuryID, AccountId),
        NewOnChainTreasuryRegisteredWithWeightedShareGroupPermissions(OnChainTreasuryID, OrgId, ShareID),
        CapitalDepositedIntoOnChainBankAccount(AccountId, OnChainTreasuryID, Balance, IpfsReference),
        SudoWithdrawalFromOnChainBankAccount(OnChainTreasuryID, AccountId, Balance),
        WeightedShareGroupMemberClaimedPortionOfDepositToWithdraw(OnChainTreasuryID, AccountId, Balance),
        WeightedShareGroupMemberBurnedSharesToClaimProportionalWithdrawal(OnChainTreasuryID, AccountId, Balance),
    }
);

decl_error! {
    pub enum Error for Module<T: Trait> {
        MustHaveCertainAuthorityToRegisterBankAccount,
        MustBeWeightedShareGroupToCalculatePortionOfOnChainDeposit,
        CannotWithdrawIfOnChainBankDNE,
        CannotClaimDepositFromBankThatDNE,
        CannotCalculateDepositPortionFromBankThatDNE,
        CannotCalculateLiquidShareCapitalFromBankThatDNE,
        CannotBurnEnoughSharesToLiquidateCapitalForWithdrawalRequest,
        DepositCannotBeFoundToCalculateCorrectPortion,
        CanOnlyClaimUpToOwnershipPortionByDefault,
        BankAccountNotFoundForDeposit,
        BankAccountNotFoundForWithdrawal,
        BankAccountEitherNotSudoOrCallerIsNotDesignatedSudo,
        MustBeWeightedShareGroupToCalculatePortionLiquidShareCapital,
        NotEnoughFundsInReservedToAllowSpend,
    }
}

decl_storage! {
    trait Store for Module<T: Trait> as Bank {

        BankIDNonce get(fn bank_id_nonce): OnChainTreasuryID;

        ReservationIdentityNonce get(fn reservation_identity_nonce): map 
            hasher(opaque_blake2_256) OnChainTreasuryID => ReservationId;

        InternalTransfersIdentityNone get(fn internal_transfers_identity_nonce): map
            hasher(opaque_blake2_256) OnChainTreasuryID => u32;

        /// Source of truth for OnChainTreasuryId uniqueness checks
        /// WARNING: do not append a prefix because the keyspace is used directly for checking uniqueness
        /// TODO: pre-reserve any known ModuleId's that could be accidentally generated that already exist elsewhere
        BankStores get(fn bank_stores): map
            hasher(opaque_blake2_256) OnChainTreasuryID => Option<BankState<WithdrawalPermissions<T::AccountId>, BalanceOf<T>>>;

        /// All deposits made into the joint bank account represented by OnChainTreasuryID
        /// - I want to use DepositInfo as a key so that I can add Option<WithdrawalPermissions<T::AccountId>> as a value when deposits eventually have deposit-specific withdrawal permissions (like for grant milestones)
        Deposits get(fn deposits): double_map
            hasher(blake2_128_concat) OnChainTreasuryID,
            hasher(blake2_128_concat) DepositId => Option<DepositInfo<T::AccountId, IpfsReference, BalanceOf<T>>>;

        /// Spend reservations which designated a committee for formally transferring ownership to specific destination addresses
        SpendReservations get(fn spend_reservations): double_map
            hasher(blake2_128_concat) OnChainTreasuryID,
            hasher(blake2_128_concat) ReservationId => Option<ReservationInfo<IpfsReference, BalanceOf<T>, WithdrawalPermissions<T::AccountId>>>;

        /// Internal transfers of control over capital that allow transfer liquidity rights to the current controller
        InternalTransfers get(fn internal_transfers): double_map
            hasher(blake2_128_concat) OnChainTreasuryID,
            // TransferId
            hasher(blake2_128_concat) u32 => Option<InternalTransferInfo<IpfsReference, BalanceOf<T>, WithdrawalPermissions<T::AccountId>>>;
    }
}

decl_module! {
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        type Error = Error<T>;
        fn deposit_event() = default;

        #[weight = 0]
        fn register_on_chain_bank_account_with_sudo_permissions(
            origin,
            seed: BalanceOf<T>,
            sudo_acc: T::AccountId, // sole controller for the bank account
        ) -> DispatchResult {
            let seeder = ensure_signed(origin)?;
            let authentication: bool = Self::is_sudo_account(&seeder)
                || Self::is_organization_supervisor(1u32, &seeder);
            ensure!(authentication, Error::<T>::MustHaveCertainAuthorityToRegisterBankAccount);

            let new_bank_id = Self::register_on_chain_bank_account(seeder, seed, None, WithdrawalPermissions::Sudo(sudo_acc.clone()))?;
            Self::deposit_event(RawEvent::NewOnChainTreasuryRegisteredWithSudoPermissions(new_bank_id, sudo_acc));
            Ok(())
        }
        #[weight = 0]
        fn register_on_chain_bank_account_with_weighted_share_group_permissions(
            origin,
            seed: BalanceOf<T>,
            organization: OrgId,
            share_id: u32,
        ) -> DispatchResult {
            let seeder = ensure_signed(origin)?;
            let authentication: bool = Self::is_sudo_account(&seeder)
                || Self::is_organization_supervisor(1u32, &seeder);
            ensure!(authentication, Error::<T>::MustHaveCertainAuthorityToRegisterBankAccount);

            let wrapped_share_id = ShareID::WeightedAtomic(share_id);
            let new_bank_id = Self::register_on_chain_bank_account(seeder, seed, None, WithdrawalPermissions::RegisteredShareGroup(organization, wrapped_share_id))?;
            Self::deposit_event(RawEvent::NewOnChainTreasuryRegisteredWithWeightedShareGroupPermissions(new_bank_id, organization, wrapped_share_id));
            Ok(())
        }
        #[weight = 0]
        fn deposit_from_signer_into_on_chain_bank_account(
            origin,
            bank_id: OnChainTreasuryID,
            amount: BalanceOf<T>,
            savings_tax: Option<Permill>,
            reason: IpfsReference,
        ) -> DispatchResult {
            let depositer = ensure_signed(origin)?;

            Self::deposit_currency_into_on_chain_bank_account(depositer.clone(), bank_id, amount, savings_tax, reason.clone())?;
            Self::deposit_event(RawEvent::CapitalDepositedIntoOnChainBankAccount(depositer, bank_id, amount, reason));
            Ok(())
        }
        #[weight = 0]
        fn sudo_withdrawal_from_on_chain_bank_account(
            origin,
            bank_id: OnChainTreasuryID,
            to: T::AccountId,
            amount: BalanceOf<T>,
        ) -> DispatchResult {
            let sudo_withdrawer = ensure_signed(origin)?;
            let bank_account = <BankStores<T>>::get(bank_id).ok_or(Error::<T>::CannotWithdrawIfOnChainBankDNE)?;
            ensure!(bank_account.verify_sudo(&sudo_withdrawer), Error::<T>::BankAccountEitherNotSudoOrCallerIsNotDesignatedSudo);

            // all the bank categories are available to this account type so pass true, true
            Self::withdraw_from_on_chain_bank_account(bank_id, to.clone(), amount, true, true)?;
            Self::deposit_event(RawEvent::SudoWithdrawalFromOnChainBankAccount(bank_id, to, amount));
            Ok(())
        }
        #[weight = 0]
        fn burn_all_shares_to_leave_weighted_membership_bank(
            origin,
            bank_id: OnChainTreasuryID,
        ) -> DispatchResult {
            let leaving_member = ensure_signed(origin)?;
            let amount_withdrawn_by_burning_shares = Self::withdraw_capital_by_burning_shares(bank_id, leaving_member.clone(), None)?;
            Self::deposit_event(RawEvent::WeightedShareGroupMemberBurnedSharesToClaimProportionalWithdrawal(bank_id, leaving_member, amount_withdrawn_by_burning_shares));
            Ok(())
        }
        #[weight = 0]
        fn withdraw_due_portion_of_deposit_from_weighted_membership_bank(
            origin,
            bank_id: OnChainTreasuryID,
            deposit: DepositInfo<T::AccountId, IpfsReference, BalanceOf<T>>,
        ) -> DispatchResult {
            let to_claimer = ensure_signed(origin)?;
            let amount_withdrawn = Self::claim_portion_of_on_chain_deposit(bank_id, deposit, to_claimer.clone(), None)?;
            Self::deposit_event(RawEvent::WeightedShareGroupMemberClaimedPortionOfDepositToWithdraw(bank_id, to_claimer, amount_withdrawn));
            Ok(())
        }
    }
}

impl<T: Trait> Module<T> {
    fn account_id(id: OnChainTreasuryID) -> T::AccountId {
        id.into_account()
    }
    fn is_sudo_account(who: &T::AccountId) -> bool {
        <<T as Trait>::Organization as SupervisorPermissions<u32, T::AccountId>>::is_sudo_account(
            who,
        )
    }
    fn is_organization_supervisor(organization: u32, who: &T::AccountId) -> bool {
        <<T as Trait>::Organization as SupervisorPermissions<u32, T::AccountId>>::is_organization_supervisor(organization, who)
    }
    // fn is_share_supervisor(organization: u32, share_id: ShareID, who: &T::AccountId) -> bool {
    //     <<T as Trait>::Organization as SupervisorPermissions<u32, T::AccountId>>::is_share_supervisor(organization, share_id.into(), who)
    // }
    fn account_is_member_of_formed_organization(
        org: FormedOrganization,
        who: &T::AccountId,
    ) -> bool {
        match org {
            FormedOrganization::FlatOrg(org_id) => {
                <<T as Trait>::Organization as OrgChecks<u32, T::AccountId>>::check_membership_in_org(org_id, who)
            },
            FormedOrganization::FlatShares(org_id, share_id) => {
                <<T as Trait>::Organization as ShareGroupChecks<u32, T::AccountId>>::check_membership_in_share_group(org_id, ShareID::Flat(share_id).into(), who)
            },
            FormedOrganization::WeightedShares(org_id, share_id) => {
                <<T as Trait>::Organization as ShareGroupChecks<u32, T::AccountId>>::check_membership_in_share_group(org_id, ShareID::WeightedAtomic(share_id).into(), who)
            },
        }
    }
}

impl<T: Trait> IDIsAvailable<OnChainTreasuryID> for Module<T> {
    fn id_is_available(id: OnChainTreasuryID) -> bool {
        <BankStores<T>>::get(id).is_none()
    }
}

impl<T: Trait>
    IDIsAvailable<(
        OnChainTreasuryID,
        DepositInfo<T::AccountId, IpfsReference, BalanceOf<T>>,
    )> for Module<T>
{
    fn id_is_available(
        id: (
            OnChainTreasuryID,
            DepositInfo<T::AccountId, IpfsReference, BalanceOf<T>>,
        ),
    ) -> bool {
        !<Deposits<T>>::get(id.0, id.1)
    }
}

impl<T: Trait>
    GenerateUniqueID<(
        OnChainTreasuryID,
        DepositInfo<T::AccountId, IpfsReference, BalanceOf<T>>,
    )> for Module<T>
{
    fn generate_unique_id(
        proposed_id: (
            OnChainTreasuryID,
            DepositInfo<T::AccountId, IpfsReference, BalanceOf<T>>,
        ),
    ) -> (
        OnChainTreasuryID,
        DepositInfo<T::AccountId, IpfsReference, BalanceOf<T>>,
    ) {
        if !Self::id_is_available(proposed_id.clone()) {
            let mut new_deposit_id = proposed_id.1.iterate_salt();
            while !Self::id_is_available((proposed_id.0, new_deposit_id.clone())) {
                new_deposit_id = new_deposit_id.iterate_salt();
            }
            (proposed_id.0, new_deposit_id)
        } else {
            proposed_id
        }
    }
}

impl<T: Trait> GenerateUniqueID<OnChainTreasuryID> for Module<T> {
    fn generate_unique_id(proposed_id: OnChainTreasuryID) -> OnChainTreasuryID {
        if !Self::id_is_available(proposed_id) {
            let mut treasury_nonce_id = BankIDNonce::get().iterate();
            while !Self::id_is_available(treasury_nonce_id) {
                treasury_nonce_id = treasury_nonce_id.iterate();
            }
            treasury_nonce_id
        } else {
            proposed_id
        }
    }
}

impl<T: Trait> OnChainBank for Module<T> {
    type OrgId = u32; // TODO: here is where I should export the OrgId type from the Organization subtype
    type TreasuryId = OnChainTreasuryID;
}

impl<T: Trait> RegisterBankAccount<T::AccountId, BalanceOf<T>> for Module<T> {
    type GovernanceConfig = WithdrawalPermissions<T::AccountId>;
    fn register_on_chain_bank_account(
        registered_org: Self::OrgId,
        from: T::AccountId,
        amount: BalanceOf<T>, // TODO: ADD MINIMUM AMOUNT TO OPEN BANK
        owner_s: Self::GovernanceConfig,
    ) -> Result<Self::TreasuryId, DispatchError> {
        let proposed_id = OnChainTreasuryID::default();
        let generated_id = Self::generate_unique_id(proposed_id);
        // default all of it is put into savings but this optional param allows us to set some aside for spends
        let new_bank = BankState::new_from_deposit(registered_org, amount, owner_s);
        let to = Self::account_id(generated_id);
        T::Currency::transfer(&from, &to, amount, ExistenceRequirement::KeepAlive)?;
        <BankStores<T>>::insert(generated_id, new_bank);
        Ok(generated_id)
    }
}

impl<T: Trait> BankDepositsAndSpends<BalanceOf<T>> for Module<T> {
    type Bank = BankState<T::AccountId, BalanceOf<T>>;
    fn make_infallible_deposit_into_free(
        bank: Self::Bank,
        amount: BalanceOf<T>,
    ) -> Self::Bank {
        bank.deposit_into_free(amount)
    }
    fn fallible_spend_from_reserved(
        bank: Self::Bank,
        amount: BalanceOf<T>,
    ) -> Result<Self::Bank, DispatchError> {
        let new_bank = bank.spend_from_reserved(amount)
        .ok_or(Error::<T>::NotEnoughFundsInReservedToAllowSpend)?;
        Ok(new_bank)
    }
}

impl<T: Trait> CheckBankBalances<BalanceOf<T>> for Module<T> {
    fn get_bank_store(bank_id: Self::TreasuryId) -> Option<Self::Bank> {
        <BankStores<T>>::get(bank_id)
    }
    fn calculate_total_bank_balance_from_balances(bank_id: Self::TreasuryId) -> Option<BalanceOf<T>> {
        let bank_account = Self::account_id(bank_id);
        let bank_balance = T::Currency::total_balance(&bank_account);
        if bank_balance == 0.into() {
            None
        } else {
            Some(bank_balance)
        }
    }
}

impl<T: Trait> DepositIntoBank<T::AccountId, IpfsReference, BalanceOf<T>> for Module<T> {
    fn deposit_into_bank(
        from: T::AccountId,
        to_bank_id: Self::TreasuryId,
        reason: IpfsReference,
        amount: BalanceOf<T>,
    ) -> DispatchResult {
        Ok(())
    }
    // fn deposit_currency_into_on_chain_bank_account(
    //     from: T::AccountId,
    //     to_bank_id: Self::TreasuryId,
    //     amount: BalanceOf<T>,
    //     savings_tax: Option<Permill>,
    //     reason: IpfsReference,
    // ) -> DispatchResult {
    //     let bank_account = <BankStores<T>>::get(to_bank_id)
    //         .ok_or(Error::<T>::BankAccountNotFoundForDeposit)?;
    //     // make the transfer
    //     let dest = Self::account_id(to_bank_id);
    //     T::Currency::transfer(&from, &dest, amount, ExistenceRequirement::KeepAlive)?;
    //     // update the amount stored in the bank
    //     let updated_bank_balance =
    //         Self::make_deposit_to_update_bank_balance(bank_account, amount, savings_tax);
    //     <BankStores<T>>::insert(to_bank_id, updated_bank_balance);
    //     // form the deposit, no savings_pct allocated
    //     let new_deposit = DepositInfo::new(0u32, from, reason, amount, None);
    //     // generate unique deposit
    //     let unique_deposit = Self::generate_unique_id((to_bank_id, new_deposit));

    //     // TODO: in the future, this will hold Option<PermissionsDiff> s.t. PermissionsDiff defines payout rights for this deposit relative to the WithdrawalPermissions for the bank itself somehow (\exist restrictions on variance which determine approval for transfers)
    //     // TODO2: when will we delete this, how long is this going to stay in storage?
    //     <Deposits<T>>::insert(to_bank_id, unique_deposit.1, true);
    //     Ok(())
    // }
}

impl<T: Trait> WithdrawFromBank<T::AccountId, BalanceOf<T>> for Module<T> {
    fn withdraw_from_on_chain_bank_account(
        from_bank_id: Self::TreasuryId,
        to: T::AccountId,
        amount: BalanceOf<T>,
    ) -> DispatchResult {
        Ok(())
    }
    // // Call needs to be carefully permissioned
    // // AUTOMATIC APPROVAL ONLY IF
    // // - made by sudo_acc and WithdrawalPermissions::Sudo(sudo_acc)
    // // - portion is less than ownership in WithdrawalPermissions::_(_)
    // fn withdraw_from_on_chain_bank_account(
    //     from_bank_id: Self::TreasuryId,
    //     to: T::AccountId,
    //     amount: BalanceOf<T>,
    //     savings: bool,
    //     reserved_for_spends: bool,
    // ) -> DispatchResult {
    //     let bank_account = <BankStores<T>>::get(from_bank_id)
    //         .ok_or(Error::<T>::BankAccountNotFoundForWithdrawal)?;
    //     // update the amount stored in the bank
    //     let bank_after_withdrawal = Self::request_withdrawal_to_update_bank_balance(
    //         bank_account,
    //         amount,
    //         savings,
    //         reserved_for_spends,
    //     )?;
    //     // make the transfer
    //     let from = Self::account_id(from_bank_id);
    //     T::Currency::transfer(&from, &to, amount, ExistenceRequirement::KeepAlive)?;
    //     <BankStores<T>>::insert(from_bank_id, bank_after_withdrawal);

    //     // make the transfer
    //     let from = Self::account_id(from_bank_id);
    //     T::Currency::transfer(&from, &to, amount, ExistenceRequirement::KeepAlive)?;
    //     Ok(())
    // }
}

impl<T: Trait> BankReservations<T::AccountId, BalanceOf<T>> for Module<T> {
    fn reserve_for_spend(
        caller: T::AccountId, // must be in owner_s: GovernanceConfig for BankState, that's the auth
        bank_id: Self::TreasuryId,
        // acceptance committee for approving set aside spends below the amount
        supervision_committee: Option<Self::GovernanceConfig>, // default WithdrawalRules
        amount: BalanceOf<T>,
    ) -> DispatchResult {
        Ok(())
    }
    // Allocate some funds (previously set aside for spending reasons) to be withdrawable by new group
    // - this is an internal transfer to a team and it makes this capital withdrawable by them
    fn transfer_spending_power(
        caller: T::AccountId, // must be in reference's supervision_committee
        bank_id: Self::TreasuryId,
        // reference to specific reservation
        reservation_id: u32,
        // move control of funds to new outer group which can reserve or withdraw directly
        new_controller: Self::GovernanceConfig,
        amount: BalanceOf<T>,
    ) -> DispatchResult {
        Ok(())
    }
}

impl<T: Trait> BankSpends<T::AccountId, BalanceOf<T>> for Module<T> {
    fn spend_from_free(
        caller: T::AccountId,
        from_bank_id: Self::TreasuryId,
        to: T::AccountId,
        amount_requested: Option<BalanceOf<T>>,
    ) -> Result<BalanceOf<T>, DispatchError> {
        Ok(amount_requested.unwrap_or_else(BalanceOf::<T>::zero()))
    }
    // spends up to allowed amount by default
    fn spend_from_reserved(
        caller: T::AccountId,
        from_bank_id: Self::TreasuryId,
        transfer_id: u32, // refers to InternalTransfer, which transfers control over a subset of the overall funds
        to: T::AccountId,
        amount_requested: Option<BalanceOf<T>>,
    ) -> Result<BalanceOf<T>, DispatchError> {
        Ok(amount_requested.unwrap_or_else(BalanceOf::<T>::zero()))
    }
}

impl<T: Trait> DepositInformation<T::AccountId, BalanceOf<T>>
    for Module<T>
{
    type DepositInfo = DepositInfo<T::AccountId, IpfsReference, BalanceOf<T>>;

    fn get_deposits_by_account(
        bank_id: Self::TreasuryId,
        depositer: T::AccountId,
    ) -> Option<Vec<Self::DepositInfo>> {
        let depositers_deposits = <Deposits<T>>::iter()
            .filter(|(id, _, deposit)| id == &bank_id && deposit.depositer() == depositer)
            .map(|(_, _, deposit)| deposit)
            .collect::<Vec<Self::DepositInfo>>();
        if depositers_deposits.is_empty() {
            None
        } else {
            Some(depositers_deposits)
        }
    }
    fn total_capital_deposited_by_account(
        bank_id: Self::TreasuryId,
        depositer: T::AccountId,
    ) -> BalanceOf<T> {
        <Deposits<T>>::iter()
            .filter(|(id, _, deposit)| id == &bank_id && deposit.depositer() == depositer)
            .fold(BalanceOf::<T>::zero(), |acc, (_, _, deposit)| {
                acc + deposit.amount()
            })
    }
}

impl<T: Trait> TransferInformation<T::AccountId, BalanceOf<T>> for Module<T> {
    type TransferInfo = InternalTransferInfo<IpfsReference, BalanceOf<T>, WithdrawalPermissions<T::AccountId>>;
    fn get_transfers_by_account_that_invoked(
        bank_id: Self::TreasuryId,
        invoker: T::AccountId,
    ) -> Option<Vec<Self::TransferInfo>> {
        None
    }
    fn total_capital_transferred_by_account(
        bank_id: Self::TreasuryId,
        invoker: T::AccountId,
    ) -> BalanceOf<T> {
        BalanceOf::<T>::zero()
    }
}

// impl<T: Trait> OnChainWithdrawalFilters<T::AccountId, IpfsReference, BalanceOf<T>, Permill>
//     for Module<T>
// {
//     fn calculate_liquid_portion_of_on_chain_deposit(
//         from_bank_id: Self::TreasuryId,
//         deposit: Self::DepositInfo,
//         to_claimer: T::AccountId,
//     ) -> Result<BalanceOf<T>, DispatchError> {
//         // this is implied to be the withdrawable portion
//         // TODO: add the choice to withdraw capital or accept greater share ownership?
//         // - idk, I dont want to add too much confusion already, pick a default like it's set the same
//         // for everyone and they are mandated to withdraw and its reserved for them to withdraw...
//         let amount = if let Some(savings_pct) = deposit.savings_pct() {
//             let reserved_for_savings = savings_pct * deposit.amount();
//             deposit.amount() - reserved_for_savings
//         } else {
//             deposit.amount()
//         };
//         let deposit_dne = Self::id_is_available((from_bank_id, deposit));
//         ensure!(
//             !deposit_dne,
//             Error::<T>::DepositCannotBeFoundToCalculateCorrectPortion
//         );
//         // get the bank's controller information
//         let controller = <BankStores<T>>::get(from_bank_id)
//             .ok_or(Error::<T>::CannotCalculateDepositPortionFromBankThatDNE)?;
//         let org_share_id = controller
//             .extract_weighted_share_group_id()
//             .ok_or(Error::<T>::MustBeWeightedShareGroupToCalculatePortionOfOnChainDeposit)?;
//         // + 1 constant time map lookup
//         let total_shares_issued_for_group = <<T as Trait>::Organization as WeightedShareWrapper<
//             u32,
//             u32,
//             T::AccountId,
//         >>::get_outstanding_weighted_shares(
//             org_share_id.0, org_share_id.1
//         )?;
//         // + 1 constant time map lookup
//         let shares_owned_by_member = <<T as Trait>::Organization as WeightedShareWrapper<
//             u32,
//             u32,
//             T::AccountId,
//         >>::get_weighted_shares_for_member(
//             org_share_id.0, org_share_id.1, &to_claimer
//         )?;
//         let ownership_portion = Permill::from_rational_approximation(
//             shares_owned_by_member,
//             total_shares_issued_for_group,
//         );
//         // calculate the amount to withdraw;
//         let amount_to_withdraw = ownership_portion * amount;
//         Ok(amount_to_withdraw)
//     }
//     // no guarantees on the value this returns, on chain conditions change fast
//     fn calculate_liquid_share_capital_from_savings(
//         from_bank_id: Self::TreasuryId,
//         to_claimer: T::AccountId,
//     ) -> Result<(u32, u32, BalanceOf<T>), DispatchError> {
//         let bank_account = <BankStores<T>>::get(from_bank_id)
//             .ok_or(Error::<T>::CannotCalculateLiquidShareCapitalFromBankThatDNE)?;
//         // Burning Shares Only Yields Access To The Portion of SAVINGS -- it does not expose capital reserved for spends
//         // i.e. reserved for others as part of a grant milestone payment
//         let balance_withdrawable_by_burning_shares = bank_account.savings();
//         let org_share_id = bank_account
//             .extract_weighted_share_group_id()
//             .ok_or(Error::<T>::MustBeWeightedShareGroupToCalculatePortionLiquidShareCapital)?;
//         // + 1 constant time map lookup
//         let total_shares_issued_for_group = <<T as Trait>::Organization as WeightedShareWrapper<
//             u32,
//             u32,
//             T::AccountId,
//         >>::get_outstanding_weighted_shares(
//             org_share_id.0, org_share_id.1
//         )?;
//         // + 1 constant time map lookup
//         let shares_owned_by_member = <<T as Trait>::Organization as WeightedShareWrapper<
//             u32,
//             u32,
//             T::AccountId,
//         >>::get_weighted_shares_for_member(
//             org_share_id.0, org_share_id.1, &to_claimer
//         )?;
//         let ownership_portion = Permill::from_rational_approximation(
//             shares_owned_by_member,
//             total_shares_issued_for_group,
//         );
//         // note that this is only a proportion of savings, not deposits...
//         let amount_can_withdraw = ownership_portion * balance_withdrawable_by_burning_shares;
//         Ok((org_share_id.0, org_share_id.1, amount_can_withdraw))
//     }
//     // request for a portion of an on-chain deposit, the impl defines what determines the fair portion
//     fn claim_portion_of_on_chain_deposit(
//         from_bank_id: Self::TreasuryId,
//         deposit: Self::DepositInfo,
//         to_claimer: T::AccountId,
//         amount: Option<BalanceOf<T>>,
//     ) -> Result<BalanceOf<T>, DispatchError> {
//         let bank_account_dne = Self::id_is_available(from_bank_id);
//         ensure!(
//             bank_account_dne,
//             Error::<T>::CannotClaimDepositFromBankThatDNE
//         );
//         // check that they can claim some portion
//         let can_claim: BalanceOf<T> = Self::calculate_liquid_portion_of_on_chain_deposit(
//             from_bank_id,
//             deposit,
//             to_claimer.clone(),
//         )?;
//         // set the amount for withdrawal, make sure it is less than above
//         let amount_for_withdrawal = if let Some(amt) = amount {
//             ensure!(
//                 amt <= can_claim,
//                 Error::<T>::CanOnlyClaimUpToOwnershipPortionByDefault
//             );
//             amt
//         } else {
//             can_claim
//         };
//         // make withdrawal
//         let from = Self::account_id(from_bank_id);
//         T::Currency::transfer(
//             &from,
//             &to_claimer,
//             amount_for_withdrawal,
//             ExistenceRequirement::KeepAlive,
//         )?;
//         Ok(amount_for_withdrawal)
//     }
//     // irreversible decision to sell ownership in exchange for a portion of the collateral
//     // - automatically calculated according to the proportion of ownership at the time the request is processed
//     // -- NOTE: this does not shield against dilution if there is a run on the collateral because it does not yield a limit order for the share sale
//     fn withdraw_capital_by_burning_shares(
//         from_bank_id: Self::TreasuryId,
//         to_claimer: T::AccountId,
//         amount: Option<BalanceOf<T>>, // if None, burns all shares for to_claimer to liquidate as much as possible
//     ) -> Result<BalanceOf<T>, DispatchError> {
//         let bank_account_dne = Self::id_is_available(from_bank_id);
//         ensure!(
//             bank_account_dne,
//             Error::<T>::CannotClaimDepositFromBankThatDNE
//         );
//         let org_share_id_shares =
//             Self::calculate_liquid_share_capital_from_savings(from_bank_id, to_claimer.clone())?;
//         let can_withdraw = org_share_id_shares.2;
//         // if None, it burns all shares by default see last outside method call at bottom of method body
//         let mut proportion_of_own_shares_to_burn: Option<Permill> = None;
//         let amount_withdrawn = if let Some(amt) = amount {
//             ensure!(
//                 amt <= can_withdraw,
//                 Error::<T>::CannotBurnEnoughSharesToLiquidateCapitalForWithdrawalRequest
//             );
//             let proportion_of_capital_requested =
//                 Permill::from_rational_approximation(amt, can_withdraw);
//             proportion_of_own_shares_to_burn = Some(proportion_of_capital_requested);
//             amt
//         } else {
//             can_withdraw
//         };
//         // make withdrawal
//         let from = Self::account_id(from_bank_id);
//         T::Currency::transfer(
//             &from,
//             &to_claimer,
//             amount_withdrawn,
//             ExistenceRequirement::KeepAlive,
//         )?;
//         // burn proportional amount of shares
//         <<T as Trait>::Organization as WeightedShareIssuanceWrapper<
//             u32,
//             u32,
//             T::AccountId,
//             Permill,
//         >>::burn_weighted_shares_for_member(
//             org_share_id_shares.0,
//             org_share_id_shares.1,
//             to_claimer,
//             proportion_of_own_shares_to_burn,
//         )?;
//         Ok(amount_withdrawn)
//     }
// }