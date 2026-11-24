#![no_std]

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, Address, Env, Vec,
};

const PRICE_SCALE: i128 = 1_000_000;
const POW_SCALE: i128 = 1_000_000_000_000;

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
mod unit_tests {
    extern crate std;

    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        vec, Address, Env,
    };

    #[contracttype]
    #[derive(Clone)]
    enum FactoryKey {
        AmmPool(Address, Address),
        ClPool(Address, Address, i128),
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

        pub fn set_cl_pool(
            env: Env,
            token_a: Address,
            token_b: Address,
            fee_bps: i128,
            pool: Address,
        ) {
            env.storage()
                .instance()
                .set(&FactoryKey::ClPool(token_a, token_b, fee_bps), &pool);
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
            env: Env,
            token_a: Address,
            token_b: Address,
            fee_bps: i128,
        ) -> Option<Address> {
            env.storage()
                .instance()
                .get(&FactoryKey::ClPool(token_a, token_b, fee_bps))
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
        ClTwap(Address),
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

        pub fn set_cl_twap(env: Env, pool: Address, result: ClTwapResult) {
            env.storage()
                .instance()
                .set(&ConsumerKey::ClTwap(pool), &result);
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

        pub fn get_cl_twap_detailed(env: Env, pool: Address, _window_seconds: u64) -> ClTwapResult {
            env.storage()
                .instance()
                .get(&ConsumerKey::ClTwap(pool))
                .expect("cl twap not configured")
        }

        pub fn get_latest_snapshot_timestamp(env: Env, pool: Address) -> u64 {
            env.storage()
                .instance()
                .get(&ConsumerKey::LatestTs(pool))
                .unwrap_or(0)
        }
    }

    #[contracttype]
    #[derive(Clone)]
    enum ClPoolKey {
        Liquidity,
    }

    #[contract]
    struct MockClPool;

    #[contractimpl]
    impl MockClPool {
        pub fn set_liquidity(env: Env, liquidity: i128) {
            env.storage()
                .instance()
                .set(&ClPoolKey::Liquidity, &liquidity);
        }

        pub fn active_liquidity(env: Env) -> i128 {
            env.storage()
                .instance()
                .get(&ClPoolKey::Liquidity)
                .unwrap_or(0)
        }
    }

    struct TestFixture<'a> {
        admin: Address,
        factory_id: Address,
        factory: MockFactoryClient<'a>,
        consumer_id: Address,
        consumer: MockTwapConsumerClient<'a>,
        adapter: OracleTwapAdapterClient<'a>,
        token_a: Address,
        token_b: Address,
    }

    fn setup(env: &Env) -> TestFixture<'_> {
        let admin = Address::generate(env);
        let factory_id = env.register_contract(None, MockFactory);
        let consumer_id = env.register_contract(None, MockTwapConsumer);
        let adapter_id = env.register_contract(None, OracleTwapAdapter);

        TestFixture {
            admin,
            factory_id: factory_id.clone(),
            factory: MockFactoryClient::new(env, &factory_id),
            consumer_id: consumer_id.clone(),
            consumer: MockTwapConsumerClient::new(env, &consumer_id),
            adapter: OracleTwapAdapterClient::new(env, &adapter_id),
            token_a: Address::generate(env),
            token_b: Address::generate(env),
        }
    }

    #[test]
    fn test_initialize_and_get_config() {
        let env = Env::default();
        let f = setup(&env);

        f.adapter.initialize(
            &f.admin,
            &f.factory_id,
            &f.consumer_id,
            &AdapterMode::Amm,
            &300,
            &600,
            &vec![&env],
        );

        let (factory, consumer, mode, window, max_age) = f.adapter.get_config();
        assert_eq!(factory, f.factory_id);
        assert_eq!(consumer, f.consumer_id);
        assert_eq!(mode, AdapterMode::Amm);
        assert_eq!(window, 300);
        assert_eq!(max_age, 600);

        let err = f.adapter.try_initialize(
            &f.admin,
            &f.factory_id,
            &f.consumer_id,
            &AdapterMode::Amm,
            &300,
            &600,
            &vec![&env],
        );
        assert_eq!(err, Err(Ok(AdapterError::AlreadyInitialized)));
    }

    #[test]
    fn test_not_initialized_and_invalid_configuration() {
        let env = Env::default();
        let f = setup(&env);

        assert_eq!(
            f.adapter.try_get_config(),
            Err(Ok(AdapterError::NotInitialized))
        );
        assert_eq!(
            f.adapter.try_quote(&f.token_a, &f.token_b),
            Err(Ok(AdapterError::NotInitialized))
        );

        // window_seconds == 0
        assert_eq!(
            f.adapter.try_initialize(
                &f.admin,
                &f.factory_id,
                &f.consumer_id,
                &AdapterMode::Amm,
                &0,
                &600,
                &vec![&env],
            ),
            Err(Ok(AdapterError::InvalidConfiguration))
        );

        // max_age_seconds == 0
        assert_eq!(
            f.adapter.try_initialize(
                &f.admin,
                &f.factory_id,
                &f.consumer_id,
                &AdapterMode::Amm,
                &300,
                &0,
                &vec![&env],
            ),
            Err(Ok(AdapterError::InvalidConfiguration))
        );

        // CL mode with empty fee_tiers
        assert_eq!(
            f.adapter.try_initialize(
                &f.admin,
                &f.factory_id,
                &f.consumer_id,
                &AdapterMode::ConcentratedLiquidity,
                &300,
                &600,
                &vec![&env],
            ),
            Err(Ok(AdapterError::InvalidConfiguration))
        );
    }

    #[test]
    fn test_amm_quote_direct_order() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);
        let f = setup(&env);

        f.adapter.initialize(
            &f.admin,
            &f.factory_id,
            &f.consumer_id,
            &AdapterMode::Amm,
            &300,
            &600,
            &vec![&env],
        );

        let pool = Address::generate(&env);
        f.factory.set_pool(&f.token_a, &f.token_b, &pool);
        f.factory.set_pool_tokens(&pool, &f.token_a, &f.token_b);
        f.consumer.set_latest_timestamp(&pool, &950);
        f.consumer.set_amm_twap(
            &pool,
            &TwapResult {
                twap: 2_500_000,
                window_used: 300,
                snapshot_timestamp: 950,
            },
        );

        let (price, ts) = f.adapter.quote(&f.token_a, &f.token_b);
        assert_eq!(price, 2_500_000);
        assert_eq!(ts, 950);
    }

    #[test]
    fn test_amm_quote_inverted_token_order() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);
        let f = setup(&env);

        f.adapter.initialize(
            &f.admin,
            &f.factory_id,
            &f.consumer_id,
            &AdapterMode::Amm,
            &300,
            &600,
            &vec![&env],
        );

        let pool = Address::generate(&env);
        f.factory.set_pool(&f.token_a, &f.token_b, &pool);
        // Pool stores (token_b, token_a), so pool_tokens.0 != token_a.
        f.factory.set_pool_tokens(&pool, &f.token_b, &f.token_a);
        f.consumer.set_latest_timestamp(&pool, &950);
        f.consumer.set_amm_twap(
            &pool,
            &TwapResult {
                twap: 2_000_000, // 2.0
                window_used: 300,
                snapshot_timestamp: 950,
            },
        );

        let (price, ts) = f.adapter.quote(&f.token_a, &f.token_b);
        // Inverted: 1e12 / 2_000_000 = 500_000 (0.5)
        assert_eq!(price, 500_000);
        assert_eq!(ts, 950);
    }

    #[test]
    fn test_cl_quote_selects_highest_active_liquidity_tier() {
        let env = Env::default();
        env.ledger().set_timestamp(2_000);
        let f = setup(&env);

        f.adapter.initialize(
            &f.admin,
            &f.factory_id,
            &f.consumer_id,
            &AdapterMode::ConcentratedLiquidity,
            &300,
            &600,
            &vec![&env, 5, 30, 100],
        );

        // Tier 5: not deployed (factory returns None)
        // Tier 30: deployed with higher liquidity (5_000)
        let pool_30 = env.register_contract(None, MockClPool);
        MockClPoolClient::new(&env, &pool_30).set_liquidity(&5_000);
        f.factory.set_cl_pool(&f.token_a, &f.token_b, &30, &pool_30);
        f.factory.set_pool_tokens(&pool_30, &f.token_a, &f.token_b);
        f.consumer.set_latest_timestamp(&pool_30, &1_900);
        f.consumer.set_cl_twap(
            &pool_30,
            &ClTwapResult {
                average_tick: 100,
                window_used: 300,
                snapshot_timestamp: 1_900,
            },
        );

        // Tier 100: deployed with lower liquidity (1_000)
        let pool_100 = env.register_contract(None, MockClPool);
        MockClPoolClient::new(&env, &pool_100).set_liquidity(&1_000);
        f.factory
            .set_cl_pool(&f.token_a, &f.token_b, &100, &pool_100);
        f.factory.set_pool_tokens(&pool_100, &f.token_a, &f.token_b);
        f.consumer.set_latest_timestamp(&pool_100, &1_950);
        f.consumer.set_cl_twap(
            &pool_100,
            &ClTwapResult {
                average_tick: -100,
                window_used: 300,
                snapshot_timestamp: 1_950,
            },
        );

        let (price, ts) = f.adapter.quote(&f.token_a, &f.token_b);
        assert_eq!(price, OracleTwapAdapter::tick_to_price(100).unwrap());
        assert_eq!(ts, 1_900);
    }

    #[test]
    fn test_cl_quote_inverted_token_order() {
        let env = Env::default();
        env.ledger().set_timestamp(2_000);
        let f = setup(&env);

        f.adapter.initialize(
            &f.admin,
            &f.factory_id,
            &f.consumer_id,
            &AdapterMode::ConcentratedLiquidity,
            &300,
            &600,
            &vec![&env, 30],
        );

        let pool = env.register_contract(None, MockClPool);
        MockClPoolClient::new(&env, &pool).set_liquidity(&10_000);
        f.factory.set_cl_pool(&f.token_a, &f.token_b, &30, &pool);
        // Reversed pool token order
        f.factory.set_pool_tokens(&pool, &f.token_b, &f.token_a);
        f.consumer.set_latest_timestamp(&pool, &1_900);
        f.consumer.set_cl_twap(
            &pool,
            &ClTwapResult {
                average_tick: 100,
                window_used: 300,
                snapshot_timestamp: 1_900,
            },
        );

        let direct_price = OracleTwapAdapter::tick_to_price(100).unwrap();
        let expected_inverted = (PRICE_SCALE * PRICE_SCALE) / direct_price;

        let (price, ts) = f.adapter.quote(&f.token_a, &f.token_b);
        assert_eq!(price, expected_inverted);
        assert_eq!(ts, 1_900);
    }

    #[test]
    fn test_staleness_boundary() {
        let env = Env::default();
        let f = setup(&env);

        let max_age: u64 = 600;
        f.adapter.initialize(
            &f.admin,
            &f.factory_id,
            &f.consumer_id,
            &AdapterMode::Amm,
            &300,
            &max_age,
            &vec![&env],
        );

        let pool = Address::generate(&env);
        f.factory.set_pool(&f.token_a, &f.token_b, &pool);
        f.factory.set_pool_tokens(&pool, &f.token_a, &f.token_b);
        f.consumer.set_latest_timestamp(&pool, &1_000);
        f.consumer.set_amm_twap(
            &pool,
            &TwapResult {
                twap: 1_500_000,
                window_used: 300,
                snapshot_timestamp: 1_000,
            },
        );

        // Exactly at max_age (now - newest == max_age): still fresh
        env.ledger().set_timestamp(1_000 + max_age);
        let (price, ts) = f.adapter.quote(&f.token_a, &f.token_b);
        assert_eq!(price, 1_500_000);
        assert_eq!(ts, 1_000);

        // One second past max_age (now - newest == max_age + 1): stale
        env.ledger().set_timestamp(1_000 + max_age + 1);
        assert_eq!(
            f.adapter.try_quote(&f.token_a, &f.token_b),
            Err(Ok(AdapterError::StaleSnapshot))
        );
    }

    #[test]
    fn test_error_pool_not_found() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);
        let f = setup(&env);

        // AMM mode: factory has no pool
        f.adapter.initialize(
            &f.admin,
            &f.factory_id,
            &f.consumer_id,
            &AdapterMode::Amm,
            &300,
            &600,
            &vec![&env],
        );
        assert_eq!(
            f.adapter.try_quote(&f.token_a, &f.token_b),
            Err(Ok(AdapterError::PoolNotFound))
        );

        // Pool exists, snapshot & price valid, but get_pool_tokens returns None
        let pool = Address::generate(&env);
        f.factory.set_pool(&f.token_a, &f.token_b, &pool);
        f.consumer.set_latest_timestamp(&pool, &950);
        f.consumer.set_amm_twap(
            &pool,
            &TwapResult {
                twap: 1_000_000,
                window_used: 300,
                snapshot_timestamp: 950,
            },
        );
        assert_eq!(
            f.adapter.try_quote(&f.token_a, &f.token_b),
            Err(Ok(AdapterError::PoolNotFound))
        );

        // CL mode: no pool for any configured tier
        let f_cl = setup(&env);
        f_cl.adapter.initialize(
            &f_cl.admin,
            &f_cl.factory_id,
            &f_cl.consumer_id,
            &AdapterMode::ConcentratedLiquidity,
            &300,
            &600,
            &vec![&env, 5, 30],
        );
        assert_eq!(
            f_cl.adapter.try_quote(&f_cl.token_a, &f_cl.token_b),
            Err(Ok(AdapterError::PoolNotFound))
        );
    }

    #[test]
    fn test_error_no_snapshot() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);
        let f = setup(&env);

        f.adapter.initialize(
            &f.admin,
            &f.factory_id,
            &f.consumer_id,
            &AdapterMode::Amm,
            &300,
            &600,
            &vec![&env],
        );

        let pool = Address::generate(&env);
        f.factory.set_pool(&f.token_a, &f.token_b, &pool);
        f.factory.set_pool_tokens(&pool, &f.token_a, &f.token_b);
        // Consumer returns 0 when no snapshot is recorded.

        assert_eq!(
            f.adapter.try_quote(&f.token_a, &f.token_b),
            Err(Ok(AdapterError::NoSnapshot))
        );
    }

    #[test]
    fn test_error_invalid_price() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);
        let f = setup(&env);

        f.adapter.initialize(
            &f.admin,
            &f.factory_id,
            &f.consumer_id,
            &AdapterMode::Amm,
            &300,
            &600,
            &vec![&env],
        );

        let pool = Address::generate(&env);
        f.factory.set_pool(&f.token_a, &f.token_b, &pool);
        f.factory.set_pool_tokens(&pool, &f.token_a, &f.token_b);
        f.consumer.set_latest_timestamp(&pool, &950);

        // Zero price
        f.consumer.set_amm_twap(
            &pool,
            &TwapResult {
                twap: 0,
                window_used: 300,
                snapshot_timestamp: 950,
            },
        );
        assert_eq!(
            f.adapter.try_quote(&f.token_a, &f.token_b),
            Err(Ok(AdapterError::InvalidPrice))
        );

        // Negative price
        f.consumer.set_amm_twap(
            &pool,
            &TwapResult {
                twap: -100,
                window_used: 300,
                snapshot_timestamp: 950,
            },
        );
        assert_eq!(
            f.adapter.try_quote(&f.token_a, &f.token_b),
            Err(Ok(AdapterError::InvalidPrice))
        );
    }

    #[test]
    fn test_tick_to_price_known_values() {
        // tick = 0 -> 1.000000
        assert_eq!(OracleTwapAdapter::tick_to_price(0), Ok(1_000_000));
        // tick = +1 -> 1.000100
        assert_eq!(OracleTwapAdapter::tick_to_price(1), Ok(1_000_100));
        // tick = -1 -> 1 / 1.0001 ˜ 0.999900
        assert_eq!(OracleTwapAdapter::tick_to_price(-1), Ok(999_900));
        // tick = +100 -> 1.0001^100 ˜ 1.01004966
        assert_eq!(OracleTwapAdapter::tick_to_price(100), Ok(1_010_049));
        // tick = -100 -> 1.0001^-100 ˜ 0.99005033
        assert_eq!(OracleTwapAdapter::tick_to_price(-100), Ok(990_050));
        // Large negative tick: 1.0001^-10000 ˜ 0.36789783
        assert_eq!(OracleTwapAdapter::tick_to_price(-10_000), Ok(367_897));
    }

    #[test]
    fn test_tick_to_price_parity_with_exact_math() {
        // `concentrated_liquidity::math::tick_to_sqrt_price_x96` computes
        // sqrt(1.0001^tick) * 2^96 to within 1 ULP, so its implied price is
        // 1.0001^tick. Here we verify `tick_to_price` against f64 `1.0001^tick`
        // across positive and negative ticks with an explicit tolerance:
        // integer truncation in POW_SCALE (1e12) binary exponentiation plus
        // final scaling to PRICE_SCALE (1e6) stays within 1e-5 relative error
        // (<= 1 unit of 1e6 for |tick| <= 10_000, <= 10 units for |tick| <= 50_000).
        let cases: &[(i64, i128)] = &[
            (-50_000, 10),
            (-10_000, 1),
            (-1_000, 1),
            (-100, 1),
            (-1, 1),
            (0, 0),
            (1, 1),
            (100, 1),
            (1_000, 1),
            (10_000, 1),
            (50_000, 10),
        ];

        for &(tick, max_abs_diff) in cases {
            let actual = OracleTwapAdapter::tick_to_price(tick).unwrap();
            let expected = (1.0001_f64.powi(tick as i32) * (PRICE_SCALE as f64)).floor() as i128;
            let diff = (actual - expected).abs();
            assert!(
                diff <= max_abs_diff,
                "tick={tick}: actual={actual}, expected={expected}, diff={diff} > {max_abs_diff}"
            );
        }
    }

    #[test]
    fn test_tick_to_price_overflow() {
        // Very large tick overflows i128 during binary exponentiation in POW_SCALE.
        assert_eq!(
            OracleTwapAdapter::tick_to_price(500_000),
            Err(AdapterError::ArithmeticOverflow)
        );
    }
}
