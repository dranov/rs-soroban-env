#![allow(unused_variables)]
#![allow(dead_code)]

use core::{cell::RefCell, cmp::Ordering, fmt::Debug};
use std::rc::Rc;

use crate::{
    auth::AuthorizationManager,
    budget::{AsBudget, Budget},
    events::{diagnostic::DiagnosticLevel, Events, InternalEventsBuffer},
    host_object::{HostMap, HostObject, HostObjectType, HostVec},
    impl_bignum_host_fns_rhs_u32, impl_wrapping_obj_from_num, impl_wrapping_obj_to_num,
    num::*,
    storage::Storage,
    xdr::{
        int128_helpers, AccountId, Asset, ContractCostType, ContractEventType, ContractExecutable,
        CreateContractArgs, Duration, Hash, LedgerEntryData, PublicKey, ScAddress, ScBytes,
        ScErrorType, ScString, ScSymbol, ScVal, TimePoint,
    },
    AddressObject, Bool, BytesObject, ConversionError, Error, I128Object, I256Object, MapObject,
    StorageType, StringObject, SymbolObject, SymbolSmall, SymbolStr, TryFromVal, U128Object,
    U256Object, U32Val, U64Val, VecObject, VmCaller, VmCallerEnv, Void, I256, U256,
};

use crate::Vm;
use crate::{EnvBase, Object, Symbol, Val};

mod comparison;
mod conversion;
pub(crate) mod crypto;
mod data_helper;
mod declared_size;
pub(crate) mod error;
pub(crate) mod frame;
pub(crate) mod ledger_info_helper;
mod lifecycle;
mod mem_helper;
pub(crate) mod metered_clone;
pub(crate) mod metered_map;
pub(crate) mod metered_vector;
pub(crate) mod metered_xdr;
mod num;
mod prng;
pub use prng::{Seed, SEED_BYTES};
mod validity;
pub use error::HostError;
use soroban_env_common::xdr::{ContractIdPreimage, ContractIdPreimageFromAddress, ScErrorCode};

use self::{
    frame::{Context, ContractReentryMode},
    prng::Prng,
};
use self::{
    metered_clone::{MeteredClone, MeteredContainer},
    metered_xdr::metered_write_xdr,
};
use crate::impl_bignum_host_fns;
use crate::Compare;
#[cfg(any(test, feature = "testutils"))]
pub use frame::ContractFunctionSet;
pub(crate) use frame::Frame;

/// Defines the maximum depth for recursive calls in the host, i.e. `Val` conversion, comparison,
/// and deep clone, to prevent stack overflow.
///
/// Similar to the `xdr::DEFAULT_XDR_RW_DEPTH_LIMIT`, `DEFAULT_HOST_DEPTH_LIMIT` is also a proxy
/// to the stack depth limit, and its purpose is to prevent the program from
/// hitting the maximum stack size allowed by Rust, which would result in an unrecoverable `SIGABRT`.
///
/// The difference is the `DEFAULT_HOST_DEPTH_LIMIT`guards the recursion paths via the `Env` and
/// the `Budget`, i.e., conversion, comparison and deep clone. The limit is checked at specific
/// points of the recursion path, e.g. when `Val` is encountered, to minimize noise. So the
/// "actual stack depth"/"host depth" factor will typically be larger, and thus the
/// `DEFAULT_HOST_DEPTH_LIMIT` here is set to a smaller value.
pub const DEFAULT_HOST_DEPTH_LIMIT: u32 = 100;

/// Temporary helper for denoting a slice of guest memory, as formed by
/// various bytes operations.
pub(crate) struct VmSlice {
    vm: Rc<Vm>,
    pos: u32,
    len: u32,
}

#[derive(Debug, Clone, Default)]
pub struct LedgerInfo {
    pub protocol_version: u32,
    pub sequence_number: u32,
    pub timestamp: u64,
    pub network_id: [u8; 32],
    pub base_reserve: u32,
    pub min_temp_entry_expiration: u32,
    pub min_persistent_entry_expiration: u32,
    pub max_entry_expiration: u32,
}

#[derive(Clone, Default)]
struct HostImpl {
    source_account: RefCell<Option<AccountId>>,
    ledger: RefCell<Option<LedgerInfo>>,
    objects: RefCell<Vec<HostObject>>,
    storage: RefCell<Storage>,
    context: RefCell<Vec<Context>>,
    // Note: budget is refcounted and is _not_ deep-cloned when you call HostImpl::deep_clone,
    // mainly because it's not really possible to achieve (the same budget is connected to many
    // metered sub-objects) but also because it's plausible that the person calling deep_clone
    // actually wants their clones to be metered by "the same" total budget
    // FIXME: deep_clone is gone, maybe Budget should not be separately refcounted?
    budget: Budget,
    events: RefCell<InternalEventsBuffer>,
    authorization_manager: RefCell<AuthorizationManager>,
    diagnostic_level: RefCell<DiagnosticLevel>,
    base_prng: RefCell<Option<Prng>>,
    // Note: we're not going to charge metering for testutils because it's out of the scope
    // of what users will be charged for in production -- it's scaffolding for testing a contract,
    // but shouldn't be charged to the contract itself (and will never be compiled-in to
    // production hosts)
    #[cfg(any(test, feature = "testutils"))]
    contracts: RefCell<std::collections::HashMap<Hash, Rc<dyn ContractFunctionSet>>>,
    // Store a copy of the `AuthorizationManager` for the last host function
    // invocation. In order to emulate the production behavior in tests, we reset
    // authorization manager after every invocation (as it's not meant to be
    // shared between invocations).
    // This enables test-only functions that allow checking if the authorization
    // has happened or has been recorded.
    #[cfg(any(test, feature = "testutils"))]
    previous_authorization_manager: RefCell<Option<AuthorizationManager>>,
}
// Host is a newtype on Rc<HostImpl> so we can impl Env for it below.
#[derive(Clone)]
pub struct Host(Rc<HostImpl>);

#[allow(clippy::derivable_impls)]
impl Default for Host {
    fn default() -> Self {
        #[cfg(all(not(target_family = "wasm"), feature = "tracy"))]
        let _client = tracy_client::Client::start();
        Self(Default::default())
    }
}

macro_rules! impl_checked_borrow_helpers {
    ($field:ident, $t:ty, $borrow:ident, $borrow_mut:ident) => {
        impl Host {
            pub(crate) fn $borrow(&self) -> Result<std::cell::Ref<'_, $t>, HostError> {
                use crate::host::error::TryBorrowOrErr;
                self.0.$field.try_borrow_or_err_with(
                    self,
                    concat!("host.0.", stringify!($field), ".try_borrow failed"),
                )
            }
            pub(crate) fn $borrow_mut(&self) -> Result<std::cell::RefMut<'_, $t>, HostError> {
                use crate::host::error::TryBorrowOrErr;
                self.0.$field.try_borrow_mut_or_err_with(
                    self,
                    concat!("host.0.", stringify!($field), ".try_borrow_mut failed"),
                )
            }
        }
    };
}

impl_checked_borrow_helpers!(
    source_account,
    Option<AccountId>,
    try_borrow_source_account,
    try_borrow_source_account_mut
);
impl_checked_borrow_helpers!(
    ledger,
    Option<LedgerInfo>,
    try_borrow_ledger,
    try_borrow_ledger_mut
);
impl_checked_borrow_helpers!(
    objects,
    Vec<HostObject>,
    try_borrow_objects,
    try_borrow_objects_mut
);
impl_checked_borrow_helpers!(storage, Storage, try_borrow_storage, try_borrow_storage_mut);
impl_checked_borrow_helpers!(
    context,
    Vec<Context>,
    try_borrow_context,
    try_borrow_context_mut
);
impl_checked_borrow_helpers!(
    events,
    InternalEventsBuffer,
    try_borrow_events,
    try_borrow_events_mut
);
impl_checked_borrow_helpers!(
    authorization_manager,
    AuthorizationManager,
    try_borrow_authorization_manager,
    try_borrow_authorization_manager_mut
);
impl_checked_borrow_helpers!(
    diagnostic_level,
    DiagnosticLevel,
    try_borrow_diagnostic_level,
    try_borrow_diagnostic_level_mut
);
impl_checked_borrow_helpers!(
    base_prng,
    Option<Prng>,
    try_borrow_base_prng,
    try_borrow_base_prng_mut
);

#[cfg(any(test, feature = "testutils"))]
impl_checked_borrow_helpers!(contracts, std::collections::HashMap<Hash, Rc<dyn ContractFunctionSet>>, try_borrow_contracts, try_borrow_contracts_mut);

#[cfg(any(test, feature = "testutils"))]
impl_checked_borrow_helpers!(
    previous_authorization_manager,
    Option<AuthorizationManager>,
    try_borrow_previous_authorization_manager,
    try_borrow_previous_authorization_manager_mut
);

impl Debug for HostImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HostImpl(...)")
    }
}

impl Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Host({:x})", Rc::<HostImpl>::as_ptr(&self.0) as usize)
    }
}

impl Host {
    /// Constructs a new [`Host`] that will use the provided [`Storage`] for
    /// contract-data access functions such as
    /// [`Env::get_contract_data`].
    pub fn with_storage_and_budget(storage: Storage, budget: Budget) -> Self {
        #[cfg(all(not(target_family = "wasm"), feature = "tracy"))]
        let _client = tracy_client::Client::start();
        Self(Rc::new(HostImpl {
            source_account: RefCell::new(None),
            ledger: RefCell::new(None),
            objects: Default::default(),
            storage: RefCell::new(storage),
            context: Default::default(),
            budget,
            events: Default::default(),
            authorization_manager: RefCell::new(
                AuthorizationManager::new_enforcing_without_authorizations(),
            ),
            diagnostic_level: Default::default(),
            base_prng: RefCell::new(None),
            #[cfg(any(test, feature = "testutils"))]
            contracts: Default::default(),
            #[cfg(any(test, feature = "testutils"))]
            previous_authorization_manager: RefCell::new(None),
        }))
    }

    pub fn set_source_account(&self, source_account: AccountId) -> Result<(), HostError> {
        *self.try_borrow_source_account_mut()? = Some(source_account);
        Ok(())
    }

    #[cfg(any(test, feature = "testutils"))]
    pub fn remove_source_account(&self) -> Result<(), HostError> {
        *self.try_borrow_source_account_mut()? = None;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn source_account_id(&self) -> Result<Option<AccountId>, HostError> {
        Ok(self.try_borrow_source_account()?.metered_clone(self)?)
    }

    pub fn source_account_address(&self) -> Result<Option<AddressObject>, HostError> {
        if let Some(acc) = self.try_borrow_source_account()?.as_ref() {
            Ok(Some(self.add_host_object(ScAddress::Account(
                acc.metered_clone(self)?,
            ))?))
        } else {
            Ok(None)
        }
    }

    pub fn switch_to_recording_auth(&self, disable_non_root_auth: bool) -> Result<(), HostError> {
        *self.try_borrow_authorization_manager_mut()? =
            AuthorizationManager::new_recording(disable_non_root_auth);
        Ok(())
    }

    pub fn set_authorization_entries(
        &self,
        auth_entries: Vec<soroban_env_common::xdr::SorobanAuthorizationEntry>,
    ) -> Result<(), HostError> {
        let new_auth_manager = AuthorizationManager::new_enforcing(self, auth_entries)?;
        *self.try_borrow_authorization_manager_mut()? = new_auth_manager;
        Ok(())
    }

    pub fn set_base_prng_seed(&self, seed: prng::Seed) -> Result<(), HostError> {
        *self.try_borrow_base_prng_mut()? = Some(Prng::new_from_seed(seed));
        Ok(())
    }

    pub fn set_ledger_info(&self, info: LedgerInfo) -> Result<(), HostError> {
        *self.try_borrow_ledger_mut()? = Some(info);
        Ok(())
    }

    pub fn with_ledger_info<F, T>(&self, f: F) -> Result<T, HostError>
    where
        F: FnOnce(&LedgerInfo) -> Result<T, HostError>,
    {
        match self.try_borrow_ledger()?.as_ref() {
            None => Err(self.err(
                ScErrorType::Context,
                ScErrorCode::InternalError,
                "missing ledger info",
                &[],
            )),
            Some(li) => f(li),
        }
    }

    pub fn with_mut_ledger_info<F>(&self, mut f: F) -> Result<(), HostError>
    where
        F: FnMut(&mut LedgerInfo),
    {
        match self.try_borrow_ledger_mut()?.as_mut() {
            None => Err(self.err(
                ScErrorType::Context,
                ScErrorCode::InternalError,
                "missing ledger info",
                &[],
            )),
            Some(li) => {
                f(li);
                Ok(())
            }
        }
    }

    pub fn get_ledger_protocol_version(&self) -> Result<u32, HostError> {
        self.with_ledger_info(|li| Ok(li.protocol_version))
    }

    /// Helper for mutating the [`Budget`] held in this [`Host`], either to
    /// allocate it on contract creation or to deplete it on callbacks from
    /// the VM or host functions.
    pub(crate) fn with_budget<T, F>(&self, f: F) -> Result<T, HostError>
    where
        F: FnOnce(Budget) -> Result<T, HostError>,
    {
        f(self.0.budget.clone())
    }

    pub(crate) fn budget_ref(&self) -> &Budget {
        &self.0.budget
    }

    pub fn budget_cloned(&self) -> Budget {
        self.0.budget.clone()
    }

    pub fn charge_budget(&self, ty: ContractCostType, input: Option<u64>) -> Result<(), HostError> {
        self.0.budget.clone().charge(ty, input)
    }

    /// Accept a _unique_ (refcount = 1) host reference and destroy the
    /// underlying [`HostImpl`], returning its finalized components containing
    /// processing side effects  to the caller as a tuple wrapped in `Ok(...)`.
    pub fn try_finish(self) -> Result<(Storage, Events), HostError> {
        let events = self.try_borrow_events()?.externalize(&self)?;
        Rc::try_unwrap(self.0)
            .map(|host_impl| {
                let storage = host_impl.storage.into_inner();
                (storage, events)
            })
            .map_err(|_| {
                Error::from_type_and_code(ScErrorType::Context, ScErrorCode::InternalError).into()
            })
    }

    // Testing interface to create values directly for later use via Env functions.
    // It needs to be a `pub` method because benches are considered a separate crate.
    #[cfg(any(test, feature = "testutils"))]
    pub fn inject_val(&self, v: &ScVal) -> Result<Val, HostError> {
        self.to_host_val(v).map(Into::into)
    }

    fn symbol_matches(&self, s: &[u8], sym: Symbol) -> Result<bool, HostError> {
        if let Ok(ss) = SymbolSmall::try_from(sym) {
            let sstr: SymbolStr = ss.into();
            let slice: &[u8] = sstr.as_ref();
            self.as_budget()
                .compare(&slice, &s)
                .map(|c| c == Ordering::Equal)
        } else {
            let sobj: SymbolObject = sym.try_into()?;
            self.visit_obj(sobj, |scsym: &ScSymbol| {
                self.as_budget()
                    .compare(&scsym.as_slice(), &s)
                    .map(|c| c == Ordering::Equal)
            })
        }
    }

    fn check_symbol_matches(&self, s: &[u8], sym: Symbol) -> Result<(), HostError> {
        if self.symbol_matches(s, sym)? {
            Ok(())
        } else {
            Err(self.err(
                ScErrorType::Value,
                ScErrorCode::InvalidInput,
                "symbol mismatch",
                &[sym.to_val()],
            ))
        }
    }
}

// Notes on metering: these are called from the guest and thus charged on the VM instructions.
impl EnvBase for Host {
    type Error = HostError;

    fn error_from_error_val(&self, e: soroban_env_common::Error) -> Self::Error {
        self.error(e, "promoting Error to HostError", &[])
    }

    // This function is somewhat subtle.
    //
    // It exists to allow the client of the (VmCaller)Env interface(s) to
    // essentially _reject_ an error returned by one of the Result-returning
    // methods on the trait, choosing to panic instead. But doing so in some way
    // that the trait defines, rather than calling panic in the client.
    //
    // The only client we expect to _do_ this is a non-builtin user contract
    // compiled natively for local testing (and thus linked directly to `Host`).
    // In a wasm build of a user contract, we already encourage users to think
    // of `Env::Error` as infallible by literally defining `Guest::Error` as the
    // `Infallible` type (which makes sense: we trap the user's VM on such
    // errors, don't resume it at all). But in a non-wasm, native build of a
    // user contract, `Env=Host` and `Env::Error=HostError`, an inhabited type
    // you can observe. So the user might actually have a code path returning
    // from such an error that is suddenly non-dead and receiving an
    // `Env::Error=HostError`, which (to maintain continuity with the VM case)
    // they then _want_ to treat as impossible-to-have-occurred just like
    // `Guest::Error`. They can panic, but that doesn't quite maintain the
    // illusion properly. Instead they should call this method to "reject the
    // error".
    //
    // When such a "rejected error" occurs, we do panic, but only after checking
    // to see if we're in a `TestContract` invocation, and if so storing the
    // error's Error value in that frame, such that `Host::call_n` can recover
    // the Error when it _catches_ the panic and converts it back to an error.
    //
    // It might seem like we ought to `std::panic::panic_any(e)` here, making
    // the panic carry a `HostError` or `Error` and catching it by dynamic type
    // inspection in the `call_n` catch logic. The reason we don't do so is that
    // `panic_any` will not provide a nice printable value to the `PanicInfo`,
    // it constructs, so when/if the panic makes it to a top-level printout it
    // will display a relatively ugly message like "thread panicked at Box<dyn
    // Any>" to stderr, when it is much more useful to the user if we have it
    // print the result of HostError::Debug, with its glorious Error,
    // site-of-origin backtrace and debug log.
    //
    // To get it to do that, we have to call `panic!()`, not `panic_any`.
    // Personally I think this is a glaring weakness of `panic_any` but we are
    // not in a position to improve it.
    #[cfg(feature = "testutils")]
    fn escalate_error_to_panic(&self, e: Self::Error) -> ! {
        let _ = self.with_current_frame_opt(|f| {
            if let Some(Frame::TestContract(frame)) = f {
                if let Ok(mut panic) = frame.panic.try_borrow_mut() {
                    *panic = Some(e.error);
                }
            }
            Ok(())
        });
        let escalation = self.error(e.error, "escalating error to panic", &[]);
        panic!("{:?}", escalation)
    }

    fn augment_err_result<T>(&self, mut x: Result<T, Self::Error>) -> Result<T, Self::Error> {
        if let Err(e) = &mut x {
            if e.info.is_none() {
                e.info = self.maybe_get_debug_info()
            }
        }
        x
    }

    fn check_same_env(&self, other: &Self) -> Result<(), Self::Error> {
        if Rc::ptr_eq(&self.0, &other.0) {
            Ok(())
        } else {
            Err(self.err(
                ScErrorType::Context,
                ScErrorCode::InternalError,
                "check_same_env on different Hosts",
                &[],
            ))
        }
    }

    fn bytes_copy_from_slice(
        &self,
        b: BytesObject,
        b_pos: U32Val,
        slice: &[u8],
    ) -> Result<BytesObject, HostError> {
        self.memobj_copy_from_slice::<ScBytes>(b, b_pos, slice)
    }

    fn bytes_copy_to_slice(
        &self,
        b: BytesObject,
        b_pos: U32Val,
        slice: &mut [u8],
    ) -> Result<(), HostError> {
        self.memobj_copy_to_slice::<ScBytes>(b, b_pos, slice)
    }

    fn string_copy_to_slice(
        &self,
        b: StringObject,
        b_pos: U32Val,
        slice: &mut [u8],
    ) -> Result<(), HostError> {
        self.memobj_copy_to_slice::<ScString>(b, b_pos, slice)
    }

    fn symbol_copy_to_slice(
        &self,
        s: SymbolObject,
        b_pos: U32Val,
        slice: &mut [u8],
    ) -> Result<(), HostError> {
        self.memobj_copy_to_slice::<ScSymbol>(s, b_pos, slice)
    }

    fn bytes_new_from_slice(&self, mem: &[u8]) -> Result<BytesObject, HostError> {
        self.add_host_object(self.scbytes_from_slice(mem)?)
    }

    fn string_new_from_slice(&self, s: &str) -> Result<StringObject, HostError> {
        self.add_host_object(ScString(
            self.metered_slice_to_vec(s.as_bytes())?.try_into()?,
        ))
    }

    fn symbol_new_from_slice(&self, s: &str) -> Result<SymbolObject, HostError> {
        self.charge_budget(ContractCostType::HostMemCmp, Some(s.len() as u64))?;
        for ch in s.chars() {
            SymbolSmall::validate_char(ch)?;
        }
        self.add_host_object(ScSymbol(
            self.metered_slice_to_vec(s.as_bytes())?.try_into()?,
        ))
    }

    fn map_new_from_slices(&self, keys: &[&str], vals: &[Val]) -> Result<MapObject, HostError> {
        if keys.len() != vals.len() {
            return Err(self.err(
                ScErrorType::Object,
                ScErrorCode::UnexpectedSize,
                "differing key and value slice lengths when creating map from slices",
                &[],
            ));
        }
        Vec::<(Val, Val)>::charge_bulk_init_cpy(keys.len() as u64, self)?;
        let map_vec = keys
            .iter()
            .zip(vals.iter().copied())
            .map(|(key_str, val)| {
                let sym = Symbol::try_from_val(self, key_str)?;
                self.check_val_integrity(val)?;
                Ok((sym.to_val(), val))
            })
            .collect::<Result<Vec<(Val, Val)>, HostError>>()?;
        let map = HostMap::from_map(map_vec, self)?;
        self.add_host_object(map)
    }

    fn map_unpack_to_slice(
        &self,
        map: MapObject,
        keys: &[&str],
        vals: &mut [Val],
    ) -> Result<Void, HostError> {
        if keys.len() != vals.len() {
            return Err(self.err(
                ScErrorType::Object,
                ScErrorCode::UnexpectedSize,
                "differing key and value slice lengths when unpacking map to slice",
                &[],
            ));
        }
        self.visit_obj(map, |hm: &HostMap| {
            if hm.len() != vals.len() {
                return Err(self.err(
                    ScErrorType::Object,
                    ScErrorCode::UnexpectedSize,
                    "differing host map and output slice lengths when unpacking map to slice",
                    &[],
                ));
            }

            for (ik, mk) in keys.iter().zip(hm.keys(self)?) {
                let sym: Symbol = mk.try_into()?;
                self.check_symbol_matches(ik.as_bytes(), sym)?;
            }

            metered_clone::charge_shallow_copy::<Val>(keys.len() as u64, self)?;
            for (iv, mv) in vals.iter_mut().zip(hm.values(self)?) {
                *iv = *mv;
            }
            Ok(())
        })?;
        Ok(Val::VOID)
    }

    fn vec_new_from_slice(&self, vals: &[Val]) -> Result<VecObject, Self::Error> {
        let vec = HostVec::from_exact_iter(vals.iter().cloned(), self.budget_ref())?;
        for v in vec.iter() {
            self.check_val_integrity(*v)?;
        }
        self.add_host_object(vec)
    }

    fn vec_unpack_to_slice(&self, vec: VecObject, vals: &mut [Val]) -> Result<Void, Self::Error> {
        self.visit_obj(vec, |hv: &HostVec| {
            if hv.len() != vals.len() {
                return Err(self.err(
                    ScErrorType::Object,
                    ScErrorCode::UnexpectedSize,
                    "differing host vector and output vector lengths when unpacking vec to slice",
                    &[],
                ));
            }
            metered_clone::charge_shallow_copy::<Val>(hv.len() as u64, self)?;
            vals.copy_from_slice(hv.as_slice());
            Ok(())
        })?;
        Ok(Val::VOID)
    }

    fn symbol_index_in_strs(&self, sym: Symbol, slices: &[&str]) -> Result<U32Val, Self::Error> {
        let mut found = None;
        self.scan_slice_of_slices(slices, |i, slice| {
            if self.symbol_matches(slice.as_bytes(), sym)? && found.is_none() {
                found = Some(i)
            }
            Ok(())
        })?;
        match found {
            None => Err(self.err(
                ScErrorType::Value,
                ScErrorCode::InvalidInput,
                "symbol not found in slice of strs",
                &[sym.to_val()],
            )),
            Some(idx) => Ok(U32Val::from(self.usize_to_u32(idx)?)),
        }
    }

    fn log_from_slice(&self, msg: &str, vals: &[Val]) -> Result<Void, HostError> {
        self.log_diagnostics(msg, vals).map(|_| Void::from(()))
    }
}

impl VmCallerEnv for Host {
    type VmUserState = Host;

    // region: "context" module functions

    // Notes on metering: covered by the components
    fn log_from_linear_memory(
        &self,
        vmcaller: &mut VmCaller<Host>,
        msg_pos: U32Val,
        msg_len: U32Val,
        vals_pos: U32Val,
        vals_len: U32Val,
    ) -> Result<Void, HostError> {
        if self.is_debug()? {
            // FIXME: change to a "debug budget" https://github.com/stellar/rs-soroban-env/issues/1061
            self.as_budget().with_free_budget(|| {
                let VmSlice { vm, pos, len } = self.decode_vmslice(msg_pos, msg_len)?;
                let mut msg: Vec<u8> = vec![0u8; len as usize];
                self.metered_vm_read_bytes_from_linear_memory(vmcaller, &vm, pos, &mut msg)?;
                let msg = String::from_utf8_lossy(&msg);

                let VmSlice { vm, pos, len } = self.decode_vmslice(vals_pos, vals_len)?;
                let mut vals: Vec<Val> = vec![Val::VOID.to_val(); len as usize];
                self.metered_vm_read_vals_from_linear_memory::<8, Val>(
                    vmcaller,
                    &vm,
                    pos,
                    vals.as_mut_slice(),
                    |buf| self.relative_to_absolute(Val::from_payload(u64::from_le_bytes(*buf))),
                )?;

                self.log_diagnostics(&msg, &vals)
            })?;
        }
        Ok(Val::VOID)
    }

    // Metered: covered by `visit`.
    fn obj_cmp(&self, _vmcaller: &mut VmCaller<Host>, a: Val, b: Val) -> Result<i64, HostError> {
        self.check_val_integrity(a)?;
        self.check_val_integrity(b)?;
        let res = match {
            match (Object::try_from(a), Object::try_from(b)) {
                // We were given two objects: compare them.
                (Ok(a), Ok(b)) => self.visit_obj_untyped(a, |ao| {
                    // They might each be None but that's ok, None compares less than Some.
                    self.visit_obj_untyped(b, |bo| Ok(Some(self.compare(&ao, &bo)?)))
                })?,

                // We were given an object and a non-object: try a small-value comparison.
                (Ok(a), Err(_)) => self
                    .visit_obj_untyped(a, |aobj| aobj.try_compare_to_small(self.as_budget(), b))?,
                // Same as previous case, but reversing the resulting order.
                (Err(_), Ok(b)) => self.visit_obj_untyped(b, |bobj| {
                    let ord = bobj.try_compare_to_small(self.as_budget(), a)?;
                    Ok(match ord {
                        Some(Ordering::Less) => Some(Ordering::Greater),
                        Some(Ordering::Greater) => Some(Ordering::Less),
                        other => other,
                    })
                })?,
                // We should have been given at least one object.
                (Err(_), Err(_)) => {
                    return Err(self.err(
                        ScErrorType::Value,
                        ScErrorCode::UnexpectedType,
                        "two non-object args to obj_cmp",
                        &[a, b],
                    ))
                }
            }
        } {
            // If any of the above got us a result, great, use it.
            Some(res) => res,

            // Otherwise someone gave us an object and a non-paired value (not a small-value
            // case of the same type). Order these by their ScValType.
            None => {
                let atype = a.get_tag().get_scval_type();
                let btype = b.get_tag().get_scval_type();
                if atype == btype {
                    // This shouldn't have happened, but if it does there's a logic error.
                    return Err(self.err(
                        ScErrorType::Value,
                        ScErrorCode::InternalError,
                        "equal-tagged values rejected by small-value obj_cmp",
                        &[a, b],
                    ));
                }
                atype.cmp(&btype)
            }
        };
        // Finally, translate Ordering::Foo to a number to return to caller.
        Ok(match res {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        })
    }

    fn contract_event(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        topics: VecObject,
        data: Val,
    ) -> Result<Void, HostError> {
        self.check_val_integrity(data)?;
        self.record_contract_event(ContractEventType::Contract, topics, data)?;
        Ok(Val::VOID)
    }

    fn get_ledger_version(&self, _vmcaller: &mut VmCaller<Host>) -> Result<U32Val, Self::Error> {
        Ok(self.get_ledger_protocol_version()?.into())
    }

    fn get_ledger_sequence(&self, _vmcaller: &mut VmCaller<Host>) -> Result<U32Val, Self::Error> {
        self.with_ledger_info(|li| Ok(li.sequence_number.into()))
    }

    fn get_ledger_timestamp(&self, _vmcaller: &mut VmCaller<Host>) -> Result<U64Val, Self::Error> {
        self.with_ledger_info(|li| Ok(U64Val::try_from_val(self, &li.timestamp)?))
    }

    fn fail_with_error(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        error: Error,
    ) -> Result<Void, Self::Error> {
        if error.is_type(ScErrorType::Contract) {
            Err(self.error(
                error,
                "failing with contract error",
                &[U32Val::from(error.get_code()).to_val()],
            ))
        } else {
            Err(self.err(
                ScErrorType::Context,
                ScErrorCode::UnexpectedType,
                "contract attempted to fail with non-ContractError error code",
                &[error.to_val()],
            ))
        }
    }

    fn get_ledger_network_id(
        &self,
        _vmcaller: &mut VmCaller<Host>,
    ) -> Result<BytesObject, Self::Error> {
        self.with_ledger_info(|li| {
            // FIXME: cache this and a few other such IDs: https://github.com/stellar/rs-soroban-env/issues/681
            self.add_host_object(self.scbytes_from_slice(li.network_id.as_slice())?)
        })
    }

    // Notes on metering: covered by the components.
    fn get_current_contract_address(
        &self,
        _vmcaller: &mut VmCaller<Host>,
    ) -> Result<AddressObject, HostError> {
        // FIXME: cache this and a few other such IDs: https://github.com/stellar/rs-soroban-env/issues/681
        self.add_host_object(ScAddress::Contract(
            self.get_current_contract_id_internal()?,
        ))
    }

    fn get_max_expiration_ledger(
        &self,
        _vmcaller: &mut VmCaller<Host>,
    ) -> Result<U32Val, Self::Error> {
        Ok(self.max_expiration_ledger()?.into())
    }

    // endregion "context" module functions

    // region: "int" module functions

    impl_wrapping_obj_from_num!(obj_from_u64, u64, u64);
    impl_wrapping_obj_to_num!(obj_to_u64, u64, u64);
    impl_wrapping_obj_from_num!(obj_from_i64, i64, i64);
    impl_wrapping_obj_to_num!(obj_to_i64, i64, i64);
    impl_wrapping_obj_from_num!(timepoint_obj_from_u64, TimePoint, u64);
    impl_wrapping_obj_to_num!(timepoint_obj_to_u64, TimePoint, u64);
    impl_wrapping_obj_from_num!(duration_obj_from_u64, Duration, u64);
    impl_wrapping_obj_to_num!(duration_obj_to_u64, Duration, u64);

    fn obj_from_u128_pieces(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        hi: u64,
        lo: u64,
    ) -> Result<U128Object, Self::Error> {
        self.add_host_object(int128_helpers::u128_from_pieces(hi, lo))
    }

    fn obj_to_u128_lo64(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        obj: U128Object,
    ) -> Result<u64, Self::Error> {
        self.visit_obj(obj, |u: &u128| Ok(int128_helpers::u128_lo(*u)))
    }

    fn obj_to_u128_hi64(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        obj: U128Object,
    ) -> Result<u64, Self::Error> {
        self.visit_obj(obj, |u: &u128| Ok(int128_helpers::u128_hi(*u)))
    }

    fn obj_from_i128_pieces(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        hi: i64,
        lo: u64,
    ) -> Result<I128Object, Self::Error> {
        self.add_host_object(int128_helpers::i128_from_pieces(hi, lo))
    }

    fn obj_to_i128_lo64(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        obj: I128Object,
    ) -> Result<u64, Self::Error> {
        self.visit_obj(obj, |i: &i128| Ok(int128_helpers::i128_lo(*i)))
    }

    fn obj_to_i128_hi64(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        obj: I128Object,
    ) -> Result<i64, Self::Error> {
        self.visit_obj(obj, |i: &i128| Ok(int128_helpers::i128_hi(*i)))
    }

    fn obj_from_u256_pieces(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        hi_hi: u64,
        hi_lo: u64,
        lo_hi: u64,
        lo_lo: u64,
    ) -> Result<U256Object, Self::Error> {
        self.add_host_object(u256_from_pieces(hi_hi, hi_lo, lo_hi, lo_lo))
    }

    fn u256_val_from_be_bytes(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        bytes: BytesObject,
    ) -> Result<U256Val, HostError> {
        let num = self.visit_obj(bytes, |b: &ScBytes| {
            Ok(U256::from_be_bytes(self.fixed_length_bytes_from_slice(
                "U256 bytes",
                b.as_slice(),
            )?))
        })?;
        self.map_err(U256Val::try_from_val(self, &num))
    }

    fn u256_val_to_be_bytes(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        val: U256Val,
    ) -> Result<BytesObject, HostError> {
        if let Ok(so) = U256Small::try_from(val) {
            self.add_host_object(self.scbytes_from_slice(&U256::from(so).to_be_bytes())?)
        } else {
            let obj = val.try_into()?;
            let scb = self.visit_obj(obj, |u: &U256| self.scbytes_from_slice(&u.to_be_bytes()))?;
            self.add_host_object(scb)
        }
    }

    fn obj_to_u256_hi_hi(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        obj: U256Object,
    ) -> Result<u64, HostError> {
        self.visit_obj(obj, |u: &U256| {
            let (hi_hi, _, _, _) = u256_into_pieces(*u);
            Ok(hi_hi)
        })
    }

    fn obj_to_u256_hi_lo(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        obj: U256Object,
    ) -> Result<u64, HostError> {
        self.visit_obj(obj, |u: &U256| {
            let (_, hi_lo, _, _) = u256_into_pieces(*u);
            Ok(hi_lo)
        })
    }

    fn obj_to_u256_lo_hi(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        obj: U256Object,
    ) -> Result<u64, HostError> {
        self.visit_obj(obj, |u: &U256| {
            let (_, _, lo_hi, _) = u256_into_pieces(*u);
            Ok(lo_hi)
        })
    }

    fn obj_to_u256_lo_lo(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        obj: U256Object,
    ) -> Result<u64, HostError> {
        self.visit_obj(obj, |u: &U256| {
            let (_, _, _, lo_lo) = u256_into_pieces(*u);
            Ok(lo_lo)
        })
    }

    fn obj_from_i256_pieces(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        hi_hi: i64,
        hi_lo: u64,
        lo_hi: u64,
        lo_lo: u64,
    ) -> Result<I256Object, Self::Error> {
        self.add_host_object(i256_from_pieces(hi_hi, hi_lo, lo_hi, lo_lo))
    }

    fn i256_val_from_be_bytes(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        bytes: BytesObject,
    ) -> Result<I256Val, HostError> {
        let num = self.visit_obj(bytes, |b: &ScBytes| {
            Ok(I256::from_be_bytes(self.fixed_length_bytes_from_slice(
                "I256 bytes",
                b.as_slice(),
            )?))
        })?;
        I256Val::try_from_val(self, &num).map_err(|_| ConversionError.into())
    }

    fn i256_val_to_be_bytes(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        val: I256Val,
    ) -> Result<BytesObject, HostError> {
        if let Ok(so) = I256Small::try_from(val) {
            self.add_host_object(self.scbytes_from_slice(&I256::from(so).to_be_bytes())?)
        } else {
            let obj = val.try_into()?;
            let scb = self.visit_obj(obj, |i: &I256| self.scbytes_from_slice(&i.to_be_bytes()))?;
            self.add_host_object(scb)
        }
    }

    fn obj_to_i256_hi_hi(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        obj: I256Object,
    ) -> Result<i64, HostError> {
        self.visit_obj(obj, |i: &I256| {
            let (hi_hi, _, _, _) = i256_into_pieces(*i);
            Ok(hi_hi)
        })
    }

    fn obj_to_i256_hi_lo(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        obj: I256Object,
    ) -> Result<u64, HostError> {
        self.visit_obj(obj, |i: &I256| {
            let (_, hi_lo, _, _) = i256_into_pieces(*i);
            Ok(hi_lo)
        })
    }

    fn obj_to_i256_lo_hi(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        obj: I256Object,
    ) -> Result<u64, HostError> {
        self.visit_obj(obj, |i: &I256| {
            let (_, _, lo_hi, _) = i256_into_pieces(*i);
            Ok(lo_hi)
        })
    }

    fn obj_to_i256_lo_lo(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        obj: I256Object,
    ) -> Result<u64, HostError> {
        self.visit_obj(obj, |i: &I256| {
            let (_, _, _, lo_lo) = i256_into_pieces(*i);
            Ok(lo_lo)
        })
    }

    impl_bignum_host_fns!(u256_add, checked_add, U256, U256Val, Int256AddSub);
    impl_bignum_host_fns!(u256_sub, checked_sub, U256, U256Val, Int256AddSub);
    impl_bignum_host_fns!(u256_mul, checked_mul, U256, U256Val, Int256Mul);
    impl_bignum_host_fns!(u256_div, checked_div, U256, U256Val, Int256Div);
    impl_bignum_host_fns_rhs_u32!(u256_pow, checked_pow, U256, U256Val, Int256Pow);
    impl_bignum_host_fns_rhs_u32!(u256_shl, checked_shl, U256, U256Val, Int256Shift);
    impl_bignum_host_fns_rhs_u32!(u256_shr, checked_shr, U256, U256Val, Int256Shift);

    impl_bignum_host_fns!(i256_add, checked_add, I256, I256Val, Int256AddSub);
    impl_bignum_host_fns!(i256_sub, checked_sub, I256, I256Val, Int256AddSub);
    impl_bignum_host_fns!(i256_mul, checked_mul, I256, I256Val, Int256Mul);
    impl_bignum_host_fns!(i256_div, checked_div, I256, I256Val, Int256Div);
    impl_bignum_host_fns_rhs_u32!(i256_pow, checked_pow, I256, I256Val, Int256Pow);
    impl_bignum_host_fns_rhs_u32!(i256_shl, checked_shl, I256, I256Val, Int256Shift);
    impl_bignum_host_fns_rhs_u32!(i256_shr, checked_shr, I256, I256Val, Int256Shift);

    // endregion "int" module functions
    // region: "map" module functions

    fn map_new(&self, _vmcaller: &mut VmCaller<Host>) -> Result<MapObject, HostError> {
        self.add_host_object(HostMap::new())
    }

    fn map_put(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        m: MapObject,
        k: Val,
        v: Val,
    ) -> Result<MapObject, HostError> {
        self.check_val_integrity(k)?;
        self.check_val_integrity(v)?;
        let mnew = self.visit_obj(m, |hm: &HostMap| hm.insert(k, v, self))?;
        self.add_host_object(mnew)
    }

    fn map_get(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        m: MapObject,
        k: Val,
    ) -> Result<Val, HostError> {
        self.check_val_integrity(k)?;
        self.visit_obj(m, |hm: &HostMap| {
            hm.get(&k, self)?.copied().ok_or_else(|| {
                self.err(
                    ScErrorType::Object,
                    ScErrorCode::MissingValue,
                    "map key not found in map_get",
                    &[m.to_val(), k],
                )
            })
        })
    }

    fn map_del(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        m: MapObject,
        k: Val,
    ) -> Result<MapObject, HostError> {
        self.check_val_integrity(k)?;
        match self.visit_obj(m, |hm: &HostMap| hm.remove(&k, self))? {
            Some((mnew, _)) => Ok(self.add_host_object(mnew)?),
            None => Err(self.err(
                ScErrorType::Object,
                ScErrorCode::MissingValue,
                "map key not found in map_del",
                &[m.to_val(), k],
            )),
        }
    }

    fn map_len(&self, _vmcaller: &mut VmCaller<Host>, m: MapObject) -> Result<U32Val, HostError> {
        let len = self.visit_obj(m, |hm: &HostMap| Ok(hm.len()))?;
        self.usize_to_u32val(len)
    }

    fn map_has(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        m: MapObject,
        k: Val,
    ) -> Result<Bool, HostError> {
        self.check_val_integrity(k)?;
        self.visit_obj(m, |hm: &HostMap| Ok(hm.contains_key(&k, self)?.into()))
    }

    fn map_key_by_pos(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        m: MapObject,
        i: U32Val,
    ) -> Result<Val, HostError> {
        let i: u32 = i.into();
        self.visit_obj(m, |hm: &HostMap| {
            hm.get_at_index(i as usize, self).map(|r| r.0)
        })
    }

    fn map_val_by_pos(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        m: MapObject,
        i: U32Val,
    ) -> Result<Val, HostError> {
        let i: u32 = i.into();
        self.visit_obj(m, |hm: &HostMap| {
            hm.get_at_index(i as usize, self).map(|r| r.1)
        })
    }

    fn map_keys(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        m: MapObject,
    ) -> Result<VecObject, HostError> {
        let vec = self.visit_obj(m, |hm: &HostMap| {
            HostVec::from_exact_iter(hm.keys(self)?.cloned(), self.budget_ref())
        })?;
        self.add_host_object(vec)
    }

    fn map_values(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        m: MapObject,
    ) -> Result<VecObject, HostError> {
        let vec = self.visit_obj(m, |hm: &HostMap| {
            HostVec::from_exact_iter(hm.values(self)?.cloned(), self.budget_ref())
        })?;
        self.add_host_object(vec)
    }

    fn map_new_from_linear_memory(
        &self,
        vmcaller: &mut VmCaller<Host>,
        keys_pos: U32Val,
        vals_pos: U32Val,
        len: U32Val,
    ) -> Result<MapObject, HostError> {
        // Step 1: extract all key symbols.
        let VmSlice {
            vm,
            pos: keys_pos,
            len,
        } = self.decode_vmslice(keys_pos, len)?;
        Vec::<Symbol>::charge_bulk_init_cpy(len as u64, self)?;
        let mut key_syms: Vec<Symbol> = Vec::with_capacity(len as usize);
        self.metered_vm_scan_slices_in_linear_memory(
            vmcaller,
            &vm,
            keys_pos,
            len as usize,
            |n, slice| {
                self.charge_budget(ContractCostType::VmMemRead, Some(slice.len() as u64))?;
                let scsym = ScSymbol(slice.try_into()?);
                let sym = Symbol::try_from(self.to_host_val(&ScVal::Symbol(scsym))?)?;
                key_syms.push(sym);
                Ok(())
            },
        )?;

        // Step 2: extract all val Vals.
        let vals_pos: u32 = vals_pos.into();
        Vec::<Val>::charge_bulk_init_cpy(len as u64, self)?;
        let mut vals: Vec<Val> = vec![Val::VOID.into(); len as usize];
        self.metered_vm_read_vals_from_linear_memory::<8, Val>(
            vmcaller,
            &vm,
            vals_pos,
            vals.as_mut_slice(),
            |buf| self.relative_to_absolute(Val::from_payload(u64::from_le_bytes(*buf))),
        )?;
        for v in vals.iter() {
            self.check_val_integrity(*v)?;
        }

        // Step 3: turn pairs into a map.
        let pair_iter = key_syms
            .iter()
            .map(|s| s.to_val())
            .zip(vals.iter().cloned());
        let map = HostMap::from_exact_iter(pair_iter, self)?;
        self.add_host_object(map)
    }

    fn map_unpack_to_linear_memory(
        &self,
        vmcaller: &mut VmCaller<Host>,
        map: MapObject,
        keys_pos: U32Val,
        vals_pos: U32Val,
        len: U32Val,
    ) -> Result<Void, HostError> {
        let VmSlice {
            vm,
            pos: keys_pos,
            len,
        } = self.decode_vmslice(keys_pos, len)?;
        self.visit_obj(map, |mapobj: &HostMap| {
            // Step 1: check all key symbols.
            self.metered_vm_scan_slices_in_linear_memory(
                vmcaller,
                &vm,
                keys_pos,
                len as usize,
                |n, slice| {
                    let sym = Symbol::try_from(
                        mapobj
                            .map
                            .get(n)
                            .ok_or_else(|| {
                                self.err(
                                    ScErrorType::Object,
                                    ScErrorCode::IndexBounds,
                                    "vector out of bounds while unpacking map to linear memory",
                                    &[],
                                )
                            })?
                            .0,
                    )?;
                    self.check_symbol_matches(slice, sym)?;
                    Ok(())
                },
            )?;

            // Step 2: write all vals.
            self.metered_vm_write_vals_to_linear_memory(
                vmcaller,
                &vm,
                vals_pos.into(),
                mapobj.map.as_slice(),
                |pair| {
                    Ok(u64::to_le_bytes(
                        self.absolute_to_relative(pair.1)?.get_payload(),
                    ))
                },
            )?;
            Ok(())
        })?;

        Ok(Val::VOID)
    }

    // endregion "map" module functions
    // region: "vec" module functions

    fn vec_new(&self, _vmcaller: &mut VmCaller<Host>) -> Result<VecObject, HostError> {
        self.add_host_object(HostVec::new())
    }

    fn vec_put(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v: VecObject,
        i: U32Val,
        x: Val,
    ) -> Result<VecObject, HostError> {
        let i: u32 = i.into();
        self.check_val_integrity(x)?;
        let vnew = self.visit_obj(v, |hv: &HostVec| {
            self.validate_index_lt_bound(i, hv.len())?;
            hv.set(i as usize, x, self.as_budget())
        })?;
        self.add_host_object(vnew)
    }

    fn vec_get(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v: VecObject,
        i: U32Val,
    ) -> Result<Val, HostError> {
        let i: u32 = i.into();
        self.visit_obj(v, |hv: &HostVec| {
            self.validate_index_lt_bound(i, hv.len())?;
            hv.get(i as usize, self.as_budget()).map(|r| *r)
        })
    }

    fn vec_del(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v: VecObject,
        i: U32Val,
    ) -> Result<VecObject, HostError> {
        let i: u32 = i.into();
        let vnew = self.visit_obj(v, |hv: &HostVec| {
            self.validate_index_lt_bound(i, hv.len())?;
            hv.remove(i as usize, self.as_budget())
        })?;
        self.add_host_object(vnew)
    }

    fn vec_len(&self, _vmcaller: &mut VmCaller<Host>, v: VecObject) -> Result<U32Val, HostError> {
        let len = self.visit_obj(v, |hv: &HostVec| Ok(hv.len()))?;
        self.usize_to_u32val(len)
    }

    fn vec_push_front(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v: VecObject,
        x: Val,
    ) -> Result<VecObject, HostError> {
        self.check_val_integrity(x)?;
        let vnew = self.visit_obj(v, |hv: &HostVec| hv.push_front(x, self.as_budget()))?;
        self.add_host_object(vnew)
    }

    fn vec_pop_front(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v: VecObject,
    ) -> Result<VecObject, HostError> {
        let vnew = self.visit_obj(v, |hv: &HostVec| hv.pop_front(self.as_budget()))?;
        self.add_host_object(vnew)
    }

    fn vec_push_back(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v: VecObject,
        x: Val,
    ) -> Result<VecObject, HostError> {
        self.check_val_integrity(x)?;
        let vnew = self.visit_obj(v, |hv: &HostVec| hv.push_back(x, self.as_budget()))?;
        self.add_host_object(vnew)
    }

    fn vec_pop_back(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v: VecObject,
    ) -> Result<VecObject, HostError> {
        let vnew = self.visit_obj(v, |hv: &HostVec| hv.pop_back(self.as_budget()))?;
        self.add_host_object(vnew)
    }

    fn vec_front(&self, _vmcaller: &mut VmCaller<Host>, v: VecObject) -> Result<Val, HostError> {
        self.visit_obj(v, |hv: &HostVec| {
            hv.front(self.as_budget()).map(|hval| *hval)
        })
    }

    fn vec_back(&self, _vmcaller: &mut VmCaller<Host>, v: VecObject) -> Result<Val, HostError> {
        self.visit_obj(v, |hv: &HostVec| {
            hv.back(self.as_budget()).map(|hval| *hval)
        })
    }

    fn vec_insert(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v: VecObject,
        i: U32Val,
        x: Val,
    ) -> Result<VecObject, HostError> {
        let i: u32 = i.into();
        self.check_val_integrity(x)?;
        let vnew = self.visit_obj(v, |hv: &HostVec| {
            self.validate_index_le_bound(i, hv.len())?;
            hv.insert(i as usize, x, self.as_budget())
        })?;
        self.add_host_object(vnew)
    }

    fn vec_append(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v1: VecObject,
        v2: VecObject,
    ) -> Result<VecObject, HostError> {
        let vnew = self.visit_obj(v1, |hv1: &HostVec| {
            self.visit_obj(v2, |hv2: &HostVec| {
                if hv1.len() > u32::MAX as usize - hv2.len() {
                    Err(self.err_arith_overflow())
                } else {
                    hv1.append(hv2, self.as_budget())
                }
            })
        })?;
        self.add_host_object(vnew)
    }

    fn vec_slice(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v: VecObject,
        start: U32Val,
        end: U32Val,
    ) -> Result<VecObject, HostError> {
        let start: u32 = start.into();
        let end: u32 = end.into();
        let vnew = self.visit_obj(v, |hv: &HostVec| {
            let range = self.valid_range_from_start_end_bound(start, end, hv.len())?;
            hv.slice(range, self.as_budget())
        })?;
        self.add_host_object(vnew)
    }

    fn vec_first_index_of(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v: VecObject,
        x: Val,
    ) -> Result<Val, Self::Error> {
        self.check_val_integrity(x)?;
        self.visit_obj(v, |hv: &HostVec| {
            Ok(
                match hv.first_index_of(|other| self.compare(&x, other), self.as_budget())? {
                    Some(u) => self.usize_to_u32val(u)?.into(),
                    None => Val::VOID.into(),
                },
            )
        })
    }

    fn vec_last_index_of(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v: VecObject,
        x: Val,
    ) -> Result<Val, Self::Error> {
        self.check_val_integrity(x)?;
        self.visit_obj(v, |hv: &HostVec| {
            Ok(
                match hv.last_index_of(|other| self.compare(&x, other), self.as_budget())? {
                    Some(u) => self.usize_to_u32val(u)?.into(),
                    None => Val::VOID.into(),
                },
            )
        })
    }

    fn vec_binary_search(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v: VecObject,
        x: Val,
    ) -> Result<u64, Self::Error> {
        self.check_val_integrity(x)?;
        self.visit_obj(v, |hv: &HostVec| {
            let res = hv.binary_search_by(|probe| self.compare(probe, &x), self.as_budget())?;
            self.u64_from_binary_search_result(res)
        })
    }

    fn vec_new_from_linear_memory(
        &self,
        vmcaller: &mut VmCaller<Host>,
        vals_pos: U32Val,
        len: U32Val,
    ) -> Result<VecObject, HostError> {
        let VmSlice { vm, pos, len } = self.decode_vmslice(vals_pos, len)?;
        Vec::<Val>::charge_bulk_init_cpy(len as u64, self)?;
        let mut vals: Vec<Val> = vec![Val::VOID.to_val(); len as usize];
        self.metered_vm_read_vals_from_linear_memory::<8, Val>(
            vmcaller,
            &vm,
            pos,
            vals.as_mut_slice(),
            |buf| self.relative_to_absolute(Val::from_payload(u64::from_le_bytes(*buf))),
        )?;
        for v in vals.iter() {
            self.check_val_integrity(*v)?;
        }
        self.add_host_object(HostVec::from_vec(vals)?)
    }

    fn vec_unpack_to_linear_memory(
        &self,
        vmcaller: &mut VmCaller<Host>,
        vec: VecObject,
        vals_pos: U32Val,
        len: U32Val,
    ) -> Result<Void, HostError> {
        let VmSlice { vm, pos, len } = self.decode_vmslice(vals_pos, len)?;
        self.visit_obj(vec, |vecobj: &HostVec| {
            self.metered_vm_write_vals_to_linear_memory(
                vmcaller,
                &vm,
                vals_pos.into(),
                vecobj.as_slice(),
                |x| {
                    Ok(u64::to_le_bytes(
                        self.absolute_to_relative(*x)?.get_payload(),
                    ))
                },
            )
        })?;
        Ok(Val::VOID)
    }

    // endregion "vec" module functions
    // region: "ledger" module functions

    // Notes on metering: covered by components
    fn put_contract_data(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        k: Val,
        v: Val,
        t: StorageType,
    ) -> Result<Void, HostError> {
        self.check_val_integrity(k)?;
        self.check_val_integrity(v)?;
        match t {
            StorageType::Temporary | StorageType::Persistent => {
                self.put_contract_data_into_ledger(k, v, t)?
            }
            StorageType::Instance => self.with_mut_instance_storage(|s| {
                s.map = s.map.insert(k, v, self)?;
                Ok(())
            })?,
        };

        Ok(Val::VOID)
    }

    // Notes on metering: covered by components
    fn has_contract_data(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        k: Val,
        t: StorageType,
    ) -> Result<Bool, HostError> {
        self.check_val_integrity(k)?;
        let res = match t {
            StorageType::Temporary | StorageType::Persistent => {
                let key = self.storage_key_from_rawval(k, t.try_into()?)?;
                self.try_borrow_storage_mut()?
                    .has(&key, self.as_budget())
                    .map_err(|e| self.decorate_contract_data_storage_error(e, k))?
            }
            StorageType::Instance => {
                self.with_instance_storage(|s| Ok(s.map.get(&k, self)?.is_some()))?
            }
        };

        Ok(Val::from_bool(res))
    }

    // Notes on metering: covered by components
    fn get_contract_data(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        k: Val,
        t: StorageType,
    ) -> Result<Val, HostError> {
        self.check_val_integrity(k)?;
        match t {
            StorageType::Temporary | StorageType::Persistent => {
                let key = self.storage_key_from_rawval(k, t.try_into()?)?;
                let entry = self
                    .try_borrow_storage_mut()?
                    .get(&key, self.as_budget())
                    .map_err(|e| self.decorate_contract_data_storage_error(e, k))?;
                match &entry.data {
                    LedgerEntryData::ContractData(e) => Ok(self.to_host_val(&e.val)?),
                    _ => Err(self.err(
                        ScErrorType::Storage,
                        ScErrorCode::InternalError,
                        "expected contract data ledger entry",
                        &[],
                    )),
                }
            }
            StorageType::Instance => self.with_instance_storage(|s| {
                s.map
                    .get(&k, self)?
                    .ok_or_else(|| {
                        self.err(
                            ScErrorType::Storage,
                            ScErrorCode::MissingValue,
                            "key is missing from instance storage",
                            &[k],
                        )
                    })
                    .copied()
            }),
        }
    }

    // Notes on metering: covered by components
    fn del_contract_data(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        k: Val,
        t: StorageType,
    ) -> Result<Void, HostError> {
        self.check_val_integrity(k)?;
        match t {
            StorageType::Temporary | StorageType::Persistent => {
                let key = self.contract_data_key_from_rawval(k, t.try_into()?)?;
                self.try_borrow_storage_mut()?
                    .del(&key, self.as_budget())
                    .map_err(|e| self.decorate_contract_data_storage_error(e, k))?;
            }
            StorageType::Instance => {
                self.with_mut_instance_storage(|s| {
                    if let Some((new_map, _)) = s.map.remove(&k, self)? {
                        s.map = new_map;
                    }
                    Ok(())
                })?;
            }
        }

        Ok(Val::VOID)
    }

    // Notes on metering: covered by components
    fn bump_contract_data(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        k: Val,
        t: StorageType,
        low_expiration_watermark: U32Val,
        high_expiration_watermark: U32Val,
    ) -> Result<Void, HostError> {
        self.check_val_integrity(k)?;
        if matches!(t, StorageType::Instance) {
            return Err(self.err(
                ScErrorType::Storage,
                ScErrorCode::InvalidAction,
                "instance storage should be bumped via `bump_current_contract_instance_and_code` function only",
                &[],
            ))?;
        }
        let key = self.contract_data_key_from_rawval(k, t.try_into()?)?;
        self.try_borrow_storage_mut()?
            .bump(
                self,
                key,
                low_expiration_watermark.into(),
                high_expiration_watermark.into(),
            )
            .map_err(|e| self.decorate_contract_data_storage_error(e, k))?;
        Ok(Val::VOID)
    }

    fn bump_current_contract_instance_and_code(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        low_expiration_watermark: U32Val,
        high_expiration_watermark: U32Val,
    ) -> Result<Void, HostError> {
        let contract_id = self.get_current_contract_id_internal()?;
        self.bump_contract_instance_and_code_from_contract_id(
            &contract_id,
            low_expiration_watermark.into(),
            high_expiration_watermark.into(),
        )?;
        Ok(Val::VOID)
    }

    fn bump_contract_instance_and_code(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        contract: AddressObject,
        low_expiration_watermark: U32Val,
        high_expiration_watermark: U32Val,
    ) -> Result<Void, Self::Error> {
        let contract_id = self.contract_id_from_address(contract)?;
        self.bump_contract_instance_and_code_from_contract_id(
            &contract_id,
            low_expiration_watermark.into(),
            high_expiration_watermark.into(),
        )?;
        Ok(Val::VOID)
    }

    // Notes on metering: covered by the components.
    fn create_contract(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        deployer: AddressObject,
        wasm_hash: BytesObject,
        salt: BytesObject,
    ) -> Result<AddressObject, HostError> {
        let contract_id_preimage = ContractIdPreimage::Address(ContractIdPreimageFromAddress {
            address: self.visit_obj(deployer, |addr: &ScAddress| addr.metered_clone(self))?,
            salt: self.u256_from_bytesobj_input("contract_id_salt", salt)?,
        });
        let executable =
            ContractExecutable::Wasm(self.hash_from_bytesobj_input("wasm_hash", wasm_hash)?);
        let args = CreateContractArgs {
            contract_id_preimage,
            executable,
        };
        self.create_contract_internal(Some(deployer), args)
    }

    // Notes on metering: covered by the components.
    fn create_asset_contract(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        serialized_asset: BytesObject,
    ) -> Result<AddressObject, HostError> {
        let asset: Asset = self.metered_from_xdr_obj(serialized_asset)?;
        let contract_id_preimage = ContractIdPreimage::Asset(asset);
        let executable = ContractExecutable::Token;
        let args = CreateContractArgs {
            contract_id_preimage,
            executable,
        };
        // Asset contracts don't need any deployer authorization (they're tied
        // to the asset issuers instead).
        self.create_contract_internal(None, args)
    }

    // Notes on metering: covered by the components.
    fn get_contract_id(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        deployer: AddressObject,
        salt: BytesObject,
    ) -> Result<AddressObject, HostError> {
        let hash_id = self.get_contract_id_hash(deployer, salt)?;
        self.add_host_object(ScAddress::Contract(hash_id))
    }

    // Notes on metering: covered by the components.
    fn get_asset_contract_id(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        serialized_asset: BytesObject,
    ) -> Result<AddressObject, HostError> {
        let asset: Asset = self.metered_from_xdr_obj(serialized_asset)?;
        let hash_id = self.get_asset_contract_id_hash(asset)?;
        self.add_host_object(ScAddress::Contract(hash_id))
    }

    fn upload_wasm(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        wasm: BytesObject,
    ) -> Result<BytesObject, HostError> {
        let wasm_vec =
            self.visit_obj(wasm, |bytes: &ScBytes| bytes.as_vec().metered_clone(self))?;
        self.upload_contract_wasm(wasm_vec)
    }

    fn update_current_contract_wasm(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        hash: BytesObject,
    ) -> Result<Void, HostError> {
        let wasm_hash = self.hash_from_bytesobj_input("wasm_hash", hash)?;
        if !self.wasm_exists(&wasm_hash)? {
            return Err(self.err(
                ScErrorType::Storage,
                ScErrorCode::MissingValue,
                "Wasm does not exist",
                &[hash.to_val()],
            ));
        }
        let curr_contract_id = self.get_current_contract_id_internal()?;
        let key = self.contract_instance_ledger_key(&curr_contract_id)?;
        let mut instance = self.retrieve_contract_instance_from_storage(&key)?;
        let new_executable = ContractExecutable::Wasm(wasm_hash);
        self.emit_update_contract_event(&instance.executable, &new_executable)?;
        instance.executable = new_executable;
        self.store_contract_instance(instance, curr_contract_id, &key)?;
        Ok(Val::VOID)
    }

    // endregion "ledger" module functions
    // region: "call" module functions

    // Notes on metering: here covers the args unpacking. The actual VM work is changed at lower layers.
    fn call(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        contract_address: AddressObject,
        func: Symbol,
        args: VecObject,
    ) -> Result<Val, HostError> {
        let argvec = self.call_args_from_obj(args)?;
        // this is the recommended path of calling a contract, with `reentry`
        // always set `ContractReentryMode::Prohibited`
        let res = self.call_n_internal(
            &self.contract_id_from_address(contract_address)?,
            func,
            argvec.as_slice(),
            ContractReentryMode::Prohibited,
            false,
        );
        if let Err(e) = &res {
            self.error(
                e.error,
                "contract call failed",
                &[func.to_val(), args.to_val()],
            );
        }
        res
    }

    // Notes on metering: covered by the components.
    fn try_call(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        contract_address: AddressObject,
        func: Symbol,
        args: VecObject,
    ) -> Result<Val, HostError> {
        let argvec = self.call_args_from_obj(args)?;
        // this is the "loosened" path of calling a contract.
        // TODO: A `reentry` flag will be passed from `try_call` into here.
        // For now, we are passing in `ContractReentryMode::Prohibited` to disable
        // reentry.
        let res = self.call_n_internal(
            &self.contract_id_from_address(contract_address)?,
            func,
            argvec.as_slice(),
            ContractReentryMode::Prohibited,
            false,
        );
        match res {
            Ok(rv) => Ok(rv),
            Err(e) => {
                self.error(
                    e.error,
                    "contract try_call failed",
                    &[func.to_val(), args.to_val()],
                );
                // Only allow to gracefully handle the recoverable errors.
                // Non-recoverable errors should still cause guest to panic and
                // abort execution.
                if e.is_recoverable() {
                    // Pass contract errors through.
                    if e.error.is_type(ScErrorType::Contract) {
                        Ok(e.error.to_val())
                    } else {
                        // Narrow all the remaining host errors down to a single
                        // error type. We don't want to expose the granular host
                        // errors to the guest, consistently with how every
                        // other host function works. This reduces the risk of
                        // implementation being 'locked' into specific error
                        // codes due to them being exposed to the guest and
                        // hashed into blockchain.
                        // The granular error codes are still observable with
                        // diagnostic events.
                        Ok(Error::from_type_and_code(
                            ScErrorType::Context,
                            ScErrorCode::InvalidAction,
                        )
                        .to_val())
                    }
                } else {
                    Err(e)
                }
            }
        }
    }

    // endregion "call" module functions
    // region: "buf" module functions

    // Notes on metering: covered by components
    fn serialize_to_bytes(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        v: Val,
    ) -> Result<BytesObject, HostError> {
        self.check_val_integrity(v)?;
        let scv = self.from_host_val(v)?;
        let mut buf = Vec::<u8>::new();
        metered_write_xdr(self.budget_ref(), &scv, &mut buf)?;
        self.add_host_object(self.scbytes_from_vec(buf)?)
    }

    // Notes on metering: covered by components
    fn deserialize_from_bytes(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b: BytesObject,
    ) -> Result<Val, HostError> {
        let scv = self.visit_obj(b, |hv: &ScBytes| {
            self.metered_from_xdr::<ScVal>(hv.as_slice())
        })?;
        self.to_host_val(&scv)
    }

    fn string_copy_to_linear_memory(
        &self,
        vmcaller: &mut VmCaller<Host>,
        s: StringObject,
        s_pos: U32Val,
        lm_pos: U32Val,
        len: U32Val,
    ) -> Result<Void, HostError> {
        self.memobj_copy_to_linear_memory::<ScString>(vmcaller, s, s_pos, lm_pos, len)?;
        Ok(Val::VOID)
    }

    fn symbol_copy_to_linear_memory(
        &self,
        vmcaller: &mut VmCaller<Host>,
        s: SymbolObject,
        s_pos: U32Val,
        lm_pos: U32Val,
        len: U32Val,
    ) -> Result<Void, HostError> {
        self.memobj_copy_to_linear_memory::<ScSymbol>(vmcaller, s, s_pos, lm_pos, len)?;
        Ok(Val::VOID)
    }

    fn bytes_copy_to_linear_memory(
        &self,
        vmcaller: &mut VmCaller<Host>,
        b: BytesObject,
        b_pos: U32Val,
        lm_pos: U32Val,
        len: U32Val,
    ) -> Result<Void, HostError> {
        self.memobj_copy_to_linear_memory::<ScBytes>(vmcaller, b, b_pos, lm_pos, len)?;
        Ok(Val::VOID)
    }

    fn bytes_copy_from_linear_memory(
        &self,
        vmcaller: &mut VmCaller<Host>,
        b: BytesObject,
        b_pos: U32Val,
        lm_pos: U32Val,
        len: U32Val,
    ) -> Result<BytesObject, HostError> {
        self.memobj_copy_from_linear_memory::<ScBytes>(vmcaller, b, b_pos, lm_pos, len)
    }

    fn bytes_new_from_linear_memory(
        &self,
        vmcaller: &mut VmCaller<Host>,
        lm_pos: U32Val,
        len: U32Val,
    ) -> Result<BytesObject, HostError> {
        self.memobj_new_from_linear_memory::<ScBytes>(vmcaller, lm_pos, len)
    }

    fn string_new_from_linear_memory(
        &self,
        vmcaller: &mut VmCaller<Host>,
        lm_pos: U32Val,
        len: U32Val,
    ) -> Result<StringObject, HostError> {
        self.memobj_new_from_linear_memory::<ScString>(vmcaller, lm_pos, len)
    }

    fn symbol_new_from_linear_memory(
        &self,
        vmcaller: &mut VmCaller<Host>,
        lm_pos: U32Val,
        len: U32Val,
    ) -> Result<SymbolObject, HostError> {
        self.memobj_new_from_linear_memory::<ScSymbol>(vmcaller, lm_pos, len)
    }

    fn symbol_index_in_linear_memory(
        &self,
        vmcaller: &mut VmCaller<Host>,
        sym: Symbol,
        lm_pos: U32Val,
        len: U32Val,
    ) -> Result<U32Val, HostError> {
        let VmSlice { vm, pos, len } = self.decode_vmslice(lm_pos, len)?;
        let mut found = None;
        self.metered_vm_scan_slices_in_linear_memory(
            vmcaller,
            &vm,
            pos,
            len as usize,
            |i, slice| {
                if self.symbol_matches(slice, sym)? {
                    if found.is_none() {
                        found = Some(self.usize_to_u32(i)?)
                    }
                }
                Ok(())
            },
        )?;
        match found {
            None => Err(self.err(
                ScErrorType::Value,
                ScErrorCode::MissingValue,
                "symbol not found in linear memory slices",
                &[sym.to_val()],
            )),
            Some(idx) => Ok(U32Val::from(idx)),
        }
    }

    // Notes on metering: covered by `add_host_object`
    fn bytes_new(&self, _vmcaller: &mut VmCaller<Host>) -> Result<BytesObject, HostError> {
        self.add_host_object(self.scbytes_from_vec(Vec::<u8>::new())?)
    }

    // Notes on metering: `get_mut` is free
    fn bytes_put(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b: BytesObject,
        iv: U32Val,
        u: U32Val,
    ) -> Result<BytesObject, HostError> {
        let i: u32 = iv.into();
        let u = self.u8_from_u32val_input("u", u)?;
        let vnew = self.visit_obj(b, |hv: &ScBytes| {
            let mut vnew: Vec<u8> = hv.metered_clone(self)?.into();
            match vnew.get_mut(i as usize) {
                None => Err(self.err(
                    ScErrorType::Object,
                    ScErrorCode::IndexBounds,
                    "bytes_put out of bounds",
                    &[iv.to_val()],
                )),
                Some(v) => {
                    *v = u;
                    Ok(ScBytes(vnew.try_into()?))
                }
            }
        })?;
        self.add_host_object(vnew)
    }

    // Notes on metering: `get` is free
    fn bytes_get(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b: BytesObject,
        iv: U32Val,
    ) -> Result<U32Val, HostError> {
        let i: u32 = iv.into();
        self.visit_obj(b, |hv: &ScBytes| {
            hv.get(i as usize)
                .map(|u| U32Val::from(u32::from(*u)))
                .ok_or_else(|| {
                    self.err(
                        ScErrorType::Object,
                        ScErrorCode::IndexBounds,
                        "bytes_get out of bounds",
                        &[iv.to_val()],
                    )
                })
        })
    }

    fn bytes_del(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b: BytesObject,
        i: U32Val,
    ) -> Result<BytesObject, HostError> {
        let i: u32 = i.into();
        let vnew = self.visit_obj(b, |hv: &ScBytes| {
            self.validate_index_lt_bound(i, hv.len())?;
            let mut vnew: Vec<u8> = hv.metered_clone(self)?.into();
            // len > i has been verified above but use saturating_sub just in case
            let n_elts = (hv.len() as u64).saturating_sub(i as u64);
            // remove elements incurs the cost of moving bytes, it does not incur
            // allocation/deallocation
            metered_clone::charge_shallow_copy::<u8>(n_elts, self)?;
            vnew.remove(i as usize);
            Ok(ScBytes(vnew.try_into()?))
        })?;
        self.add_host_object(vnew)
    }

    // Notes on metering: `len` is free
    fn bytes_len(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b: BytesObject,
    ) -> Result<U32Val, HostError> {
        let len = self.visit_obj(b, |hv: &ScBytes| Ok(hv.len()))?;
        self.usize_to_u32val(len)
    }

    // Notes on metering: `len` is free
    fn string_len(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b: StringObject,
    ) -> Result<U32Val, HostError> {
        let len = self.visit_obj(b, |hv: &ScString| Ok(hv.len()))?;
        self.usize_to_u32val(len)
    }

    // Notes on metering: `len` is free
    fn symbol_len(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b: SymbolObject,
    ) -> Result<U32Val, HostError> {
        let len = self.visit_obj(b, |hv: &ScSymbol| Ok(hv.len()))?;
        self.usize_to_u32val(len)
    }

    // Notes on metering: `push` is free
    fn bytes_push(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b: BytesObject,
        u: U32Val,
    ) -> Result<BytesObject, HostError> {
        let u = self.u8_from_u32val_input("u", u)?;
        let vnew = self.visit_obj(b, |hv: &ScBytes| {
            // we allocate the new vector to be able to hold `len + 1` bytes, so that the push
            // will not trigger a reallocation, causing data to be cloned twice.
            let len = self.validate_usize_sum_fits_in_u32(hv.len(), 1)?;
            Vec::<u8>::charge_bulk_init_cpy(len as u64, self)?;
            let mut vnew: Vec<u8> = Vec::with_capacity(len);
            vnew.extend_from_slice(hv.as_slice());
            vnew.push(u);
            Ok(ScBytes(vnew.try_into()?))
        })?;
        self.add_host_object(vnew)
    }

    // Notes on metering: `pop` is free
    fn bytes_pop(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b: BytesObject,
    ) -> Result<BytesObject, HostError> {
        let vnew = self.visit_obj(b, |hv: &ScBytes| {
            let mut vnew: Vec<u8> = hv.metered_clone(self)?.into();
            // Popping will not trigger reallocation. Here we don't charge anything since this is
            // just a `len` reduction.
            if vnew.pop().is_none() {
                return Err(self.err(
                    ScErrorType::Object,
                    ScErrorCode::IndexBounds,
                    "bytes_pop out of bounds",
                    &[],
                ));
            }
            Ok(ScBytes(vnew.try_into()?))
        })?;
        self.add_host_object(vnew)
    }

    // Notes on metering: `first` is free
    fn bytes_front(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b: BytesObject,
    ) -> Result<U32Val, HostError> {
        self.visit_obj(b, |hv: &ScBytes| {
            hv.first()
                .map(|u| U32Val::from(u32::from(*u)))
                .ok_or_else(|| {
                    self.err(
                        ScErrorType::Object,
                        ScErrorCode::IndexBounds,
                        "bytes_front out of bounds",
                        &[],
                    )
                })
        })
    }

    // Notes on metering: `last` is free
    fn bytes_back(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b: BytesObject,
    ) -> Result<U32Val, HostError> {
        self.visit_obj(b, |hv: &ScBytes| {
            hv.last()
                .map(|u| U32Val::from(u32::from(*u)))
                .ok_or_else(|| {
                    self.err(
                        ScErrorType::Object,
                        ScErrorCode::IndexBounds,
                        "bytes_back out of bounds",
                        &[],
                    )
                })
        })
    }

    fn bytes_insert(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b: BytesObject,
        i: U32Val,
        u: U32Val,
    ) -> Result<BytesObject, HostError> {
        let i: u32 = i.into();
        let u = self.u8_from_u32val_input("u", u)?;
        let vnew = self.visit_obj(b, |hv: &ScBytes| {
            self.validate_index_le_bound(i, hv.len())?;
            // we allocate the new vector to be able to hold `len + 1` bytes, so that the insert
            // will not trigger a reallocation, causing data to be cloned twice.
            let len = self.validate_usize_sum_fits_in_u32(hv.len(), 1)?;
            Vec::<u8>::charge_bulk_init_cpy(len as u64, self)?;
            let mut vnew: Vec<u8> = Vec::with_capacity(len);
            vnew.extend_from_slice(hv.as_slice());
            vnew.insert(i as usize, u);
            Ok(ScBytes(vnew.try_into()?))
        })?;
        self.add_host_object(vnew)
    }

    fn bytes_append(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b1: BytesObject,
        b2: BytesObject,
    ) -> Result<BytesObject, HostError> {
        let vnew = self.visit_obj(b1, |sb1: &ScBytes| {
            self.visit_obj(b2, |sb2: &ScBytes| {
                // we allocate large enough memory to hold the new combined vector, so that
                // allocation only happens once, and charge for it upfront.
                let len = self.validate_usize_sum_fits_in_u32(sb1.len(), sb2.len())?;
                Vec::<u8>::charge_bulk_init_cpy(len as u64, self)?;
                let mut vnew: Vec<u8> = Vec::with_capacity(len);
                vnew.extend_from_slice(sb1.as_slice());
                vnew.extend_from_slice(sb2.as_slice());
                Ok(vnew)
            })
        })?;
        self.add_host_object(ScBytes(vnew.try_into()?))
    }

    fn bytes_slice(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        b: BytesObject,
        start: U32Val,
        end: U32Val,
    ) -> Result<BytesObject, HostError> {
        let start: u32 = start.into();
        let end: u32 = end.into();
        let vnew = self.visit_obj(b, |hv: &ScBytes| {
            let range = self.valid_range_from_start_end_bound(start, end, hv.len())?;
            self.metered_slice_to_vec(
                &hv.as_slice()
                    .get(range)
                    .ok_or_else(|| self.err_oob_object_index(None))?,
            )
        })?;
        self.add_host_object(self.scbytes_from_vec(vnew)?)
    }

    // endregion "buf" module functions
    // region: "crypto" module functions

    // Notes on metering: covered by components.
    fn compute_hash_sha256(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        x: BytesObject,
    ) -> Result<BytesObject, HostError> {
        let hash = self.sha256_hash_from_bytesobj_input(x)?;
        self.add_host_object(self.scbytes_from_vec(hash)?)
    }

    // Notes on metering: covered by components.
    fn compute_hash_keccak256(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        x: BytesObject,
    ) -> Result<BytesObject, HostError> {
        let hash = self.keccak256_hash_from_bytesobj_input(x)?;
        self.add_host_object(self.scbytes_from_vec(hash)?)
    }

    // Notes on metering: covered by components.
    fn verify_sig_ed25519(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        k: BytesObject,
        x: BytesObject,
        s: BytesObject,
    ) -> Result<Void, HostError> {
        let verifying_key = self.ed25519_pub_key_from_bytesobj_input(k)?;
        let sig = self.ed25519_signature_from_bytesobj_input("sig", s)?;
        let res = self.visit_obj(x, |payload: &ScBytes| {
            self.verify_sig_ed25519_internal(payload.as_slice(), &verifying_key, &sig)
        });
        Ok(res?.into())
    }

    fn recover_key_ecdsa_secp256k1(
        &self,
        _vmcaller: &mut VmCaller<Host>,
        msg_digest: BytesObject,
        signature: BytesObject,
        recovery_id: U32Val,
    ) -> Result<BytesObject, HostError> {
        let sig = self.secp256k1_signature_from_bytesobj_input(signature)?;
        let rid = self.secp256k1_recovery_id_from_u32val(recovery_id)?;
        let hash = self.hash_from_bytesobj_input("msg_digest", msg_digest)?;
        self.recover_key_ecdsa_secp256k1_internal(&hash, &sig, rid)
    }

    // endregion "crypto" module functions
    // region: "test" module functions

    fn dummy0(&self, _vmcaller: &mut VmCaller<Self::VmUserState>) -> Result<Val, Self::Error> {
        Ok(().into())
    }

    // endregion "test" module functions
    // region: "address" module functions

    fn require_auth_for_args(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        address: AddressObject,
        args: VecObject,
    ) -> Result<Void, Self::Error> {
        let args = self.visit_obj(args, |a: &HostVec| a.to_vec(self.budget_ref()))?;
        Ok(self
            .try_borrow_authorization_manager()?
            .require_auth(self, address, args)?
            .into())
    }

    fn require_auth(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        address: AddressObject,
    ) -> Result<Void, Self::Error> {
        let args = self.with_current_frame(|f| {
            let args = match f {
                Frame::ContractVM { args, .. } => args,
                Frame::HostFunction(_) => {
                    return Err(self.err(
                        ScErrorType::Context,
                        ScErrorCode::InternalError,
                        "require_auth is not suppported for host fns",
                        &[],
                    ))
                }
                Frame::Token(_, _, args, _) => args,
                #[cfg(any(test, feature = "testutils"))]
                Frame::TestContract(c) => &c.args,
            };
            args.metered_clone(self)
        })?;

        Ok(self
            .try_borrow_authorization_manager()?
            .require_auth(self, address, args)?
            .into())
    }

    fn authorize_as_curr_contract(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        auth_entries: VecObject,
    ) -> Result<Void, HostError> {
        Ok(self
            .try_borrow_authorization_manager()?
            .add_invoker_contract_auth(self, auth_entries)?
            .into())
    }

    fn account_public_key_to_address(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        pk_bytes: BytesObject,
    ) -> Result<AddressObject, Self::Error> {
        let account_id = self.account_id_from_bytesobj(pk_bytes)?;
        self.add_host_object(ScAddress::Account(account_id))
    }

    fn contract_id_to_address(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        contract_id_bytes: BytesObject,
    ) -> Result<AddressObject, Self::Error> {
        let contract_id = self.hash_from_bytesobj_input("contract_id", contract_id_bytes)?;
        self.add_host_object(ScAddress::Contract(contract_id))
    }

    fn address_to_account_public_key(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        address: AddressObject,
    ) -> Result<Val, Self::Error> {
        let addr = self.visit_obj(address, |addr: &ScAddress| addr.metered_clone(self))?;
        match addr {
            ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(pk))) => Ok(self
                .add_host_object(ScBytes(self.metered_slice_to_vec(&pk.0)?.try_into()?))?
                .into()),
            ScAddress::Contract(_) => Ok(().into()),
        }
    }

    fn address_to_contract_id(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        address: AddressObject,
    ) -> Result<Val, Self::Error> {
        let addr = self.visit_obj(address, |addr: &ScAddress| addr.metered_clone(self))?;
        match addr {
            ScAddress::Account(_) => Ok(().into()),
            ScAddress::Contract(Hash(h)) => Ok(self
                .add_host_object(ScBytes(self.metered_slice_to_vec(&h)?.try_into()?))?
                .into()),
        }
    }

    // endregion "address" module functions
    // region: "prng" module functions

    fn prng_reseed(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        seed: BytesObject,
    ) -> Result<Void, Self::Error> {
        self.visit_obj(seed, |bytes: &ScBytes| {
            let slice: &[u8] = bytes.as_ref();
            self.charge_budget(ContractCostType::HostMemCpy, Some(prng::SEED_BYTES))?;
            if let Ok(seed32) = slice.try_into() {
                self.with_current_prng(|prng| {
                    *prng = Prng::new_from_seed(seed32);
                    Ok(())
                })?;
                Ok(Val::VOID)
            } else if let Ok(len) = u32::try_from(slice.len()) {
                Err(self.err(
                    ScErrorType::Value,
                    ScErrorCode::UnexpectedSize,
                    "Unexpected size of BytesObject in prng_reseed",
                    &[U32Val::from(len).to_val()],
                ))
            } else {
                Err(self.err(
                    ScErrorType::Value,
                    ScErrorCode::UnexpectedSize,
                    "Unexpected size of BytesObject in prng_reseed",
                    &[],
                ))
            }
        })
    }

    fn prng_bytes_new(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        length: U32Val,
    ) -> Result<BytesObject, Self::Error> {
        self.add_host_object(
            self.with_current_prng(|prng| prng.bytes_new(length.into(), self.as_budget()))?,
        )
    }

    fn prng_u64_in_inclusive_range(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        lo: u64,
        hi: u64,
    ) -> Result<u64, Self::Error> {
        self.with_current_prng(|prng| prng.u64_in_inclusive_range(lo..=hi, self.as_budget()))
    }

    fn prng_vec_shuffle(
        &self,
        _vmcaller: &mut VmCaller<Self::VmUserState>,
        vec: VecObject,
    ) -> Result<VecObject, Self::Error> {
        let vnew = self.visit_obj(vec, |v: &HostVec| {
            self.with_current_prng(|prng| prng.vec_shuffle(v, self.as_budget()))
        })?;
        self.add_host_object(vnew)
    }
    // endregion "prng" module functions
}

#[cfg(any(test, feature = "testutils"))]
pub(crate) mod testutils {
    use std::cell::Cell;
    use std::panic::{catch_unwind, set_hook, take_hook, UnwindSafe};
    use std::sync::Once;

    /// Catch panics while suppressing the default panic hook that prints to the
    /// console.
    ///
    /// For the purposes of test reporting we don't want every panicking (but
    /// caught) contract call to print to the console. This requires overriding
    /// the panic hook, a global resource. This is an awkward thing to do with
    /// tests running in parallel.
    ///
    /// This function lazily performs a one-time wrapping of the existing panic
    /// hook. It then uses a thread local variable to track contract call depth.
    /// If a panick occurs during a contract call the original hook is not
    /// called, otherwise it is called.
    pub fn call_with_suppressed_panic_hook<C, R>(closure: C) -> std::thread::Result<R>
    where
        C: FnOnce() -> R + UnwindSafe,
    {
        thread_local! {
            static TEST_CONTRACT_CALL_COUNT: Cell<u64> = Cell::new(0);
        }

        static WRAP_PANIC_HOOK: Once = Once::new();

        WRAP_PANIC_HOOK.call_once(|| {
            let existing_panic_hook = take_hook();
            set_hook(Box::new(move |info| {
                let calling_test_contract = TEST_CONTRACT_CALL_COUNT.with(|c| c.get() != 0);
                if !calling_test_contract {
                    existing_panic_hook(info)
                }
            }))
        });

        TEST_CONTRACT_CALL_COUNT.with(|c| {
            let old_count = c.get();
            let new_count = old_count.checked_add(1).expect("overflow");
            c.set(new_count);
        });

        let res = catch_unwind(closure);

        TEST_CONTRACT_CALL_COUNT.with(|c| {
            let old_count = c.get();
            let new_count = old_count.checked_sub(1).expect("overflow");
            c.set(new_count);
        });

        res
    }
}
