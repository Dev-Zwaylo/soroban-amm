#![no_std]

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, Address, BytesN, Env,
    Symbol, Vec,
};

const PRICE_SCALE: i128 = 1_000_000;
const POW_SCALE: i128 = 1_000_000_000_000;
const EVENT_SCHEMA_VERSION: u32 = 1;

macro_rules! emit_versioned_event {
    ($env:expr, $topic:expr, $payload:expr) => {{
        $env.events()
            .publish($topic, (EVENT_SCHEMA_VERSION, $payload));
    }};
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdapterMode {
    Amm,
    ConcentratedLiquidity,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    PendingAdmin,
    Paused,
    Factory,
    Consumer,
    Mode,
    Window,
    MaxAge,
    FeeTiers,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AdapterError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidConfiguration = 3,
    PoolNotFound = 4,
    NoSnapshot = 5,
    StaleSnapshot = 6,
    InvalidPrice = 7,
    ArithmeticOverflow = 8,
    NoPendingAdmin = 9,
    WrongPendingAdmin = 10,
    Paused = 11,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TwapResult {
    pub twap: i128,
    pub window_used: u64,
    pub snapshot_timestamp: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClTwapResult {
    pub average_tick: i64,
    pub window_used: u64,
    pub snapshot_timestamp: u64,
}

// Only the generated `FactoryClient` is used; the trait is its source of truth.
#[allow(dead_code)]
#[contractclient(name = "FactoryClient")]
trait Factory {
    fn get_pool(env: Env, token_a: Address, token_b: Address) -> Option<Address>;
    fn get_cl_pool(env: Env, token_a: Address, token_b: Address, fee_bps: i128) -> Option<Address>;
    fn get_pool_tokens(env: Env, pool: Address) -> Option<(Address, Address)>;
}

// Only the generated `TwapConsumerClient` is used; the trait is its source of truth.
#[allow(dead_code)]
#[contractclient(name = "TwapConsumerClient")]
trait TwapConsumer {
    fn get_twap_price_detailed(env: Env, pool: Address, window_seconds: u64) -> TwapResult;
    fn get_cl_twap_detailed(env: Env, pool: Address, window_seconds: u64) -> ClTwapResult;
    fn get_latest_snapshot_timestamp(env: Env, pool: Address) -> u64;
}

// Only the generated `ClPoolClient` is used; the trait is its source of truth.
#[allow(dead_code)]
#[contractclient(name = "ClPoolClient")]
trait ClPool {
    fn active_liquidity(env: Env) -> i128;
}

#[contract]
pub struct OracleTwapAdapter;

#[contractimpl]
impl OracleTwapAdapter {
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        env: Env,
        admin: Address,
        factory: Address,
        consumer: Address,
        mode: AdapterMode,
        window_seconds: u64,
        max_age_seconds: u64,
        fee_tiers: Vec<i128>,
    ) -> Result<(), AdapterError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(AdapterError::AlreadyInitialized);
        }
        if window_seconds == 0 || max_age_seconds == 0 {
            return Err(AdapterError::InvalidConfiguration);
        }
        if mode == AdapterMode::ConcentratedLiquidity && fee_tiers.is_empty() {
            return Err(AdapterError::InvalidConfiguration);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Factory, &factory);
        env.storage().instance().set(&DataKey::Consumer, &consumer);
        env.storage().instance().set(&DataKey::Mode, &mode);
        env.storage()
            .instance()
            .set(&DataKey::Window, &window_seconds);
        env.storage()
            .instance()
            .set(&DataKey::MaxAge, &max_age_seconds);
        env.storage().instance().set(&DataKey::FeeTiers, &fee_tiers);
        Ok(())
    }

    pub fn quote(
        env: Env,
        token_a: Address,
        token_b: Address,
    ) -> Result<(i128, u64), AdapterError> {
        let factory: Address = Self::read(&env, DataKey::Factory)?;
        if env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(AdapterError::Paused);
        }
        let consumer: Address = Self::read(&env, DataKey::Consumer)?;
        let mode: AdapterMode = Self::read(&env, DataKey::Mode)?;
        let window: u64 = Self::read(&env, DataKey::Window)?;
        let max_age: u64 = Self::read(&env, DataKey::MaxAge)?;
        let factory_client = FactoryClient::new(&env, &factory);

        let pool = match mode {
            AdapterMode::Amm => factory_client
                .get_pool(&token_a, &token_b)
                .ok_or(AdapterError::PoolNotFound)?,
            AdapterMode::ConcentratedLiquidity => {
                let tiers: Vec<i128> = Self::read(&env, DataKey::FeeTiers)?;
                let mut selected: Option<(Address, i128)> = None;
                for tier in tiers.iter() {
                    if let Some(candidate) = factory_client.get_cl_pool(&token_a, &token_b, &tier) {
                        let liquidity = ClPoolClient::new(&env, &candidate).active_liquidity();
                        if selected
                            .as_ref()
                            .map(|(_, l)| liquidity > *l)
                            .unwrap_or(true)
                        {
                            selected = Some((candidate, liquidity));
                        }
                    }
                }
                selected
                    .map(|(pool, _)| pool)
                    .ok_or(AdapterError::PoolNotFound)?
            }
        };

        let newest = TwapConsumerClient::new(&env, &consumer).get_latest_snapshot_timestamp(&pool);
        if newest == 0 {
            return Err(AdapterError::NoSnapshot);
        }
        if env.ledger().timestamp().saturating_sub(newest) > max_age {
            return Err(AdapterError::StaleSnapshot);
        }

        let mut price = match mode {
            AdapterMode::Amm => {
                TwapConsumerClient::new(&env, &consumer)
                    .get_twap_price_detailed(&pool, &window)
                    .twap
            }
            AdapterMode::ConcentratedLiquidity => {
                let result =
                    TwapConsumerClient::new(&env, &consumer).get_cl_twap_detailed(&pool, &window);
                Self::tick_to_price(result.average_tick)?
            }
        };
        if price <= 0 {
            return Err(AdapterError::InvalidPrice);
        }

        let pool_tokens = factory_client
            .get_pool_tokens(&pool)
            .ok_or(AdapterError::PoolNotFound)?;
        if pool_tokens.0 != token_a {
            price = PRICE_SCALE
                .checked_mul(PRICE_SCALE)
                .ok_or(AdapterError::ArithmeticOverflow)?
                / price;
        }
        Ok((price, newest))
    }

    pub fn get_config(env: Env) -> Result<(Address, Address, AdapterMode, u64, u64), AdapterError> {
        Ok((
            Self::read(&env, DataKey::Factory)?,
            Self::read(&env, DataKey::Consumer)?,
            Self::read(&env, DataKey::Mode)?,
            Self::read(&env, DataKey::Window)?,
            Self::read(&env, DataKey::MaxAge)?,
        ))
    }

    pub fn get_fee_tiers(env: Env) -> Result<Vec<i128>, AdapterError> {
        Self::read(&env, DataKey::FeeTiers)
    }

    pub fn get_admin(env: Env) -> Result<Address, AdapterError> {
        Self::read(&env, DataKey::Admin)
    }

    pub fn get_pending_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PendingAdmin)
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Nominate a new admin. Admin-only; the nominee must separately authorize `accept_admin`.
    pub fn propose_admin(env: Env, new_admin: Address) -> Result<(), AdapterError> {
        Self::require_admin(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        emit_versioned_event!(&env, (Symbol::new(&env, "admin_proposed"),), (new_admin,));
        Ok(())
    }

    /// Complete a two-step admin rotation started by `propose_admin`.
    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), AdapterError> {
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(AdapterError::NoPendingAdmin)?;
        if new_admin != pending {
            return Err(AdapterError::WrongPendingAdmin);
        }
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        emit_versioned_event!(&env, (Symbol::new(&env, "admin_changed"),), (new_admin,));
        Ok(())
    }

    /// Update the factory contract address. Admin-only.
    pub fn set_factory(env: Env, factory: Address) -> Result<(), AdapterError> {
        Self::require_admin(&env)?;
        env.storage().instance().set(&DataKey::Factory, &factory);
        emit_versioned_event!(&env, (Symbol::new(&env, "factory_updated"),), (factory,));
        Ok(())
    }

    /// Update the TWAP consumer contract address. Admin-only.
    pub fn set_consumer(env: Env, consumer: Address) -> Result<(), AdapterError> {
        Self::require_admin(&env)?;
        env.storage().instance().set(&DataKey::Consumer, &consumer);
        emit_versioned_event!(&env, (Symbol::new(&env, "consumer_updated"),), (consumer,));
        Ok(())
    }

    /// Update the adapter mode. Switching to `ConcentratedLiquidity` requires non-empty `fee_tiers`. Admin-only.
    pub fn set_mode(env: Env, mode: AdapterMode) -> Result<(), AdapterError> {
        Self::require_admin(&env)?;
        if mode == AdapterMode::ConcentratedLiquidity {
            let tiers: Vec<i128> = Self::read(&env, DataKey::FeeTiers)?;
            if tiers.is_empty() {
                return Err(AdapterError::InvalidConfiguration);
            }
        }
        env.storage().instance().set(&DataKey::Mode, &mode);
        emit_versioned_event!(&env, (Symbol::new(&env, "mode_updated"),), (mode,));
        Ok(())
    }

    /// Update the TWAP window in seconds. Must be non-zero. Admin-only.
    pub fn set_window(env: Env, window_seconds: u64) -> Result<(), AdapterError> {
        Self::require_admin(&env)?;
        if window_seconds == 0 {
            return Err(AdapterError::InvalidConfiguration);
        }
        env.storage()
            .instance()
            .set(&DataKey::Window, &window_seconds);
        emit_versioned_event!(
            &env,
            (Symbol::new(&env, "window_updated"),),
            (window_seconds,)
        );
        Ok(())
    }

    /// Update the maximum snapshot age in seconds. Must be non-zero. Admin-only.
    pub fn set_max_age(env: Env, max_age_seconds: u64) -> Result<(), AdapterError> {
        Self::require_admin(&env)?;
        if max_age_seconds == 0 {
            return Err(AdapterError::InvalidConfiguration);
        }
        env.storage()
            .instance()
            .set(&DataKey::MaxAge, &max_age_seconds);
        emit_versioned_event!(
            &env,
            (Symbol::new(&env, "max_age_updated"),),
            (max_age_seconds,)
        );
        Ok(())
    }

    /// Update the CL fee tiers. Must be non-empty when mode is `ConcentratedLiquidity`. Admin-only.
    pub fn set_fee_tiers(env: Env, fee_tiers: Vec<i128>) -> Result<(), AdapterError> {
        Self::require_admin(&env)?;
        let mode: AdapterMode = Self::read(&env, DataKey::Mode)?;
        if mode == AdapterMode::ConcentratedLiquidity && fee_tiers.is_empty() {
            return Err(AdapterError::InvalidConfiguration);
        }
        env.storage().instance().set(&DataKey::FeeTiers, &fee_tiers);
        emit_versioned_event!(
            &env,
            (Symbol::new(&env, "fee_tiers_updated"),),
            (fee_tiers,)
        );
        Ok(())
    }

    /// Pause quote reads. Admin-only.
    pub fn pause(env: Env) -> Result<(), AdapterError> {
        Self::require_admin(&env)?;
        env.storage().instance().set(&DataKey::Paused, &true);
        emit_versioned_event!(&env, (Symbol::new(&env, "paused"),), (true,));
        Ok(())
    }

    /// Resume quote reads. Admin-only.
    pub fn unpause(env: Env) -> Result<(), AdapterError> {
        Self::require_admin(&env)?;
        env.storage().instance().set(&DataKey::Paused, &false);
        emit_versioned_event!(&env, (Symbol::new(&env, "unpaused"),), (false,));
        Ok(())
    }

    /// Replace this contract's WASM. Admin-only; stored config is preserved.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), AdapterError> {
        Self::require_admin(&env)?;
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        emit_versioned_event!(&env, (Symbol::new(&env, "upgraded"),), (new_wasm_hash,));
        Ok(())
    }

    fn require_admin(env: &Env) -> Result<(), AdapterError> {
        Self::read::<Address>(env, DataKey::Admin)?.require_auth();
        Ok(())
    }

    fn read<T>(env: &Env, key: DataKey) -> Result<T, AdapterError>
    where
        T: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>,
        T::Error: core::fmt::Debug,
    {
        env.storage()
            .instance()
            .get(&key)
            .ok_or(AdapterError::NotInitialized)
    }

    fn tick_to_price(tick: i64) -> Result<i128, AdapterError> {
        let negative = tick < 0;
        let mut exponent = tick.unsigned_abs();
        let mut factor = 1_000_100_000_000_i128;
        let mut ratio = POW_SCALE;
        while exponent > 0 {
            if exponent & 1 == 1 {
                ratio = ratio
                    .checked_mul(factor)
                    .ok_or(AdapterError::ArithmeticOverflow)?
                    / POW_SCALE;
            }
            exponent >>= 1;
            if exponent > 0 {
                factor = factor
                    .checked_mul(factor)
                    .ok_or(AdapterError::ArithmeticOverflow)?
                    / POW_SCALE;
            }
        }
        if negative {
            ratio = POW_SCALE
                .checked_mul(POW_SCALE)
                .ok_or(AdapterError::ArithmeticOverflow)?
                / ratio;
        }
        ratio
            .checked_mul(PRICE_SCALE)
            .ok_or(AdapterError::ArithmeticOverflow)
            .map(|value| value / POW_SCALE)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Events, Ledger, MockAuth, MockAuthInvoke},
        vec, Address, Bytes, BytesN, Env, IntoVal, Symbol, TryFromVal, Val,
    };

    #[contracttype]
    #[derive(Clone)]
    enum FactoryKey {
        AmmPool(Address, Address),
        PoolTokens(Address),
    }

    #[contract]
    struct MockFactory;

    #[contractimpl]
    impl MockFactory {
        pub fn set_pool(env: Env, token_a: Address, token_b: Address, pool: Address) {
            env.storage()
                .instance()
                .set(&FactoryKey::AmmPool(token_a, token_b), &pool);
        }

        pub fn set_pool_tokens(env: Env, pool: Address, token_0: Address, token_1: Address) {
            env.storage()
                .instance()
                .set(&FactoryKey::PoolTokens(pool), &(token_0, token_1));
        }

        pub fn get_pool(env: Env, token_a: Address, token_b: Address) -> Option<Address> {
            env.storage()
                .instance()
                .get(&FactoryKey::AmmPool(token_a, token_b))
        }

        pub fn get_cl_pool(
            _env: Env,
            _token_a: Address,
            _token_b: Address,
            _fee_bps: i128,
        ) -> Option<Address> {
            None
        }

        pub fn get_pool_tokens(env: Env, pool: Address) -> Option<(Address, Address)> {
            env.storage().instance().get(&FactoryKey::PoolTokens(pool))
        }
    }

    #[contracttype]
    #[derive(Clone)]
    enum ConsumerKey {
        LatestTs(Address),
        AmmTwap(Address),
    }

    #[contract]
    struct MockTwapConsumer;

    #[contractimpl]
    impl MockTwapConsumer {
        pub fn set_latest_timestamp(env: Env, pool: Address, ts: u64) {
            env.storage()
                .instance()
                .set(&ConsumerKey::LatestTs(pool), &ts);
        }

        pub fn set_amm_twap(env: Env, pool: Address, result: TwapResult) {
            env.storage()
                .instance()
                .set(&ConsumerKey::AmmTwap(pool), &result);
        }

        pub fn get_twap_price_detailed(
            env: Env,
            pool: Address,
            _window_seconds: u64,
        ) -> TwapResult {
            env.storage()
                .instance()
                .get(&ConsumerKey::AmmTwap(pool))
                .expect("amm twap not configured")
        }

        pub fn get_cl_twap_detailed(
            _env: Env,
            _pool: Address,
            _window_seconds: u64,
        ) -> ClTwapResult {
            panic!("not used")
        }

        pub fn get_latest_snapshot_timestamp(env: Env, pool: Address) -> u64 {
            env.storage()
                .instance()
                .get(&ConsumerKey::LatestTs(pool))
                .unwrap_or(0)
        }
    }

    fn setup_adapter(env: &Env) -> (Address, Address, Address, OracleTwapAdapterClient<'_>) {
        let admin = Address::generate(env);
        let factory = env.register_contract(None, MockFactory);
        let consumer = env.register_contract(None, MockTwapConsumer);
        let adapter_id = env.register_contract(None, OracleTwapAdapter);
        let adapter = OracleTwapAdapterClient::new(env, &adapter_id);
        adapter.initialize(
            &admin,
            &factory,
            &consumer,
            &AdapterMode::Amm,
            &300,
            &600,
            &vec![env],
        );
        (admin, factory, consumer, adapter)
    }

    fn has_versioned_event(env: &Env, contract: &Address, name: &str) -> bool {
        let expected_sym = Symbol::new(env, name);
        env.events().all().iter().any(|(addr, topics, data)| {
            if addr != *contract || topics.len() != 1 {
                return false;
            }
            let topic: Option<Symbol> = topics
                .get(0)
                .and_then(|v: Val| Symbol::try_from_val(env, &v).ok());
            let version: Option<u32> = <(u32, Val)>::try_from_val(env, &data).ok().map(|(v, _)| v);
            topic == Some(expected_sym.clone()) && version == Some(EVENT_SCHEMA_VERSION)
        })
    }

    #[test]
    fn test_admin_setters_update_config_and_emit_events() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, _factory, _consumer, adapter) = setup_adapter(&env);

        let new_factory = Address::generate(&env);
        let new_consumer = Address::generate(&env);
        let new_tiers = vec![&env, 5, 30, 100];

        adapter.set_factory(&new_factory);
        assert!(has_versioned_event(
            &env,
            &adapter.address,
            "factory_updated"
        ));

        adapter.set_consumer(&new_consumer);
        assert!(has_versioned_event(
            &env,
            &adapter.address,
            "consumer_updated"
        ));

        adapter.set_fee_tiers(&new_tiers);
        assert!(has_versioned_event(
            &env,
            &adapter.address,
            "fee_tiers_updated"
        ));

        adapter.set_mode(&AdapterMode::ConcentratedLiquidity);
        assert!(has_versioned_event(&env, &adapter.address, "mode_updated"));

        adapter.set_window(&900);
        assert!(has_versioned_event(
            &env,
            &adapter.address,
            "window_updated"
        ));

        adapter.set_max_age(&1_800);
        assert!(has_versioned_event(
            &env,
            &adapter.address,
            "max_age_updated"
        ));

        let (factory, consumer, mode, window, max_age) = adapter.get_config();
        assert_eq!(factory, new_factory);
        assert_eq!(consumer, new_consumer);
        assert_eq!(mode, AdapterMode::ConcentratedLiquidity);
        assert_eq!(window, 900);
        assert_eq!(max_age, 1_800);
        assert_eq!(adapter.get_fee_tiers(), new_tiers);
    }

    #[test]
    fn test_non_admin_rejected_on_every_setter() {
        let env = Env::default();
        let (_admin, _factory, _consumer, adapter) = setup_adapter(&env);
        let attacker = Address::generate(&env);
        let addr = Address::generate(&env);
        let tiers = vec![&env, 30];

        assert!(adapter
            .mock_auths(&[MockAuth {
                address: &attacker,
                invoke: &MockAuthInvoke {
                    contract: &adapter.address,
                    fn_name: "set_factory",
                    args: (addr.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_set_factory(&addr)
            .is_err());

        assert!(adapter.try_set_consumer(&addr).is_err());
        assert!(adapter.try_set_mode(&AdapterMode::Amm).is_err());
        assert!(adapter.try_set_window(&600).is_err());
        assert!(adapter.try_set_max_age(&1200).is_err());
        assert!(adapter.try_set_fee_tiers(&tiers).is_err());
        assert!(adapter.try_propose_admin(&attacker).is_err());
        assert!(adapter.try_pause().is_err());
        assert!(adapter.try_unpause().is_err());
        assert!(adapter
            .try_upgrade(&BytesN::from_array(&env, &[1u8; 32]))
            .is_err());
    }

    #[test]
    fn test_setter_validation_rejects_invalid_inputs() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, _factory, _consumer, adapter) = setup_adapter(&env);

        // Zero window or max_age rejected
        assert_eq!(
            adapter.try_set_window(&0),
            Err(Ok(AdapterError::InvalidConfiguration))
        );
        assert_eq!(
            adapter.try_set_max_age(&0),
            Err(Ok(AdapterError::InvalidConfiguration))
        );

        // Switching to CL mode when fee_tiers is empty is rejected
        assert_eq!(
            adapter.try_set_mode(&AdapterMode::ConcentratedLiquidity),
            Err(Ok(AdapterError::InvalidConfiguration))
        );

        // Setting non-empty tiers then switching to CL succeeds
        adapter.set_fee_tiers(&vec![&env, 30]);
        adapter.set_mode(&AdapterMode::ConcentratedLiquidity);

        // Clearing fee_tiers while in CL mode is rejected
        assert_eq!(
            adapter.try_set_fee_tiers(&vec![&env]),
            Err(Ok(AdapterError::InvalidConfiguration))
        );
    }

    #[test]
    fn test_two_step_admin_rotation_happy_path_and_errors() {
        let env = Env::default();
        let (old_admin, _factory, _consumer, adapter) = setup_adapter(&env);
        let new_admin = Address::generate(&env);
        let stranger = Address::generate(&env);

        assert_eq!(adapter.get_admin(), old_admin);
        assert_eq!(adapter.get_pending_admin(), None);

        // Accepting before any proposal fails
        assert_eq!(
            adapter.try_accept_admin(&new_admin),
            Err(Ok(AdapterError::NoPendingAdmin))
        );

        // Old admin proposes new admin
        adapter
            .mock_auths(&[MockAuth {
                address: &old_admin,
                invoke: &MockAuthInvoke {
                    contract: &adapter.address,
                    fn_name: "propose_admin",
                    args: (new_admin.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .propose_admin(&new_admin);
        assert_eq!(adapter.get_pending_admin(), Some(new_admin.clone()));
        assert!(has_versioned_event(
            &env,
            &adapter.address,
            "admin_proposed"
        ));

        // Wrong address cannot accept
        assert_eq!(
            adapter.try_accept_admin(&stranger),
            Err(Ok(AdapterError::WrongPendingAdmin))
        );

        // New admin accepts
        adapter
            .mock_auths(&[MockAuth {
                address: &new_admin,
                invoke: &MockAuthInvoke {
                    contract: &adapter.address,
                    fn_name: "accept_admin",
                    args: (new_admin.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .accept_admin(&new_admin);
        assert_eq!(adapter.get_admin(), new_admin);
        assert_eq!(adapter.get_pending_admin(), None);
        assert!(has_versioned_event(&env, &adapter.address, "admin_changed"));

        // Old admin has lost authority
        assert!(adapter
            .mock_auths(&[MockAuth {
                address: &old_admin,
                invoke: &MockAuthInvoke {
                    contract: &adapter.address,
                    fn_name: "set_window",
                    args: (900u64,).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .try_set_window(&900)
            .is_err());

        // New admin can update config
        adapter
            .mock_auths(&[MockAuth {
                address: &new_admin,
                invoke: &MockAuthInvoke {
                    contract: &adapter.address,
                    fn_name: "set_window",
                    args: (900u64,).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .set_window(&900);
        assert_eq!(adapter.get_config().3, 900);
    }

    #[test]
    fn test_pause_and_unpause_gates_quote() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);
        let (_admin, factory_id, consumer_id, adapter) = setup_adapter(&env);

        let token_a = Address::generate(&env);
        let token_b = Address::generate(&env);
        let pool = Address::generate(&env);

        let factory = MockFactoryClient::new(&env, &factory_id);
        let consumer = MockTwapConsumerClient::new(&env, &consumer_id);
        factory.set_pool(&token_a, &token_b, &pool);
        factory.set_pool_tokens(&pool, &token_a, &token_b);
        consumer.set_latest_timestamp(&pool, &950);
        consumer.set_amm_twap(
            &pool,
            &TwapResult {
                twap: 2_000_000,
                window_used: 300,
                snapshot_timestamp: 950,
            },
        );

        assert!(!adapter.is_paused());
        assert_eq!(adapter.quote(&token_a, &token_b), (2_000_000, 950));

        adapter.pause();
        assert!(adapter.is_paused());
        assert!(has_versioned_event(&env, &adapter.address, "paused"));
        assert_eq!(
            adapter.try_quote(&token_a, &token_b),
            Err(Ok(AdapterError::Paused))
        );

        adapter.unpause();
        assert!(!adapter.is_paused());
        assert!(has_versioned_event(&env, &adapter.address, "unpaused"));
        assert_eq!(adapter.quote(&token_a, &token_b), (2_000_000, 950));
    }

    #[test]
    fn test_upgrade_is_admin_gated_and_preserves_config() {
        let env = Env::default();
        let (admin, factory, consumer, adapter) = setup_adapter(&env);

        // Minimal valid Soroban WASM module (header + `contractenvmetav0` custom section for protocol 21)
        const MINIMAL_SOROBAN_WASM: &[u8] = &[
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // WASM magic + version 1
            0x00, 0x1e, // custom section, 30 bytes
            0x11, // name len = 17
            b'c', b'o', b'n', b't', b'r', b'a', b'c', b't', b'e', b'n', b'v', b'm', b'e', b't',
            b'a', b'v', b'0', // "contractenvmetav0"
            0x00, 0x00, 0x00, 0x00, // ScEnvMetaKindInterfaceVersion = 0
            0x00, 0x00, 0x00, 0x15, // protocol = 21
            0x00, 0x00, 0x00, 0x00, // pre_release = 0
        ];
        let wasm = Bytes::from_slice(&env, MINIMAL_SOROBAN_WASM);
        let wasm_hash = env.deployer().upload_contract_wasm(wasm);

        // Non-admin rejected
        assert!(adapter.try_upgrade(&wasm_hash).is_err());

        // Admin succeeds and preserves stored config
        adapter
            .mock_auths(&[MockAuth {
                address: &admin,
                invoke: &MockAuthInvoke {
                    contract: &adapter.address,
                    fn_name: "upgrade",
                    args: (wasm_hash.clone(),).into_val(&env),
                    sub_invokes: &[],
                },
            }])
            .upgrade(&wasm_hash);

        assert!(has_versioned_event(&env, &adapter.address, "upgraded"));
        env.as_contract(&adapter.address, || {
            assert_eq!(
                env.storage().instance().get::<_, Address>(&DataKey::Admin),
                Some(admin.clone())
            );
            assert_eq!(
                env.storage()
                    .instance()
                    .get::<_, Address>(&DataKey::Factory),
                Some(factory.clone())
            );
            assert_eq!(
                env.storage()
                    .instance()
                    .get::<_, Address>(&DataKey::Consumer),
                Some(consumer.clone())
            );
            assert_eq!(
                env.storage()
                    .instance()
                    .get::<_, AdapterMode>(&DataKey::Mode),
                Some(AdapterMode::Amm)
            );
            assert_eq!(
                env.storage().instance().get::<_, u64>(&DataKey::Window),
                Some(300)
            );
            assert_eq!(
                env.storage().instance().get::<_, u64>(&DataKey::MaxAge),
                Some(600)
            );
        });
    }
}
