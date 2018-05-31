use std::sync::Arc;

use ethabi::ContractFunction;
use ethcore;
use ethcore::client::{EvmTestClient, TransactResult};
use ethcore_transaction::{Action, SignedTransaction, Transaction};
use ethereum_types::{Address, U256, H256, H160};
use vm;
use error;
use std::error::Error;
use std::fmt;

use trace;

#[derive(Debug)]
pub struct Evm {
    evm: EvmTestClient<'static>,
    sender: Address,
    contract_address: Option<Address>,
    value: U256,
    gas: U256,
    gas_price: U256,
    is_tracing_enabled: bool,
    logs: Vec<ethcore::log_entry::LogEntry>,
}

impl Default for Evm {
    fn default() -> Self {
        Evm::new_current()
    }
}

struct TransactSuccess<T, V> {
    /// State root
    state_root: H256,
    /// Amount of gas left
    gas_left: U256,
    /// Output
    output: Vec<u8>,
    /// Traces
    trace: Vec<T>,
    /// VM Traces
    vm_trace: Option<V>,
    /// Created contract address (if any)
    contract_address: Option<H160>,
    /// Generated logs
    logs: Vec<ethcore::log_entry::LogEntry>,
    /// outcome
    outcome: ethcore::receipt::TransactionOutcome,
}

struct TransactionOutput {
    state_root: H256,
    gas_left: U256,
    output: Vec<u8>,
    contract_address: Option<H160>,
    logs: Vec<ethcore::log_entry::LogEntry>,
    outcome: ethcore::receipt::TransactionOutcome,
}

impl<T, V> From<TransactSuccess<T, V>> for TransactionOutput {
    fn from(t: TransactSuccess<T, V>) -> Self {
        TransactionOutput {
            state_root: t.state_root,
            gas_left: t.gas_left,
            output: t.output,
            contract_address: t.contract_address,
            logs: t.logs,
            outcome: t.outcome,
        }
    }
}

#[derive(Debug)]
pub struct TransactError {
    /// State root
    state_root: H256,
    /// Execution error
    error: ethcore::error::Error,
}

impl Error for TransactError {
    fn description(&self) -> &str {
        "error transacting with the test evm"
    }
}

impl fmt::Display for TransactError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

fn split_transact_result<T, V>(result: TransactResult<T, V>) -> Result<TransactSuccess<T, V>, TransactError> {
    match result {
        TransactResult::Ok {
            state_root,
            gas_left,
            output,
            trace,
            vm_trace,
            contract_address,
            logs,
            outcome,
        } => Ok(TransactSuccess {
            state_root,
            gas_left,
            output,
            trace,
            vm_trace,
            contract_address,
            logs,
            outcome,
        }),
        TransactResult::Err { state_root, error } => Err(TransactError { state_root, error }),
    }
}

impl Evm {
    pub fn new_current() -> Self {
        let evm = EvmTestClient::new(&*::FOUNDATION).expect("Valid spec given; qed");
        Evm {
            evm,
            sender: 0.into(),
            contract_address: None,
            gas: 4_000_000.into(),
            gas_price: 0.into(),
            value: 0.into(),
            logs: vec![],
            is_tracing_enabled: false,
        }
    }

    fn tracers(&self) -> (trace::PrintingTracer, trace::PrintingTracer) {
        Default::default()
    }

    fn env_info(&self) -> vm::EnvInfo {
        vm::EnvInfo {
            number: 5_000_000u64,
            author: 0.into(),
            timestamp: 1u64,
            difficulty: 1.into(),
            last_hashes: Arc::new([0.into(); 256].to_vec()),
            gas_used: 0.into(),
            gas_limit: 4_700_000.into(),
        }
    }

    pub fn deploy(&mut self, code: &[u8]) -> error::Result<Address> {
        let env_info = self.env_info();
        let nonce = self.evm.state().nonce(&self.sender).expect(STATE);
        let transaction = Transaction {
            nonce,
            gas_price: self.gas_price,
            gas: self.gas,
            action: Action::Create,
            value: self.value,
            data: code.to_vec(),
        }.fake_sign((&*self.sender).into());

        let transaction_output = self.raw_transact(&env_info, transaction)?;

        let contract_address = transaction_output.contract_address
            .expect("transaction output must have contract_address after deploy");

        self.contract_address = Some(contract_address);

        Ok(contract_address)
    }

    pub fn with_gas(&mut self, gas: U256) -> &mut Self {
        self.gas = gas;
        self
    }

    pub fn with_gas_price(&mut self, gas_price: U256) -> &mut Self {
        self.gas_price = gas_price;
        self
    }

    pub fn with_value(&mut self, value: U256) -> &mut Self {
        self.value = value;
        self
    }

    pub fn with_sender(&mut self, address: Address) -> &mut Self {
        self.sender = address;
        self
    }

    pub fn with_tracing(&mut self, is_tracing_enabled: bool) -> &mut Self {
        self.is_tracing_enabled = is_tracing_enabled;
        self
    }

    /// Ensures that sender has enough funds (value) to call next transaction.
    pub fn ensure_funds(&mut self) -> &mut Self {
        // TODO [ToDr] Just transfer to amount that is actually needed
        let env_info = self.env_info();
        let sender = "7c532DB9E0c06C26fd40Acc56AC55C1eE92D3C3A".parse().unwrap();
        let nonce = self.evm.state().nonce(&sender).expect(STATE);
        let transaction = Transaction {
            nonce,
            gas_price: 0.into(),
            // supplying a bit more than 21k if people use builtin addresses as destinations.
            // builtins have different pricing schemes.
            gas: 22_000.into(),
            action: Action::Call(self.sender),
            value: self.value,
            data: vec![],
        }.fake_sign(sender);

        self.raw_transact(&env_info, transaction).expect("Unable to top up account.");
        self
    }

    pub fn logs(&self, _filter: ::ethabi::TopicFilter) -> Vec<()> {
        // TODO [ToDr] Add filter querying
        self.logs.iter().map(|_| ()).collect()
    }

    /// Run the EVM and panic on all errors.
    pub fn run<F>(self, func: F)
    where
        F: FnOnce(Self) -> ::ethabi::Result<()>,
    {
        func(self).expect("Unexpected error occured.");
    }

    fn call<F: ContractFunction>(&mut self, f: F) -> error::Result<F::Output> {
        let contract_address = self.contract_address
            .expect("Contract address is not set. Did you forget to deploy the contract?");
        let mut params = vm::ActionParams::default();
        params.sender = self.sender;
        params.origin = self.sender;
        params.address = contract_address;
        params.code_address = contract_address;
        params.code = self.evm.state().code(&contract_address).expect(STATE);
        params.data = Some(f.encoded());
        params.call_type = vm::CallType::Call;
        params.value = vm::ActionValue::Transfer(self.value);
        params.gas = self.gas;
        params.gas_price = self.gas_price;

        let result = if self.is_tracing_enabled {
            let mut tracers = self.tracers();
            self.evm.call(params, &mut tracers.0, &mut tracers.1)?
        } else {
            self.evm.call(params, &mut ethcore::trace::NoopTracer, &mut ethcore::trace::NoopVMTracer)?
        };

        let output = f.output(result.return_data.to_vec())
            .expect("output must be decodable with `ContractFunction` that has encoded input. q.e.d.");
         Ok(output)
    }

    fn raw_transact(&mut self, env_info: &vm::EnvInfo, transaction: SignedTransaction) -> error::Result<TransactionOutput> {
        if self.is_tracing_enabled {
            let mut tracers = self.tracers();
            Ok(split_transact_result(self.evm.transact(env_info, transaction, tracers.0, tracers.1))?.into())
        } else {
            let transact_success = split_transact_result(self.evm.transact(env_info, transaction, ethcore::trace::NoopTracer, ethcore::trace::NoopVMTracer))?;
            self.logs.extend(transact_success.logs.clone());
            Ok(transact_success.into())
        }
    }

    fn transact<F: ContractFunction>(&mut self, f: F) -> error::Result<TransactionOutput> {
        let contract_address = self.contract_address
            .expect("Contract address is not set. Did you forget to deploy the contract?");
        let env_info = self.env_info();
        let nonce = self.evm.state().nonce(&self.sender).expect(STATE);
        let transaction = Transaction {
            nonce,
            gas_price: self.gas_price,
            gas: self.gas,
            action: Action::Call(contract_address),
            value: self.value,
            data: f.encoded(),
        }.fake_sign(self.sender);

        self.raw_transact(&env_info, transaction)
    }
}

const STATE: &str = "State failure.";
