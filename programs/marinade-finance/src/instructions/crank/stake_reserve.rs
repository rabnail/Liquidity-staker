use crate::{
    checks::check_address, error::MarinadeError, state::stake_system::StakeSystem, State, ID,
};
use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    log::sol_log_compute_units,
    program::{invoke, invoke_signed},
    stake::{
        self,
        state::{Authorized, Lockup, StakeState},
    },
    system_instruction,
    sysvar::stake_history,
};
use anchor_spl::stake::{Stake, StakeAccount};
use std::convert::TryFrom;
use std::ops::Deref;

#[derive(Accounts)]
pub struct StakeReserve<'info> {
    #[account(mut)]
    pub state: Box<Account<'info, State>>,
    /// CHECK: manual account processing
    #[account(mut)]
    pub validator_list: UncheckedAccount<'info>,
    /// CHECK: manual account processing
    #[account(mut)]
    pub stake_list: UncheckedAccount<'info>,
    /// CHECK: CPI
    #[account(mut)]
    pub validator_vote: UncheckedAccount<'info>,
    #[account(mut, seeds = [&state.key().to_bytes(),
            State::RESERVE_SEED],
            bump = state.reserve_bump_seed)]
    pub reserve_pda: SystemAccount<'info>,
    #[account(mut)]
    pub stake_account: Box<Account<'info, StakeAccount>>, // must be uninitialized
    /// CHECK: PDA
    #[account(seeds = [&state.key().to_bytes(),
            StakeSystem::STAKE_DEPOSIT_SEED],
            bump = state.stake_system.stake_deposit_bump_seed)]
    pub stake_deposit_authority: UncheckedAccount<'info>,

    pub clock: Sysvar<'info, Clock>,
    pub epoch_schedule: Sysvar<'info, EpochSchedule>,
    pub rent: Sysvar<'info, Rent>,
    /// CHECK: have no CPU budget to parse
    pub stake_history: UncheckedAccount<'info>,
    /// CHECK: CPI
    #[account(address = stake::config::ID)]
    pub stake_config: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
    pub stake_program: Program<'info, Stake>,
}

impl<'info> StakeReserve<'info> {
    fn check_stake_history(&self) -> Result<()> {
        if !stake_history::check_id(self.stake_history.key) {
            msg!(
                "Stake history sysvar must be {}. Got {}",
                stake_history::ID,
                self.stake_history.key
            );
            return Err(Error::from(ProgramError::InvalidArgument).with_source(source!()));
        }
        Ok(())
    }

    ///
    /// called by the bot
    /// Receives self.stake_account where to stake, normally an empty account (new keypair)
    /// stakes from available delta-stake in data.validator_index
    /// pub fn stake_reserve()
    pub fn process(&mut self, validator_index: u32) -> Result<()> {
        sol_log_compute_units();
        msg!("Stake reserve");
        self.state
            .validator_system
            .check_validator_list(&self.validator_list)?;
        self.state.stake_system.check_stake_list(&self.stake_list)?;
        self.check_stake_history()?;
        match StakeAccount::deref(&self.stake_account) {
            StakeState::Uninitialized => (),
            _ => {
                msg!("Stake {} must be uninitialized", self.stake_account.key());
                return Err(Error::from(ProgramError::InvalidAccountData).with_source(source!()));
            }
        }
        if self.stake_account.to_account_info().lamports()
            != self.rent.minimum_balance(std::mem::size_of::<StakeState>())
        {
            msg!(
                "Stake {} must have balance {} but has {} lamports",
                self.stake_account.key(),
                self.rent.minimum_balance(std::mem::size_of::<StakeState>()),
                self.stake_account.to_account_info().lamports()
            );
            return Err(Error::from(ProgramError::InvalidAccountData).with_source(source!()));
        }

        let staker = Pubkey::create_program_address(
            &[
                &self.state.key().to_bytes(),
                StakeSystem::STAKE_DEPOSIT_SEED,
                &[self.state.stake_system.stake_deposit_bump_seed],
            ],
            &ID,
        )
        .unwrap();

        let withdrawer = Pubkey::create_program_address(
            &[
                &self.state.key().to_bytes(),
                StakeSystem::STAKE_WITHDRAW_SEED,
                &[self.state.stake_system.stake_withdraw_bump_seed],
            ],
            &ID,
        )
        .unwrap();

        let stake_delta = self.state.stake_delta(self.reserve_pda.lamports());
        if stake_delta <= 0 {
            if stake_delta < 0 {
                msg!(
                    "Must unstake {} instead of staking",
                    u64::try_from(-stake_delta).expect("Stake delta overflow")
                );
            } else {
                msg!("Noting to do");
            }
            return Ok(()); // Not an error. Don't fail other instructions in tx
        }
        let stake_delta = u64::try_from(stake_delta).expect("Stake delta overflow");
        let total_stake_target = self
            .state
            .validator_system
            .total_active_balance
            .saturating_add(stake_delta);

        let mut validator = self
            .state
            .validator_system
            .get(&self.validator_list.data.as_ref().borrow(), validator_index)?;

        check_address(
            &self.validator_vote.key,
            &validator.validator_account,
            "validator_vote",
        )?;

        if validator.last_stake_delta_epoch == self.clock.epoch {
            // check if we have some extra stake runs allowed
            if self.state.stake_system.extra_stake_delta_runs == 0 {
                msg!(
                    "Double delta stake command for validator {} in epoch {}",
                    validator.validator_account,
                    self.clock.epoch
                );
                return Ok(()); // Not an error. Don't fail other instructions in tx
            } else {
                // some extra runs allowed. Use one
                self.state.stake_system.extra_stake_delta_runs -= 1;
            }
        } else {
            // first stake in this epoch
            validator.last_stake_delta_epoch = self.clock.epoch;
        }

        let last_slot = self.epoch_schedule.get_last_slot_in_epoch(self.clock.epoch);

        if self.clock.slot < last_slot.saturating_sub(self.state.stake_system.slots_for_stake_delta)
        {
            msg!(
                "Stake delta is available only last {} slots of epoch",
                self.state.stake_system.slots_for_stake_delta
            );
            return Err(Error::from(ProgramError::Custom(332)).with_source(source!()));
        }

        let validator_stake_target = self
            .state
            .validator_system
            .validator_stake_target(&validator, total_stake_target)?;

        //verify the validator is under-staked
        if validator.active_balance >= validator_stake_target {
            msg!(
                    "Validator {} has already reached stake target {}. Please stake into another validator",
                    validator.validator_account,
                    validator_stake_target
                );
            return Ok(()); // Not an error. Don't fail other instructions in tx
        }

        // compute stake_target
        // stake_target = target_validator_balance - validator.balance, at least self.state.min_stake and at most delta_stake
        let stake_target = validator_stake_target
            .saturating_sub(validator.active_balance)
            .max(self.state.stake_system.min_stake)
            .min(stake_delta);

        // if what's left after this stake is < state.min_stake, take all the remainder
        let stake_target = if stake_delta - stake_target < self.state.stake_system.min_stake {
            stake_delta
        } else {
            stake_target
        };

        // transfer SOL from reserve_pda to the stake-account
        sol_log_compute_units();
        msg!("Transfer to stake account");
        invoke_signed(
            &system_instruction::transfer(
                self.reserve_pda.key,
                &self.stake_account.key(),
                stake_target,
            ),
            &[
                self.system_program.to_account_info(),
                self.reserve_pda.to_account_info(),
                self.stake_account.to_account_info(),
            ],
            &[&[
                &self.state.key().to_bytes(),
                State::RESERVE_SEED,
                &[self.state.reserve_bump_seed],
            ]],
        )?;
        self.state.on_transfer_from_reserve(stake_target)?;

        sol_log_compute_units();
        msg!("Initialize stake");
        invoke(
            &stake::instruction::initialize(
                &self.stake_account.key(),
                &Authorized { staker, withdrawer },
                &Lockup::default(),
            ),
            &[
                self.stake_program.to_account_info(),
                self.stake_account.to_account_info(),
                self.rent.to_account_info(),
            ],
        )?;

        sol_log_compute_units();
        msg!("Delegate stake");
        invoke_signed(
            &stake::instruction::delegate_stake(
                &self.stake_account.key(),
                &staker,
                self.validator_vote.key,
            ),
            &[
                self.stake_program.to_account_info(),
                self.stake_account.to_account_info(),
                self.stake_deposit_authority.to_account_info(),
                self.validator_vote.to_account_info(),
                self.clock.to_account_info(),
                self.stake_history.to_account_info(),
                self.stake_config.to_account_info(),
            ],
            &[&[
                &self.state.key().to_bytes(),
                StakeSystem::STAKE_DEPOSIT_SEED,
                &[self.state.stake_system.stake_deposit_bump_seed],
            ]],
        )?;

        self.state.stake_system.add(
            &mut self.stake_list.data.as_ref().borrow_mut(),
            &self.stake_account.key(),
            stake_target,
            &self.clock,
            0, // is_emergency_unstaking? no
        )?;

        // self.state.epoch_stake_orders -= amount;
        validator.active_balance = validator
            .active_balance
            .checked_add(stake_target)
            .ok_or(MarinadeError::CalculationFailure)?;
        validator.last_stake_delta_epoch = self.clock.epoch;
        // Any stake-delta activity must activate stake delta mode
        self.state.stake_system.last_stake_delta_epoch = self.clock.epoch;
        self.state.validator_system.set(
            &mut self.validator_list.data.as_ref().borrow_mut(),
            validator_index,
            validator,
        )?;
        self.state.validator_system.total_active_balance = self
            .state
            .validator_system
            .total_active_balance
            .checked_add(stake_target)
            .ok_or(MarinadeError::CalculationFailure)?;
        Ok(())
    }
}
