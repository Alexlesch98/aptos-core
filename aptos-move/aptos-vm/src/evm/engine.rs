// Copyright © Aptos Foundation

use std::collections::BTreeMap;
use evm::executor::stack::{MemoryStackState, PrecompileFn, StackExecutor, StackSubstateMetadata};
use evm_core::ExitReason;
use evm_runtime::Config;
use primitive_types::{H160, H256, U256};
use aptos_table_natives::{TableHandle, TableResolver};
use crate::evm::eth_address::EthAddress;
use crate::evm::evm_backend::EVMBackend;
use move_core_types::account_address::AccountAddress;
use evm::backend::MemoryAccount;
#[cfg(test)]
use crate::evm::in_memory_storage::InMemoryTableResolver;
use crate::evm::utils::u256_to_arr;
use std::str::FromStr;

pub struct Engine<'a> {
    resolver: &'a dyn TableResolver,
    nonce_table_handle: TableHandle,
    balance_table_handle: TableHandle,
    code_table_handle: TableHandle,
    storage_table_handle: TableHandle,
    origin: EthAddress,
}

impl<'a> Engine<'a> {
    pub fn new(
        resolver: &'a dyn TableResolver,
        nonce_table_handle: TableHandle,
        balance_table_handle: TableHandle,
        code_table_handle: TableHandle,
        storage_table_handle: TableHandle,
        origin: EthAddress,
    ) -> Self {
        Self {
            resolver,
            nonce_table_handle,
            balance_table_handle,
            code_table_handle,
            storage_table_handle,
            origin,
        }
    }

    pub fn transact_call(
        &mut self,
        caller: H160,
        address: H160,
        value: U256,
        data: Vec<u8>,
        gas_limit: u64,
        access_list: Vec<(H160, Vec<H256>)>,
    ) -> (ExitReason, Vec<u8>) {
        let config = Config::istanbul();
        let backend = EVMBackend::new(self.resolver,
                                      self.nonce_table_handle,
                                      self.balance_table_handle,
                                      self.code_table_handle,
                                      self.storage_table_handle,
                                      self.origin.clone());

        let metadata = StackSubstateMetadata::new(u64::MAX, &config);
        let state = MemoryStackState::new(metadata, &backend);
        let precompiles: BTreeMap<_, PrecompileFn> = BTreeMap::new();
        let mut executor = StackExecutor::new_with_precompiles(state, &config, &precompiles);
        executor.transact_call(caller, address, value, data, gas_limit, access_list)
    }

    pub fn transact_create(
        &mut self,
        caller: H160,
        value: U256,
        init_code: Vec<u8>,
        gas_limit: u64,
        access_list: Vec<(H160, Vec<H256>)>,
    ) -> (ExitReason, Vec<u8>) {
        let config = Config::istanbul();
        let backend = EVMBackend::new(self.resolver,
                                      self.nonce_table_handle,
                                      self.balance_table_handle,
                                      self.code_table_handle,
                                      self.storage_table_handle,
                                      self.origin.clone());

        let metadata = StackSubstateMetadata::new(u64::MAX, &config);
        let state = MemoryStackState::new(metadata, &backend);
        let precompiles: BTreeMap<_, PrecompileFn> = BTreeMap::new();
        let mut executor = StackExecutor::new_with_precompiles(state, &config, &precompiles);
        executor.transact_create(caller, value, init_code, gas_limit, access_list)
    }
}

#[cfg(test)]
fn test_contract_in_memory_table() {
    let config = Config::istanbul();

    let mut table_resolver = InMemoryTableResolver::new();
    let nonce_table_handle = TableHandle(AccountAddress::random());
    table_resolver.add_table(nonce_table_handle.clone());
    let balance_table_handle = TableHandle(AccountAddress::random());
    table_resolver.add_table(balance_table_handle.clone());
    let code_table_handle = TableHandle(AccountAddress::random());
    table_resolver.add_table(code_table_handle.clone());
    let storage_table_handle = TableHandle(AccountAddress::random());
    table_resolver.add_table(storage_table_handle.clone());

    fn add_memory_account(resolver: &mut InMemoryTableResolver,
                          nonce_table_handle: &TableHandle,
                          balance_table_handle: &TableHandle,
                          code_table_handle: &TableHandle,
                          storage_table_handle: &TableHandle,
                          address: &EthAddress, account: MemoryAccount) {
        resolver.add_table_entry(nonce_table_handle, address.as_bytes().to_vec(), u256_to_arr( &account.nonce).to_vec());
        resolver.add_table_entry(balance_table_handle, address.as_bytes().to_vec(), u256_to_arr( &account.balance).to_vec());
        resolver.add_table_entry(code_table_handle, address.as_bytes().to_vec(), account.code);
        for (index, value) in account.storage {
            let mut buf = [0u8; 52];
            buf[..20].copy_from_slice(&address.as_bytes());
            buf[20..].copy_from_slice(&index.as_bytes());
            resolver.add_table_entry(storage_table_handle, buf.to_vec(), value.as_bytes().to_vec());
        }
    }

    let account1 = MemoryAccount {
        nonce: U256::one(),
        balance: U256::from(10000000),
        storage: BTreeMap::new(),
        code: hex::decode("6080604052348015600f57600080fd5b506004361060285760003560e01c80630f14a40614602d575b600080fd5b605660048036036020811015604157600080fd5b8101908080359060200190929190505050606c565b6040518082815260200191505060405180910390f35b6000806000905060005b83811015608f5760018201915080806001019150506076565b508091505091905056fea26469706673582212202bc9ec597249a9700278fe4ce78da83273cb236e76d4d6797b441454784f901d64736f6c63430007040033").unwrap(),
    };

    add_memory_account(&mut table_resolver,
                       &nonce_table_handle,
                       &balance_table_handle,
                       &code_table_handle,
                       &storage_table_handle,
                       &EthAddress::new(H160::from_str("0x1000000000000000000000000000000000000000").unwrap(),),
                       account1);


    let account2 =  MemoryAccount {
        nonce: U256::one(),
        balance: U256::from(10000000),
        storage: BTreeMap::new(),
        code: Vec::new(),
    };

    add_memory_account(&mut table_resolver,
                       &nonce_table_handle,
                       &balance_table_handle,
                       &code_table_handle,
                       &storage_table_handle,
                       &EthAddress::new(H160::from_str("0xf000000000000000000000000000000000000000").unwrap()),
                       account2);

    let mut engine = Engine::new(&table_resolver,
                                 nonce_table_handle,
                                 balance_table_handle,
                                 code_table_handle,
                                 storage_table_handle,
                                 EthAddress::new(H160::default())
    );

    let _reason = engine.transact_call(
        H160::from_str("0xf000000000000000000000000000000000000000").unwrap(),
        H160::from_str("0x1000000000000000000000000000000000000000").unwrap(),
        U256::zero(),
        // hex::decode("0f14a4060000000000000000000000000000000000000000000000000000000000b71b00")
        // 	.unwrap(),
        hex::decode("0f14a4060000000000000000000000000000000000000000000000000000000000002ee0")
            .unwrap(),
        u64::MAX,
        Vec::new(),
    );
}


#[cfg(test)]
mod tests {
    use crate::evm::engine::test_contract_in_memory_table;
    #[test]
    fn test_run_loop_contract_table() {
        test_contract_in_memory_table();

    }

}