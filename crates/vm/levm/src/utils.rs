use crate::{
    account::Account,
    call_frame::CallFrame,
    constants::*,
    db::{
        cache::{self},
        CacheDB, Database,
    },
    errors::{InternalError, OutOfGasError, VMError},
    gas_cost::{
        self, fake_exponential, ACCESS_LIST_ADDRESS_COST, ACCESS_LIST_STORAGE_KEY_COST,
        BLOB_GAS_PER_BLOB, COLD_ADDRESS_ACCESS_COST, CREATE_BASE_COST, WARM_ADDRESS_ACCESS_COST,
    },
    opcodes::Opcode,
    vm::{AccessList, AuthorizationList, AuthorizationTuple, EVMConfig, Substate},
    AccountInfo,
};
use bytes::Bytes;
use ethrex_core::{types::Fork, Address, H256, U256};
use ethrex_rlp;
use ethrex_rlp::encode::RLPEncode;
use keccak_hash::keccak;
use libsecp256k1::{Message, RecoveryId, Signature};
use sha3::{Digest, Keccak256};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
pub type Storage = HashMap<U256, H256>;

// ================== Address related functions ======================
pub fn address_to_word(address: Address) -> U256 {
    // This unwrap can't panic, as Address are 20 bytes long and U256 use 32 bytes
    let mut word = [0u8; 32];

    for (word_byte, address_byte) in word.iter_mut().skip(12).zip(address.as_bytes().iter()) {
        *word_byte = *address_byte;
    }

    U256::from_big_endian(&word)
}

/// Calculates the address of a new conctract using the CREATE
/// opcode as follows:
///
/// address = keccak256(rlp([sender_address,sender_nonce]))[12:]
pub fn calculate_create_address(
    sender_address: Address,
    sender_nonce: u64,
) -> Result<Address, VMError> {
    let mut encoded = Vec::new();
    (sender_address, sender_nonce).encode(&mut encoded);
    let mut hasher = Keccak256::new();
    hasher.update(encoded);
    Ok(Address::from_slice(hasher.finalize().get(12..).ok_or(
        VMError::Internal(InternalError::CouldNotComputeCreateAddress),
    )?))
}

/// Calculates the address of a new contract using the CREATE2 opcode as follow
///
/// initialization_code = memory[offset:offset+size]
///
/// address = keccak256(0xff + sender_address + salt + keccak256(initialization_code))[12:]
///
pub fn calculate_create2_address(
    sender_address: Address,
    initialization_code: &Bytes,
    salt: U256,
) -> Result<Address, VMError> {
    let init_code_hash = keccak(initialization_code);

    let generated_address = Address::from_slice(
        keccak(
            [
                &[0xff],
                sender_address.as_bytes(),
                &salt.to_big_endian(),
                init_code_hash.as_bytes(),
            ]
            .concat(),
        )
        .as_bytes()
        .get(12..)
        .ok_or(VMError::Internal(
            InternalError::CouldNotComputeCreate2Address,
        ))?,
    );
    Ok(generated_address)
}

pub fn get_valid_jump_destinations(code: &Bytes) -> Result<HashSet<usize>, VMError> {
    let mut valid_jump_destinations = HashSet::new();
    let mut pc = 0;

    while let Some(&opcode_number) = code.get(pc) {
        let current_opcode = Opcode::from(opcode_number);

        if current_opcode == Opcode::JUMPDEST {
            // If current opcode is jumpdest, add it to valid destinations set
            valid_jump_destinations.insert(pc);
        } else if (Opcode::PUSH1..=Opcode::PUSH32).contains(&current_opcode) {
            // If current opcode is push, skip as many positions as the size of the push
            let size_to_push =
                opcode_number
                    .checked_sub(u8::from(Opcode::PUSH1))
                    .ok_or(VMError::Internal(
                        InternalError::ArithmeticOperationUnderflow,
                    ))?;
            let skip_length = usize::from(size_to_push.checked_add(1).ok_or(VMError::Internal(
                InternalError::ArithmeticOperationOverflow,
            ))?);
            pc = pc.checked_add(skip_length).ok_or(VMError::Internal(
                InternalError::ArithmeticOperationOverflow, // to fail, pc should be at least usize max - 31
            ))?;
        }

        pc = pc.checked_add(1).ok_or(VMError::Internal(
            InternalError::ArithmeticOperationOverflow, // to fail, code len should be more than usize max
        ))?;
    }

    Ok(valid_jump_destinations)
}

// ================== Account related functions =====================
/// Gets account, first checking the cache and then the database
/// (caching in the second case)
pub fn get_account(cache: &mut CacheDB, db: &Arc<dyn Database>, address: Address) -> Account {
    match cache::get_account(cache, &address) {
        Some(acc) => acc.clone(),
        None => {
            let account_info = db.get_account_info(address);
            let account = Account {
                info: account_info,
                storage: HashMap::new(),
            };
            cache::insert_account(cache, address, account.clone());
            account
        }
    }
}

pub fn get_account_no_push_cache(
    cache: &CacheDB,
    db: &Arc<dyn Database>,
    address: Address,
) -> Account {
    match cache::get_account(cache, &address) {
        Some(acc) => acc.clone(),
        None => {
            let account_info = db.get_account_info(address);
            Account {
                info: account_info,
                storage: HashMap::new(),
            }
        }
    }
}

pub fn get_account_mut_vm<'vm>(
    cache: &'vm mut CacheDB,
    db: &'vm Arc<dyn Database>,
    address: Address,
) -> Result<&'vm mut Account, VMError> {
    if !cache::is_account_cached(cache, &address) {
        let account_info = db.get_account_info(address);
        let account = Account {
            info: account_info,
            storage: HashMap::new(),
        };
        cache::insert_account(cache, address, account.clone());
    }
    cache::get_account_mut(cache, &address).ok_or(VMError::Internal(InternalError::AccountNotFound))
}

pub fn increase_account_balance(
    cache: &mut CacheDB,
    db: &mut Arc<dyn Database>,
    address: Address,
    increase: U256,
) -> Result<(), VMError> {
    let account = get_account_mut_vm(cache, db, address)?;
    account.info.balance = account
        .info
        .balance
        .checked_add(increase)
        .ok_or(VMError::BalanceOverflow)?;
    Ok(())
}

pub fn decrease_account_balance(
    cache: &mut CacheDB,
    db: &mut Arc<dyn Database>,
    address: Address,
    decrease: U256,
) -> Result<(), VMError> {
    let account = get_account_mut_vm(cache, db, address)?;
    account.info.balance = account
        .info
        .balance
        .checked_sub(decrease)
        .ok_or(VMError::BalanceUnderflow)?;
    Ok(())
}

// ================== Bytecode related functions =====================
pub fn update_account_bytecode(
    cache: &mut CacheDB,
    db: &Arc<dyn Database>,
    address: Address,
    new_bytecode: Bytes,
) -> Result<(), VMError> {
    let account = get_account_mut_vm(cache, db, address)?;
    account.info.bytecode = new_bytecode;
    Ok(())
}

// ==================== Gas related functions =======================
pub fn get_intrinsic_gas(
    is_create: bool,
    fork: Fork,
    access_list: &AccessList,
    authorization_list: &Option<AuthorizationList>,
    initial_call_frame: &CallFrame,
) -> Result<u64, VMError> {
    // Intrinsic Gas = Calldata cost + Create cost + Base cost + Access list cost
    let mut intrinsic_gas: u64 = 0;

    // Calldata Cost
    // 4 gas for each zero byte in the transaction data 16 gas for each non-zero byte in the transaction.
    let calldata_cost =
        gas_cost::tx_calldata(&initial_call_frame.calldata, fork).map_err(VMError::OutOfGas)?;

    intrinsic_gas = intrinsic_gas
        .checked_add(calldata_cost)
        .ok_or(OutOfGasError::ConsumedGasOverflow)?;

    // Base Cost
    intrinsic_gas = intrinsic_gas
        .checked_add(TX_BASE_COST)
        .ok_or(OutOfGasError::ConsumedGasOverflow)?;

    // Create Cost
    if is_create {
        intrinsic_gas = intrinsic_gas
            .checked_add(CREATE_BASE_COST)
            .ok_or(OutOfGasError::ConsumedGasOverflow)?;

        let number_of_words = initial_call_frame.calldata.len().div_ceil(WORD_SIZE);
        let double_number_of_words: u64 = number_of_words
            .checked_mul(2)
            .ok_or(OutOfGasError::ConsumedGasOverflow)?
            .try_into()
            .map_err(|_| VMError::Internal(InternalError::ConversionError))?;

        intrinsic_gas = intrinsic_gas
            .checked_add(double_number_of_words)
            .ok_or(OutOfGasError::ConsumedGasOverflow)?;
    }

    // Access List Cost
    let mut access_lists_cost: u64 = 0;
    for (_, keys) in access_list {
        access_lists_cost = access_lists_cost
            .checked_add(ACCESS_LIST_ADDRESS_COST)
            .ok_or(OutOfGasError::ConsumedGasOverflow)?;
        for _ in keys {
            access_lists_cost = access_lists_cost
                .checked_add(ACCESS_LIST_STORAGE_KEY_COST)
                .ok_or(OutOfGasError::ConsumedGasOverflow)?;
        }
    }

    intrinsic_gas = intrinsic_gas
        .checked_add(access_lists_cost)
        .ok_or(OutOfGasError::ConsumedGasOverflow)?;

    // Authorization List Cost
    // `unwrap_or_default` will return an empty vec when the `authorization_list` field is None.
    // If the vec is empty, the len will be 0, thus the authorization_list_cost is 0.
    let amount_of_auth_tuples: u64 = authorization_list
        .clone()
        .unwrap_or_default()
        .len()
        .try_into()
        .map_err(|_| VMError::Internal(InternalError::ConversionError))?;
    let authorization_list_cost = PER_EMPTY_ACCOUNT_COST
        .checked_mul(amount_of_auth_tuples)
        .ok_or(VMError::Internal(InternalError::GasOverflow))?;

    intrinsic_gas = intrinsic_gas
        .checked_add(authorization_list_cost)
        .ok_or(OutOfGasError::ConsumedGasOverflow)?;

    Ok(intrinsic_gas)
}

// ================= Blob hash related functions =====================
pub fn get_base_fee_per_blob_gas(
    block_excess_blob_gas: Option<U256>,
    evm_config: &EVMConfig,
) -> Result<U256, VMError> {
    let base_fee_update_fraction = evm_config.blob_schedule.base_fee_update_fraction;
    fake_exponential(
        MIN_BASE_FEE_PER_BLOB_GAS,
        block_excess_blob_gas.unwrap_or_default(),
        base_fee_update_fraction.into(),
    )
}

/// Gets the max blob gas cost for a transaction that a user is
/// willing to pay.
pub fn get_max_blob_gas_price(
    tx_blob_hashes: Vec<H256>,
    tx_max_fee_per_blob_gas: Option<U256>,
) -> Result<U256, VMError> {
    let blobhash_amount: u64 = tx_blob_hashes
        .len()
        .try_into()
        .map_err(|_| VMError::Internal(InternalError::ConversionError))?;

    let blob_gas_used: u64 = blobhash_amount
        .checked_mul(BLOB_GAS_PER_BLOB)
        .unwrap_or_default();

    let max_blob_gas_cost = tx_max_fee_per_blob_gas
        .unwrap_or_default()
        .checked_mul(blob_gas_used.into())
        .ok_or(InternalError::UndefinedState(1))?;

    Ok(max_blob_gas_cost)
}
/// Gets the actual blob gas cost.
pub fn get_blob_gas_price(
    tx_blob_hashes: Vec<H256>,
    block_excess_blob_gas: Option<U256>,
    evm_config: &EVMConfig,
) -> Result<U256, VMError> {
    let blobhash_amount: u64 = tx_blob_hashes
        .len()
        .try_into()
        .map_err(|_| VMError::Internal(InternalError::ConversionError))?;

    let blob_gas_price: u64 = blobhash_amount
        .checked_mul(BLOB_GAS_PER_BLOB)
        .unwrap_or_default();

    let base_fee_per_blob_gas = get_base_fee_per_blob_gas(block_excess_blob_gas, evm_config)?;

    let blob_gas_price: U256 = blob_gas_price.into();
    let blob_fee: U256 = blob_gas_price
        .checked_mul(base_fee_per_blob_gas)
        .ok_or(VMError::Internal(InternalError::UndefinedState(1)))?;

    Ok(blob_fee)
}

// =================== Opcode related functions ======================

pub fn get_n_value(op: Opcode, base_opcode: Opcode) -> Result<usize, VMError> {
    let offset = (usize::from(op))
        .checked_sub(usize::from(base_opcode))
        .ok_or(VMError::InvalidOpcode)?
        .checked_add(1)
        .ok_or(VMError::InvalidOpcode)?;

    Ok(offset)
}

pub fn get_number_of_topics(op: Opcode) -> Result<u8, VMError> {
    let number_of_topics = (u8::from(op))
        .checked_sub(u8::from(Opcode::LOG0))
        .ok_or(VMError::InvalidOpcode)?;

    Ok(number_of_topics)
}

// =================== Nonce related functions ======================
pub fn increment_account_nonce(
    cache: &mut CacheDB,
    db: &Arc<dyn Database>,
    address: Address,
) -> Result<u64, VMError> {
    let account = get_account_mut_vm(cache, db, address)?;
    account.info.nonce = account
        .info
        .nonce
        .checked_add(1)
        .ok_or(VMError::NonceOverflow)?;
    Ok(account.info.nonce)
}

pub fn decrement_account_nonce(
    cache: &mut CacheDB,
    db: &Arc<dyn Database>,
    address: Address,
) -> Result<(), VMError> {
    let account = get_account_mut_vm(cache, db, address)?;
    account.info.nonce = account
        .info
        .nonce
        .checked_sub(1)
        .ok_or(VMError::NonceUnderflow)?;
    Ok(())
}

// ==================== Word related functions =======================
pub fn word_to_address(word: U256) -> Address {
    Address::from_slice(&word.to_big_endian()[12..])
}

// ================== EIP-7702 related functions =====================

/// Checks if account.info.bytecode has been delegated as the EIP7702
/// determines.
pub fn has_delegation(account_info: &AccountInfo) -> Result<bool, VMError> {
    let mut has_delegation = false;
    if account_info.has_code() && account_info.bytecode.len() == EIP7702_DELEGATED_CODE_LEN {
        let first_3_bytes = account_info
            .bytecode
            .get(..3)
            .ok_or(VMError::Internal(InternalError::SlicingError))?;

        if first_3_bytes == SET_CODE_DELEGATION_BYTES {
            has_delegation = true;
        }
    }
    Ok(has_delegation)
}

/// Gets the address inside the account.info.bytecode if it has been
/// delegated as the EIP7702 determines.
pub fn get_authorized_address(account_info: &AccountInfo) -> Result<Address, VMError> {
    if has_delegation(account_info)? {
        let address_bytes = account_info
            .bytecode
            .get(SET_CODE_DELEGATION_BYTES.len()..)
            .ok_or(VMError::Internal(InternalError::SlicingError))?;
        // It shouldn't panic when doing Address::from_slice()
        // because the length is checked inside the has_delegation() function
        let address = Address::from_slice(address_bytes);
        Ok(address)
    } else {
        // if we end up here, it means that the address wasn't previously delegated.
        Err(VMError::Internal(InternalError::AccountNotDelegated))
    }
}

pub fn eip7702_recover_address(
    auth_tuple: &AuthorizationTuple,
) -> Result<Option<Address>, VMError> {
    if auth_tuple.s_signature > *SECP256K1_ORDER_OVER2 || U256::zero() >= auth_tuple.s_signature {
        return Ok(None);
    }
    if auth_tuple.r_signature > *SECP256K1_ORDER || U256::zero() >= auth_tuple.r_signature {
        return Ok(None);
    }
    if auth_tuple.v != U256::one() && auth_tuple.v != U256::zero() {
        return Ok(None);
    }

    let rlp_buf = (auth_tuple.chain_id, auth_tuple.address, auth_tuple.nonce).encode_to_vec();

    let mut hasher = Keccak256::new();
    hasher.update([MAGIC]);
    hasher.update(rlp_buf);
    let bytes = &mut hasher.finalize();

    let Ok(message) = Message::parse_slice(bytes) else {
        return Ok(None);
    };

    let bytes = [
        auth_tuple.r_signature.to_big_endian(),
        auth_tuple.s_signature.to_big_endian(),
    ]
    .concat();

    let Ok(signature) = Signature::parse_standard_slice(&bytes) else {
        return Ok(None);
    };

    let Ok(recovery_id) = RecoveryId::parse(
        auth_tuple
            .v
            .as_u32()
            .try_into()
            .map_err(|_| VMError::Internal(InternalError::ConversionError))?,
    ) else {
        return Ok(None);
    };

    let Ok(authority) = libsecp256k1::recover(&message, &signature, &recovery_id) else {
        return Ok(None);
    };

    let public_key = authority.serialize();
    let mut hasher = Keccak256::new();
    hasher.update(
        public_key
            .get(1..)
            .ok_or(VMError::Internal(InternalError::SlicingError))?,
    );
    let address_hash = hasher.finalize();

    // Get the last 20 bytes of the hash -> Address
    let authority_address_bytes: [u8; 20] = address_hash
        .get(12..32)
        .ok_or(VMError::Internal(InternalError::SlicingError))?
        .try_into()
        .map_err(|_| VMError::Internal(InternalError::ConversionError))?;
    Ok(Some(Address::from_slice(&authority_address_bytes)))
}

/// Used for the opcodes
/// The following reading instructions are impacted:
///      EXTCODESIZE, EXTCODECOPY, EXTCODEHASH
/// and the following executing instructions are impacted:
///      CALL, CALLCODE, STATICCALL, DELEGATECALL
/// In case a delegation designator points to another designator,
/// creating a potential chain or loop of designators, clients must
/// retrieve only the first code and then stop following the
/// designator chain.
///
/// For example,
/// EXTCODESIZE would return 2 (the size of 0xef01) instead of 23
/// which would represent the delegation designation, EXTCODEHASH
/// would return
/// 0xeadcdba66a79ab5dce91622d1d75c8cff5cff0b96944c3bf1072cd08ce018329
/// (keccak256(0xef01)), and CALL would load the code from address and
/// execute it in the context of authority.
///
/// The idea of this function comes from ethereum/execution-specs:
/// https://github.com/ethereum/execution-specs/blob/951fc43a709b493f27418a8e57d2d6f3608cef84/src/ethereum/prague/vm/eoa_delegation.py#L115
pub fn eip7702_get_code(
    cache: &mut CacheDB,
    db: &Arc<dyn Database>,
    accrued_substate: &mut Substate,
    address: Address,
) -> Result<(bool, u64, Address, Bytes), VMError> {
    // Address is the delgated address
    let account = get_account(cache, db, address);
    let bytecode = account.info.bytecode.clone();

    // If the Address doesn't have a delegation code
    // return false meaning that is not a delegation
    // return the same address given
    // return the bytecode of the given address
    if !has_delegation(&account.info)? {
        return Ok((false, 0, address, bytecode));
    }

    // Here the address has a delegation code
    // The delegation code has the authorized address
    let auth_address = get_authorized_address(&account.info)?;

    let access_cost = if accrued_substate.touched_accounts.contains(&auth_address) {
        WARM_ADDRESS_ACCESS_COST
    } else {
        accrued_substate.touched_accounts.insert(auth_address);
        COLD_ADDRESS_ACCESS_COST
    };

    let authorized_bytecode = get_account(cache, db, auth_address).info.bytecode;

    Ok((true, access_cost, auth_address, authorized_bytecode))
}
