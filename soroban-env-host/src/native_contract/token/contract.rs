use crate::host::{metered_clone::MeteredClone, Host};
use crate::native_contract::base_types::{Address, Bytes, BytesN, String};
use crate::native_contract::contract_error::ContractError;
use crate::native_contract::token::allowance::{read_allowance, spend_allowance, write_allowance};
use crate::native_contract::token::asset_info::{has_asset_info, write_asset_info};
use crate::native_contract::token::balance::{
    is_authorized, read_balance, receive_balance, spend_balance, write_authorization,
};
use crate::native_contract::token::event;
use crate::native_contract::token::public_types::AssetInfo;
use crate::{err, HostError};

use soroban_env_common::xdr::Asset;
use soroban_env_common::{ConversionError, Env, EnvBase, TryFromVal, TryIntoVal};
use soroban_native_sdk_macros::contractimpl;

use super::admin::{read_administrator, write_administrator};
use super::asset_info::read_asset_info;
use super::balance::{
    check_clawbackable, get_spendable_balance, spend_balance_no_authorization_check,
};
use super::metadata::{read_name, read_symbol, set_metadata, DECIMAL};
use super::public_types::{AlphaNum12AssetInfo, AlphaNum4AssetInfo};
use super::storage_types::{INSTANCE_BUMP_AMOUNT, INSTANCE_LIFETIME_THRESHOLD};

pub trait TokenTrait {
    /// init_asset can create a contract for a wrapped classic asset
    /// (Native, AlphaNum4, or AlphaNum12). It will fail if the contractID
    /// of this contract does not match the expected contractID for this asset
    /// returned by Host::get_asset_contract_id_hash. This function should only be
    /// called internally by the host.
    ///
    /// No admin will be set for the Native token, so any function that checks the admin
    /// (clawback, set_auth, mint, set_admin, admin) will always fail
    fn init_asset(e: &Host, asset_bytes: Bytes) -> Result<(), HostError>;

    fn allowance(e: &Host, from: Address, spender: Address) -> Result<i128, HostError>;

    fn approve(
        e: &Host,
        from: Address,
        spender: Address,
        amount: i128,
        expiration_ledger: u32,
    ) -> Result<(), HostError>;

    fn balance(e: &Host, addr: Address) -> Result<i128, HostError>;

    fn spendable_balance(e: &Host, addr: Address) -> Result<i128, HostError>;

    fn authorized(e: &Host, addr: Address) -> Result<bool, HostError>;

    fn transfer(e: &Host, from: Address, to: Address, amount: i128) -> Result<(), HostError>;

    fn transfer_from(
        e: &Host,
        spender: Address,
        from: Address,
        to: Address,
        amount: i128,
    ) -> Result<(), HostError>;

    fn burn(e: &Host, from: Address, amount: i128) -> Result<(), HostError>;

    fn burn_from(e: &Host, spender: Address, from: Address, amount: i128) -> Result<(), HostError>;

    fn set_authorized(e: &Host, addr: Address, authorize: bool) -> Result<(), HostError>;

    fn mint(e: &Host, to: Address, amount: i128) -> Result<(), HostError>;

    fn clawback(e: &Host, from: Address, amount: i128) -> Result<(), HostError>;

    fn set_admin(e: &Host, new_admin: Address) -> Result<(), HostError>;

    fn admin(e: &Host) -> Result<Address, HostError>;

    fn decimals(e: &Host) -> Result<u32, HostError>;

    fn name(e: &Host) -> Result<String, HostError>;

    fn symbol(e: &Host) -> Result<String, HostError>;
}

pub struct Token;

fn check_nonnegative_amount(e: &Host, amount: i128) -> Result<(), HostError> {
    if amount < 0 {
        Err(err!(
            e,
            ContractError::NegativeAmountError,
            "negative amount is not allowed",
            amount
        ))
    } else {
        Ok(())
    }
}

fn check_non_native(e: &Host) -> Result<(), HostError> {
    match read_asset_info(e)? {
        AssetInfo::Native => Err(e.error(
            ContractError::OperationNotSupportedError.into(),
            "operation invalid on native asset",
            &[],
        )),
        AssetInfo::AlphaNum4(_) | AssetInfo::AlphaNum12(_) => Ok(()),
    }
}

#[contractimpl]
// Metering: *mostly* covered by components.
impl TokenTrait for Token {
    fn init_asset(e: &Host, asset_bytes: Bytes) -> Result<(), HostError> {
        let _span = tracy_span!("native token init_asset");
        if has_asset_info(e)? {
            return Err(e.error(
                ContractError::AlreadyInitializedError.into(),
                "token has been already initialized",
                &[],
            ));
        }

        let asset: Asset = e.metered_from_xdr_obj(asset_bytes.into())?;

        let curr_contract_id = e.get_current_contract_id_internal()?;
        let expected_contract_id = e.get_asset_contract_id_hash(asset.metered_clone(e)?)?;
        if curr_contract_id != expected_contract_id {
            return Err(e.error(
                ContractError::InternalError.into(),
                "bad id for asset contract",
                &[],
            ));
        }
        match asset {
            Asset::Native => {
                write_asset_info(e, AssetInfo::Native)?;
                //No admin for the Native token
            }
            Asset::CreditAlphanum4(asset4) => {
                write_administrator(e, Address::from_account(e, &asset4.issuer)?)?;
                write_asset_info(
                    e,
                    AssetInfo::AlphaNum4(AlphaNum4AssetInfo {
                        asset_code: String::try_from_val(
                            e,
                            &e.string_new_from_slice(
                                core::str::from_utf8(&asset4.asset_code.0)
                                    .map_err(|_| ConversionError)?,
                            )?,
                        )?,
                        issuer: BytesN::<32>::try_from_val(
                            e,
                            &e.bytes_new_from_slice(&e.u256_from_account(&asset4.issuer)?.0)?,
                        )?,
                    }),
                )?;
            }
            Asset::CreditAlphanum12(asset12) => {
                write_administrator(e, Address::from_account(e, &asset12.issuer)?)?;
                write_asset_info(
                    e,
                    AssetInfo::AlphaNum12(AlphaNum12AssetInfo {
                        asset_code: String::try_from_val(
                            e,
                            &e.string_new_from_slice(
                                core::str::from_utf8(&asset12.asset_code.0)
                                    .map_err(|_| ConversionError)?,
                            )?,
                        )?,
                        issuer: BytesN::<32>::try_from_val(
                            e,
                            &e.bytes_new_from_slice(&e.u256_from_account(&asset12.issuer)?.0)?,
                        )?,
                    }),
                )?;
            }
        }

        //Write metadata only after asset_info is set
        set_metadata(e)?;
        Ok(())
    }

    fn allowance(e: &Host, from: Address, spender: Address) -> Result<i128, HostError> {
        let _span = tracy_span!("native token allowance");
        e.bump_current_contract_instance_and_code(
            INSTANCE_LIFETIME_THRESHOLD.into(),
            INSTANCE_BUMP_AMOUNT.into(),
        )?;
        read_allowance(e, from, spender)
    }

    // Metering: covered by components
    fn approve(
        e: &Host,
        from: Address,
        spender: Address,
        amount: i128,
        expiration_ledger: u32,
    ) -> Result<(), HostError> {
        let _span = tracy_span!("native token approve");
        check_nonnegative_amount(e, amount)?;
        from.require_auth()?;

        e.bump_current_contract_instance_and_code(
            INSTANCE_LIFETIME_THRESHOLD.into(),
            INSTANCE_BUMP_AMOUNT.into(),
        )?;

        write_allowance(
            e,
            from.metered_clone(e)?,
            spender.metered_clone(e)?,
            amount,
            expiration_ledger,
        )?;
        event::approve(e, from, spender, amount, expiration_ledger)?;
        Ok(())
    }

    // Metering: covered by components
    fn balance(e: &Host, addr: Address) -> Result<i128, HostError> {
        let _span = tracy_span!("native token balance");
        e.bump_current_contract_instance_and_code(
            INSTANCE_LIFETIME_THRESHOLD.into(),
            INSTANCE_BUMP_AMOUNT.into(),
        )?;
        read_balance(e, addr)
    }

    fn spendable_balance(e: &Host, addr: Address) -> Result<i128, HostError> {
        let _span = tracy_span!("native token spendable balance");
        e.bump_current_contract_instance_and_code(
            INSTANCE_LIFETIME_THRESHOLD.into(),
            INSTANCE_BUMP_AMOUNT.into(),
        )?;
        get_spendable_balance(e, addr)
    }

    // Metering: covered by components
    fn authorized(e: &Host, addr: Address) -> Result<bool, HostError> {
        let _span = tracy_span!("native token authorized");
        e.bump_current_contract_instance_and_code(
            INSTANCE_LIFETIME_THRESHOLD.into(),
            INSTANCE_BUMP_AMOUNT.into(),
        )?;
        is_authorized(e, addr)
    }

    // Metering: covered by components
    fn transfer(e: &Host, from: Address, to: Address, amount: i128) -> Result<(), HostError> {
        let _span = tracy_span!("native token transfer");
        check_nonnegative_amount(e, amount)?;
        from.require_auth()?;

        e.bump_current_contract_instance_and_code(
            INSTANCE_LIFETIME_THRESHOLD.into(),
            INSTANCE_BUMP_AMOUNT.into(),
        )?;

        spend_balance(e, from.metered_clone(e)?, amount)?;
        receive_balance(e, to.metered_clone(e)?, amount)?;
        event::transfer(e, from, to, amount)?;
        Ok(())
    }

    // Metering: covered by components
    fn transfer_from(
        e: &Host,
        spender: Address,
        from: Address,
        to: Address,
        amount: i128,
    ) -> Result<(), HostError> {
        let _span = tracy_span!("native token transfer_from");
        check_nonnegative_amount(e, amount)?;
        spender.require_auth()?;

        e.bump_current_contract_instance_and_code(
            INSTANCE_LIFETIME_THRESHOLD.into(),
            INSTANCE_BUMP_AMOUNT.into(),
        )?;

        spend_allowance(e, from.metered_clone(e)?, spender, amount)?;
        spend_balance(e, from.metered_clone(e)?, amount)?;
        receive_balance(e, to.metered_clone(e)?, amount)?;
        event::transfer(e, from, to, amount)?;
        Ok(())
    }

    // Metering: covered by components
    fn burn(e: &Host, from: Address, amount: i128) -> Result<(), HostError> {
        let _span = tracy_span!("native token burn");
        check_nonnegative_amount(e, amount)?;
        check_non_native(e)?;
        from.require_auth()?;

        e.bump_current_contract_instance_and_code(
            INSTANCE_LIFETIME_THRESHOLD.into(),
            INSTANCE_BUMP_AMOUNT.into(),
        )?;

        spend_balance(e, from.metered_clone(e)?, amount)?;
        event::burn(e, from, amount)?;
        Ok(())
    }

    // Metering: covered by components
    fn burn_from(e: &Host, spender: Address, from: Address, amount: i128) -> Result<(), HostError> {
        let _span = tracy_span!("native token burn_from");
        check_nonnegative_amount(e, amount)?;
        check_non_native(e)?;
        spender.require_auth()?;

        e.bump_current_contract_instance_and_code(
            INSTANCE_LIFETIME_THRESHOLD.into(),
            INSTANCE_BUMP_AMOUNT.into(),
        )?;

        spend_allowance(e, from.metered_clone(e)?, spender, amount)?;
        spend_balance(e, from.metered_clone(e)?, amount)?;
        event::burn(e, from, amount)?;
        Ok(())
    }

    // Metering: covered by components
    fn clawback(e: &Host, from: Address, amount: i128) -> Result<(), HostError> {
        let _span = tracy_span!("native token clawback");
        check_nonnegative_amount(e, amount)?;
        check_clawbackable(e, from.metered_clone(e)?)?;
        let admin = read_administrator(e)?;
        admin.require_auth()?;

        e.bump_current_contract_instance_and_code(
            INSTANCE_LIFETIME_THRESHOLD.into(),
            INSTANCE_BUMP_AMOUNT.into(),
        )?;

        spend_balance_no_authorization_check(e, from.metered_clone(e)?, amount)?;
        event::clawback(e, admin, from, amount)?;
        Ok(())
    }

    // Metering: covered by components
    fn set_authorized(e: &Host, addr: Address, authorize: bool) -> Result<(), HostError> {
        let _span = tracy_span!("native token set_authorized");
        let admin = read_administrator(e)?;
        admin.require_auth()?;

        e.bump_current_contract_instance_and_code(
            INSTANCE_LIFETIME_THRESHOLD.into(),
            INSTANCE_BUMP_AMOUNT.into(),
        )?;

        write_authorization(e, addr.metered_clone(e)?, authorize)?;
        event::set_authorized(e, admin, addr, authorize)?;
        Ok(())
    }

    // Metering: covered by components
    fn mint(e: &Host, to: Address, amount: i128) -> Result<(), HostError> {
        let _span = tracy_span!("native token mint");
        check_nonnegative_amount(e, amount)?;
        let admin = read_administrator(e)?;
        admin.require_auth()?;

        e.bump_current_contract_instance_and_code(
            INSTANCE_LIFETIME_THRESHOLD.into(),
            INSTANCE_BUMP_AMOUNT.into(),
        )?;

        receive_balance(e, to.metered_clone(e)?, amount)?;
        event::mint(e, admin, to, amount)?;
        Ok(())
    }

    // Metering: covered by components
    fn set_admin(e: &Host, new_admin: Address) -> Result<(), HostError> {
        let _span = tracy_span!("native token set_admin");
        let admin = read_administrator(e)?;
        admin.require_auth()?;

        e.bump_current_contract_instance_and_code(
            INSTANCE_LIFETIME_THRESHOLD.into(),
            INSTANCE_BUMP_AMOUNT.into(),
        )?;

        write_administrator(e, new_admin.metered_clone(e)?)?;
        event::set_admin(e, admin, new_admin)?;
        Ok(())
    }

    fn admin(e: &Host) -> Result<Address, HostError> {
        let _span = tracy_span!("native token admin");
        read_administrator(e)
    }

    fn decimals(_e: &Host) -> Result<u32, HostError> {
        let _span = tracy_span!("native token decimals");
        // no need to load metadata since this is fixed for all SAC tokens
        Ok(DECIMAL)
    }

    fn name(e: &Host) -> Result<String, HostError> {
        let _span = tracy_span!("native token name");
        read_name(e)
    }

    fn symbol(e: &Host) -> Result<String, HostError> {
        let _span = tracy_span!("native token symbol");
        read_symbol(e)
    }
}
