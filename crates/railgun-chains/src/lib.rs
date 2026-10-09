//! Railgun V2 deployments known to the apps.
//!
//! The SDK ships mainnet and Sepolia; the three other deployments are defined here. Proxy and
//! wrapped-token addresses and subsquid endpoints follow `@railgun-community/shared-models`;
//! relay-adapt addresses and POI start blocks are from memory of the same file and only matter for
//! relayed spends, which the viewer never does. Verify them before spending through the relay
//! adapt on these chains.

use alloy::primitives::{ChainId, address};
use railgun::chain_config::ChainConfig;

/// The deployment of `chain_id`: the SDK's, else one defined here.
pub fn from_chain_id(chain_id: ChainId) -> Option<ChainConfig> {
    ChainConfig::from_chain_id(chain_id).or_else(|| extra().into_iter().find(|c| c.id == chain_id))
}

/// Every deployment known to this build, for UI selectors.
pub fn all() -> Vec<ChainConfig> {
    let mut chains = vec![ChainConfig::mainnet(), ChainConfig::sepolia()];
    chains.extend(extra());
    chains
}

fn extra() -> [ChainConfig; 3] {
    [bnb(), polygon(), arbitrum()]
}

const POI_NODE: &str = "https://ppoi.fdi.network/";
const POI_LIST_KEY: &str = "efc6ddb59c098a13fb2b618fdae94c1c3a807abc8fb1837c93620c9143ee9e88";

pub fn bnb() -> ChainConfig {
    ChainConfig::new(
        56,
        address!("0x590162bf4b50f6576a459b75309ee21d92178a10"),
        25,
        address!("0x741936fb83ddf324636d3048b3e6bc800b8d9e12"),
        address!("0xbb4cdb9cbd36b01bd1cbaebf2de08d9173bc095c"),
        17421663,
        33336700,
        "https://rail-squid.squids.live/squid-railgun-bsc-v2/v/v1/graphql",
        POI_NODE,
        &[POI_LIST_KEY],
        None,
        None,
    )
}

pub fn polygon() -> ChainConfig {
    ChainConfig::new(
        137,
        address!("0x19b620929f97b7b990801496c3b361ca5def8c71"),
        25,
        address!("0xc7ffa542736321a3dd69246d73987566a5486968"),
        address!("0x0d500b1d8e8ef31e21c99d1db9a6444d3adf1270"),
        28083766,
        49877700,
        "https://rail-squid.squids.live/squid-railgun-polygon-v2/v/v1/graphql",
        POI_NODE,
        &[POI_LIST_KEY],
        None,
        None,
    )
}

pub fn arbitrum() -> ChainConfig {
    ChainConfig::new(
        42161,
        address!("0xfa7093cdd9ee6932b4eb2c9e1cde7ce00b1fa4b9"),
        25,
        address!("0x5ad95c537b002770a39dea342c4bb2b68b1497aa"),
        address!("0x82af49447d8a07e3bd95bd0d56f35241523fbab1"),
        56109834,
        148574000,
        "https://rail-squid.squids.live/squid-railgun-arbitrum-v2/v/v1/graphql",
        POI_NODE,
        &[POI_LIST_KEY],
        None,
        None,
    )
}
