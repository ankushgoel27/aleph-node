//! Implement a Dutch auction of one token for another.
//!
//! This contract will auction off units of one token (referred to as `tickets`), accepting payment
//! in another token (`reward token`). The auction keeps track of the average price over all sales
//! made (before the first auction, this number is set on contract initialization) and starts off
//! the auction at `average_price() * sale_multiplier()`. Afterwards the price decreases linearly
//! with each block until reaching `min_price()` after `auction_length()` blocks, at which point
//! the price stays at that level indefinitely.
//!
//! A user can use the `buy(max_price)` call (after issuing a `psp22::approve` for the appropriate
//! amount to the reward token contract) to accept the current price and buy one ticket. This
//! transaction will fail if the price increased to above `max_price`, for example, due to a ticket
//! getting sold.
//!
//! The admin of the contract is expected to transfer a number of reward tokens for sale
//! into this contract and then call `reset()` in the same transaction to begin the auction. Calling
//! `reset()` if an auction is already in progress.

#![cfg_attr(not(feature = "std"), no_std)]
#![feature(min_specialization)]

use ink_lang as ink;

#[ink::contract]
pub mod marketplace {
    use access_control::{roles::Role, traits::AccessControlled, ACCESS_CONTROL_PUBKEY};
    use game_token::TRANSFER_FROM_SELECTOR as TRANSFER_FROM_GAME_TOKEN_SELECTOR;
    use ink_env::{
        call::{build_call, Call, ExecutionInput, Selector},
        CallFlags,
    };
    use ink_lang::{codegen::EmitEvent, reflect::ContractEventBase};
    use ink_prelude::{format, string::String};
    use openbrush::contracts::psp22::PSP22Error;
    use ticket_token::{
        BALANCE_OF_SELECTOR as TICKET_BALANCE_SELECTOR,
        TRANSFER_SELECTOR as TRANSFER_TICKET_SELECTOR,
    };

    type Event = <Marketplace as ContractEventBase>::Type;

    const DUMMY_DATA: &[u8] = &[0x0];

    #[ink(storage)]
    pub struct Marketplace {
        total_proceeds: Balance,
        tickets_sold: Balance,
        min_price: Balance,
        current_start_block: BlockNumber,
        auction_length: BlockNumber,
        sale_multiplier: Balance,
        ticket_token: AccountId,
        reward_token: AccountId,
    }

    #[derive(Eq, PartialEq, Debug, scale::Encode, scale::Decode)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum Error {
        MissingRole(Role),
        ContractCall(String),
        PSP22TokenCall(PSP22Error),
        MaxPriceExceeded,
        MarketplaceEmpty,
    }

    #[ink(event)]
    #[derive(Clone, Eq, PartialEq, Debug)]
    pub struct Bought {
        #[ink(topic)]
        pub account_id: AccountId,
        pub price: Balance,
    }

    #[ink(event)]
    #[derive(Clone, Eq, PartialEq, Debug)]
    pub struct Reset;

    impl From<ink_env::Error> for Error {
        fn from(inner: ink_env::Error) -> Self {
            Error::ContractCall(format!("{:?}", inner))
        }
    }

    impl From<PSP22Error> for Error {
        fn from(inner: PSP22Error) -> Self {
            Error::PSP22TokenCall(inner)
        }
    }

    impl AccessControlled for Marketplace {
        type ContractError = Error;
    }

    impl Marketplace {
        #[ink(constructor)]
        pub fn new(
            ticket_token: AccountId,
            reward_token: AccountId,
            starting_price: Balance,
            min_price: Balance,
            sale_multiplier: Balance,
            auction_length: BlockNumber,
        ) -> Self {
            Self::ensure_role(Self::initializer())
                .unwrap_or_else(|e| panic!("Failed to initialize the contract {:?}", e));

            Marketplace {
                ticket_token,
                reward_token,
                min_price,
                sale_multiplier,
                auction_length,
                current_start_block: Self::env().block_number(),
                total_proceeds: starting_price.saturating_div(sale_multiplier),
                tickets_sold: 1,
            }
        }

        /// The length of each auction of a single ticket in blocks.
        ///
        /// The contract will decrease the price linearly from `average_price() * sale_multiplier()`
        /// to `min_price()` over this period. The auction doesn't end after the period elapses -
        /// the ticket remains available for purchase at `min_price()`.
        #[ink(message)]
        pub fn auction_length(&self) -> BlockNumber {
            self.auction_length
        }

        /// The block at which the auction of the current ticket started.
        #[ink(message)]
        pub fn current_start_block(&self) -> BlockNumber {
            self.current_start_block
        }

        /// The price the contract would charge when buying at the current block.
        #[ink(message)]
        pub fn price(&self) -> Balance {
            self.current_price()
        }

        /// The average price over all sales the contract made.
        #[ink(message)]
        pub fn average_price(&self) -> Balance {
            self.total_proceeds.saturating_div(self.tickets_sold)
        }

        /// The multiplier applied to the average price after each sale.
        ///
        /// The contract tracks the average price of all sold tickets and starts off each new
        /// auction at `price() = average_price() * sale_multiplier()`.
        #[ink(message)]
        pub fn sale_multiplier(&self) -> Balance {
            self.sale_multiplier
        }

        /// Number of tickets available for sale.
        ///
        /// The tickets will be auctioned off one by one.
        #[ink(message)]
        pub fn available_tickets(&self) -> Result<Balance, Error> {
            self.ticket_balance()
        }

        /// The minimal price the contract allows.
        #[ink(message)]
        pub fn min_price(&self) -> Balance {
            self.min_price
        }

        /// Update the minimal price.
        #[ink(message)]
        pub fn set_min_price(&mut self, value: Balance) -> Result<(), Error> {
            Self::ensure_role(self.admin())?;

            self.min_price = value;

            Ok(())
        }

        /// Address of the reward token contract this contract will accept as payment.
        #[ink(message)]
        pub fn reward_token(&self) -> AccountId {
            self.reward_token
        }

        /// Address of the ticket token contract this contract will auction off.
        #[ink(message)]
        pub fn ticket_token(&self) -> AccountId {
            self.ticket_token
        }

        /// Buy one ticket at the current_price.
        ///
        /// The caller should make an approval for at least `price()` reward tokens to make sure the
        /// call will succeed. The caller can specify a `max_price` - the call will fail if the
        /// current price is greater than that.
        #[ink(message)]
        pub fn buy(&mut self, max_price: Option<Balance>) -> Result<(), Error> {
            if self.ticket_balance()? <= 0 {
                return Err(Error::MarketplaceEmpty);
            }

            let price = self.current_price();
            if let Some(max_price) = max_price {
                if price > max_price {
                    return Err(Error::MaxPriceExceeded);
                }
            }

            let account_id = self.env().caller();

            self.take_payment(account_id, price)?;
            self.give_ticket(account_id)?;

            self.total_proceeds = self.total_proceeds.saturating_add(price);
            self.tickets_sold = self.tickets_sold.saturating_add(1);
            self.current_start_block = self.env().block_number();
            Self::emit_event(self.env(), Event::Bought(Bought { price, account_id }));

            Ok(())
        }

        /// Re-start the auction from the current block.
        ///
        /// Note that this will keep the average estimate from previous auctions.
        ///
        /// Requires `Role::Admin`.
        #[ink(message)]
        pub fn reset(&mut self) -> Result<(), Error> {
            Self::ensure_role(self.admin())?;

            self.current_start_block = self.env().block_number();
            Self::emit_event(self.env(), Event::Reset(Reset {}));

            Ok(())
        }

        /// Terminates the contract
        ///
        /// Should only be called by the contract Owner
        #[ink(message)]
        pub fn terminate(&mut self) -> Result<(), Error> {
            let caller = self.env().caller();
            let this = self.env().account_id();
            Self::ensure_role(Role::Owner(this))?;
            self.env().terminate_contract(caller)
        }

        fn current_price(&self) -> Balance {
            let block = self.env().block_number();
            let elapsed = block.saturating_sub(self.current_start_block.into());
            self.average_price()
                .saturating_mul(self.sale_multiplier)
                .saturating_sub(self.per_block_reduction().saturating_mul(elapsed.into()))
                .max(self.min_price)
        }

        fn per_block_reduction(&self) -> Balance {
            self.average_price()
                .saturating_div(self.auction_length.into())
                .max(1u128)
        }

        fn take_payment(&self, from: AccountId, amount: Balance) -> Result<(), Error> {
            build_call::<Environment>()
                .call_type(Call::new().callee(self.reward_token))
                .exec_input(
                    ExecutionInput::new(Selector::new(TRANSFER_FROM_GAME_TOKEN_SELECTOR))
                        .push_arg(from)
                        .push_arg(self.env().account_id())
                        .push_arg(amount)
                        .push_arg(DUMMY_DATA),
                )
                .call_flags(CallFlags::default().set_allow_reentry(true))
                .returns::<Result<(), PSP22Error>>()
                .fire()??;

            Ok(())
        }

        fn give_ticket(&self, to: AccountId) -> Result<(), Error> {
            build_call::<Environment>()
                .call_type(Call::new().callee(self.ticket_token))
                .exec_input(
                    ExecutionInput::new(Selector::new(TRANSFER_TICKET_SELECTOR))
                        .push_arg(to)
                        .push_arg(1u128)
                        .push_arg(DUMMY_DATA),
                )
                .returns::<Result<(), PSP22Error>>()
                .fire()??;

            Ok(())
        }

        fn ticket_balance(&self) -> Result<Balance, Error> {
            let balance = build_call::<Environment>()
                .call_type(Call::new().callee(self.ticket_token))
                .exec_input(
                    ExecutionInput::new(Selector::new(TICKET_BALANCE_SELECTOR))
                        .push_arg(self.env().account_id()),
                )
                .returns::<Balance>()
                .fire()?;

            Ok(balance)
        }

        fn ensure_role(role: Role) -> Result<(), Error> {
            <Self as AccessControlled>::check_role(
                AccountId::from(ACCESS_CONTROL_PUBKEY),
                Self::env().caller(),
                role,
                |reason| reason.into(),
                |role| Error::MissingRole(role),
            )
        }

        fn initializer() -> Role {
            let code_hash = Self::env()
                .own_code_hash()
                .expect("Failure to retrieve code hash.");
            Role::Initializer(code_hash)
        }

        fn admin(&self) -> Role {
            Role::Admin(self.env().account_id())
        }

        fn emit_event<EE: EmitEvent<Self>>(emitter: EE, event: Event) {
            emitter.emit_event(event)
        }
    }
}
