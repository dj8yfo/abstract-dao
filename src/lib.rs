mod constants;
mod helpers;
mod primitives;

use constants::{MIN_GAS_FOR_GET_SIGNATURE, ONE_MINUTE_NANOS};
use helpers::{
    assert_deposit, assert_gas, calculate_deposit_for_used_storage, create_derivation_path,
    create_eip1559_tx, create_on_sign_callback_promise, create_sign_promise, refund_unused_deposit,
};
use near_sdk::serde_json;
use near_sdk::{
    env::{self, block_timestamp},
    near, require,
    store::LookupMap,
    AccountId, NearToken, PanicOnDefault, Promise, PromiseResult,
};
use primitives::{
    GetSignatureResponse, InputRequest, OtherEip1559TransactionPayload,
    RegisterSignatureReqResponse, Request, RequestId, StorageKey,
};

// Define the contract structure
#[derive(PanicOnDefault)]
#[near(contract_state)]
pub struct Contract {
    /// Next available id for the requests.
    pub next_request_id: RequestId,
    /// Map of signing requests
    pub requests: LookupMap<RequestId, Request>,
    /// MPC Account ID
    pub mpc_contract_id: AccountId,
}

// Public API
#[near]
impl Contract {
    #[init]
    pub fn new(mpc_contract_id: AccountId) -> Self {
        Self {
            next_request_id: 0,
            requests: LookupMap::new(StorageKey::AllRequests),
            mpc_contract_id: mpc_contract_id.clone(),
        }
    }

    pub fn get_mpc_contract_id(&self) -> AccountId {
        self.mpc_contract_id.clone()
    }

    #[payable]
    pub fn register_signature_request(
        &mut self,
        request: InputRequest,
    ) -> RegisterSignatureReqResponse {
        let storage_used_before = env::storage_usage();
        let new_request = self.add_request(request);
        let storage_used_after = env::storage_usage();

        let used_storage = storage_used_after
            .checked_sub(storage_used_before)
            .expect("ERR_UNEXPECTED");

        let storage_deposit = calculate_deposit_for_used_storage(used_storage);

        assert_deposit(storage_deposit);
        refund_unused_deposit(storage_deposit);

        RegisterSignatureReqResponse {
            request_id: new_request.id,
            deadline: new_request.deadline,
            derivation_path: new_request.derivation_path,
            mpc_account_id: self.mpc_contract_id.clone(),
            allowed_account_id: new_request.allowed_account_id,
        }
    }

    #[payable]
    pub fn get_signature(
        &mut self,
        request_id: RequestId,
        other_payload: OtherEip1559TransactionPayload,
    ) -> Promise {
        assert_deposit(NearToken::from_yoctonear(1));
        assert_gas(MIN_GAS_FOR_GET_SIGNATURE);

        let request = self.get_request_or_panic(request_id);

        require!(
            !request.is_time_exceeded(env::block_timestamp()),
            "ERR_TIME_IS_UP"
        );

        require!(
            request.is_account_allowed(env::predecessor_account_id()),
            "ERR_FORBIDDEN"
        );

        let tx = create_eip1559_tx(request.payload.clone(), other_payload);

        let sign_promise =
            create_sign_promise(self.mpc_contract_id.clone(), tx.clone(), request.clone());
        let callback_promise = create_on_sign_callback_promise(tx);

        sign_promise.then(callback_promise)
    }

    #[private]
    pub fn on_get_signature(&mut self, tx_hex: String) -> GetSignatureResponse {
        // https://docs.rs/near-sdk/latest/near_sdk/env/fn.promise_results_count.html
        assert_eq!(env::promise_results_count(), 1, "ERR_TOO_MANY_RESULTS");

        let signature_json = match env::promise_result(0) {
            PromiseResult::Successful(data) => {
                serde_json::from_slice::<serde_json::Value>(data.as_slice())
                    .expect("Couldn't deserialize signature!")
            }
            _ => env::panic_str("Signature couldn't be decoded from Promise response!"),
        };

        GetSignatureResponse {
            tx: tx_hex,
            signature: signature_json,
        }
    }
}

/// Internal helpers API
impl Contract {
    fn add_request(&mut self, input_request: InputRequest) -> Request {
        let current_request_id = self.next_request_id;
        self.next_request_id += 1;

        let internal_request = Request {
            id: current_request_id,
            allowed_account_id: input_request.allowed_account_id,
            payload: input_request.transaction_payload.into(),
            derivation_path: create_derivation_path(input_request.derivation_seed_number),
            key_version: input_request.key_version.unwrap_or(0),
            deadline: block_timestamp() + 24 * 60 * ONE_MINUTE_NANOS, // in one day
        };
        self.requests
            .insert(internal_request.id, internal_request.clone());
        // this is required as LookupMap doesn't write state immediately
        // Bug4 -> https://docs.near.org/build/smart-contracts/anatomy/collections#error-prone-patterns
        self.requests.flush();

        internal_request
    }

    fn get_request_or_panic(&self, request_id: RequestId) -> &Request {
        // TODO: use errors from Enum
        self.requests.get(&request_id).expect("ERR_NOT_FOUND")
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use ethers_core::{
        abi::{Function, Param, ParamType, StateMutability, Token},
        types::U256,
    };
    use near_sdk::{json_types::U128, test_utils::VMContextBuilder, testing_env, Gas, NearToken};
    use primitives::{FunctionData, InputTransactionPayload, OtherEip1559TransactionPayload};

    fn current() -> AccountId {
        AccountId::from_str("current").unwrap()
    }

    fn user1() -> AccountId {
        AccountId::from_str("user1").unwrap()
    }

    fn user2() -> AccountId {
        AccountId::from_str("user2").unwrap()
    }

    fn signer() -> AccountId {
        AccountId::from_str("signer").unwrap()
    }

    fn setup() -> (Contract, VMContextBuilder) {
        let mut context = VMContextBuilder::new();
        let contract = Contract::new(signer());

        context.current_account_id(current());
        context.account_balance(NearToken::from_near(1));
        context.attached_deposit(NearToken::from_millinear(10));
        context.predecessor_account_id(user1());
        context.block_timestamp(0);
        context.prepaid_gas(Gas::from_tgas(300));

        testing_env!(context.build());

        (contract, context)
    }

    fn input_request() -> InputRequest {
        InputRequest {
            allowed_account_id: user1(),
            derivation_seed_number: 0,
            transaction_payload: InputTransactionPayload {
                to: "0x0000000000000000000000000000000000000000".to_string(),
                function_data: None,
                value: None,
                nonce: U128(0),
            },
            key_version: None,
        }
    }

    fn other_payload() -> OtherEip1559TransactionPayload {
        OtherEip1559TransactionPayload {
            chain_id: 1,
            gas: Some(U128(42_000)),
            max_fee_per_gas: U128(120_000),
            max_priority_fee_per_gas: U128(120_000),
        }
    }

    #[test]
    fn test_setup_succeeds() {
        setup();
    }

    #[test]
    fn test_register_signature_request() {
        let (mut contract, _) = setup();

        let input_request = input_request();
        contract.register_signature_request(input_request.clone());
    }

    #[test]
    fn test_register_signature_request_uses_different_ids() {
        let (mut contract, _) = setup();

        let input_request = input_request();
        let request_1 = contract.register_signature_request(input_request.clone());
        let request_2 = contract.register_signature_request(input_request.clone());

        assert_ne!(request_1.request_id, request_2.request_id);
    }

    #[test]
    fn test_register_signature_request_with_function_data() {
        let (mut contract, _) = setup();

        let mut input_request = input_request();
        input_request.transaction_payload.function_data = Some(FunctionData {
            function_abi: Function {
                name: "set".to_string(),
                inputs: vec![Param {
                    name: "_num".to_string(),
                    kind: ParamType::Uint(256),
                    internal_type: Some("uint256".to_string()),
                }],
                outputs: vec![],
                constant: None,
                state_mutability: StateMutability::NonPayable,
            },
            arguments: vec![Token::Uint(U256([2000, 0, 0, 0]))],
        });

        contract.register_signature_request(input_request.clone());
    }

    #[should_panic]
    #[test]
    fn test_register_signature_request_panics_on_invalid_function_arguments() {
        let (mut contract, _) = setup();

        let mut input_request = input_request();
        input_request.transaction_payload.function_data = Some(FunctionData {
            function_abi: Function {
                name: "set".to_string(),
                inputs: vec![Param {
                    name: "_num".to_string(),
                    kind: ParamType::Uint(256),
                    internal_type: Some("uint256".to_string()),
                }],
                outputs: vec![],
                constant: None,
                state_mutability: StateMutability::NonPayable,
            },
            arguments: vec![],
        });

        // must panic since no arguments are provided
        contract.register_signature_request(input_request.clone());
    }

    #[should_panic]
    #[test]
    fn test_register_signature_request_panics_on_wrong_address() {
        let (mut contract, _) = setup();

        let mut input_request = input_request();
        input_request.transaction_payload.to = "0xbajdo3i1o21o214".to_string();

        // must panic since address is invalid
        contract.register_signature_request(input_request.clone());
    }

    #[should_panic]
    #[test]
    fn test_register_signature_request_panics_on_small_deposit() {
        let (mut contract, mut context) = setup();

        context.attached_deposit(NearToken::from_yoctonear(100));
        testing_env!(context.build());

        let input_request = input_request();
        contract.register_signature_request(input_request.clone());
    }

    #[should_panic = "ERR_NOT_FOUND"]
    #[test]
    fn test_get_signature_panics_on_unexisted_request() {
        let (mut contract, _) = setup();

        let other_payload = other_payload();
        contract.get_signature(100, other_payload);
    }

    #[should_panic = "ERR_FORBIDDEN"]
    #[test]
    fn test_get_signature_panics_on_non_allowed_actor() {
        let (mut contract, mut context) = setup();

        let input_request = input_request();
        let request = contract.register_signature_request(input_request.clone());

        context.predecessor_account_id(user2());
        testing_env!(context.build());

        let other_payload = other_payload();
        contract.get_signature(request.request_id, other_payload);
    }

    #[should_panic = "ERR_TIME_IS_UP"]
    #[test]
    fn test_get_signature_panics_on_deadline() {
        let (mut contract, mut context) = setup();

        let input_request = input_request();
        let request = contract.register_signature_request(input_request.clone());

        // one day + a second
        context.block_timestamp(24 * 60 * ONE_MINUTE_NANOS + 1);
        testing_env!(context.build());

        let other_payload = other_payload();
        contract.get_signature(request.request_id, other_payload);
    }

    #[should_panic = "ERR_INSUFFICIENT_GAS"]
    #[test]
    fn test_get_signature_panics_on_insufficient_gas() {
        let (mut contract, mut context) = setup();

        let input_request = input_request();
        let request = contract.register_signature_request(input_request.clone());

        // 260TGas - 1Gas
        context.prepaid_gas(Gas::from_tgas(260).checked_sub(Gas::from_gas(1)).unwrap());
        testing_env!(context.build());

        let other_payload = other_payload();
        contract.get_signature(request.request_id, other_payload);
    }
}
