// use anyhow::Context;
use async_recursion::async_recursion;
use derivative::Derivative;
use melnet::MelnetError;
use novasmt::{CompressedProof, Database, FullProof, InMemoryCas, Tree};
use once_cell::sync::Lazy;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
    time::Instant,
};

use themelio_structs::{
    AbbrBlock, Address, Block, BlockHeight, CoinDataHeight, CoinID, CoinValue, ConsensusProof,
    Header, NetID, PoolKey, PoolState, StakeDoc, Transaction, TxHash, STAKE_EPOCH,
};
use thiserror::Error;
use tmelcrypt::{HashVal, Hashable};

use crate::{cache::AsyncCache, InMemoryTrustStore, NodeRequest, StateSummary, Substate};

#[derive(Debug, Clone)]
pub struct TrustedHeight {
    pub height: BlockHeight,
    pub header_hash: HashVal,
}

#[derive(Error, Debug)]
pub enum ParseTrustedHeightError {
    #[error("expected a ':' character to split the height and header hash")]
    ParseSplitError,
    #[error("height is not an integer")]
    ParseIntError(#[from] std::num::ParseIntError),
    #[error("failed to parse header hash as a hash")]
    ParseHeaderHash(#[from] hex::FromHexError),
}

impl FromStr for TrustedHeight {
    type Err = ParseTrustedHeightError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (height_str, hash_str) = s
            .split_once(':')
            .ok_or(ParseTrustedHeightError::ParseSplitError)?;

        let height = BlockHeight::from_str(height_str)?;
        let header_hash = HashVal::from_str(hash_str)?;

        Ok(Self {
            height,
            header_hash,
        })
    }
}

/// Standard interface for persisting a trusted block.
pub trait TrustStore {
    /// Set a trusted block in persistent storage, overriding the current
    /// value only if this one has a higher block height.
    fn set(&self, netid: NetID, trusted: TrustedHeight);
    /// Get the latest trusted block from persistent storage if one exists.
    fn get(&self, netid: NetID) -> Option<TrustedHeight>;
}

/// A higher-level client that validates all information.
#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub struct ValClient<T = InMemoryTrustStore> {
    netid: NetID,
    raw: NodeClient,
    trust_store: T,
    #[derivative(Debug = "ignore")]
    cache: Arc<AsyncCache>,
}

impl ValClient<InMemoryTrustStore> {
    /// Creates a new ValClient, hardcoding the default, in-memory trust store.
    pub fn new(netid: NetID, remote: SocketAddr) -> Self {
        Self::new_with_truststore(netid, remote, InMemoryTrustStore::new())
    }
}

impl<T: TrustStore + Send + Sync> ValClient<T> {
    /// Creates a new ValClient.
    pub fn new_with_truststore(netid: NetID, remote: SocketAddr, trust_store: T) -> Self {
        let raw = NodeClient::new(netid, remote);
        Self {
            netid,
            raw,
            trust_store,
            cache: Arc::new(AsyncCache::new(10000)),
        }
    }

    /// Gets the netid.
    pub fn netid(&self) -> NetID {
        self.netid
    }

    /// Trust a height.
    pub fn trust(&self, trusted: TrustedHeight) {
        self.trust_store.set(self.netid, trusted);
    }

    /// Obtains the latest validated snapshot. Use this method first to get something to validate info against.
    #[deprecated]
    pub async fn insecure_latest_snapshot(&self) -> melnet::Result<ValClientSnapshot> {
        self.trust_latest().await?;
        self.snapshot().await
    }

    // trust latest height
    async fn trust_latest(&self) -> melnet::Result<()> {
        let summary = self.raw.get_summary().await?;
        self.trust(TrustedHeight {
            height: summary.height,
            header_hash: summary.header.hash(),
        });
        Ok(())
    }

    /// Obtains a validated snapshot based on what height was trusted.
    pub async fn snapshot(&self) -> melnet::Result<ValClientSnapshot> {
        static INCEPTION: Lazy<Instant> = Lazy::new(Instant::now);
        // cache key: current time, divided by 10 seconds
        let cache_key = INCEPTION.elapsed().as_secs() / 10;
        let (height, header, _) = self
            .cache
            .get_or_try_fill((cache_key, "summary"), async {
                let summary = self.raw.get_summary().await?;
                self.validate_height(summary.height, summary.header, summary.proof.clone())
                    .await?;
                Ok((summary.height, summary.header, summary.proof))
            })
            .await?;
        Ok(ValClientSnapshot {
            height,
            header,
            raw: self.raw.clone(),
            cache: self.cache.clone(),
        })
    }

    /// Convenience function to obtains a validated snapshot based on a given height.
    pub async fn older_snapshot(&self, height: BlockHeight) -> melnet::Result<ValClientSnapshot> {
        let snap = self.snapshot().await?;
        snap.get_older(height).await
    }

    /// Helper to validate a given block height and header.
    #[async_recursion]
    async fn validate_height(
        &self,
        height: BlockHeight,
        header: Header,
        proof: ConsensusProof,
    ) -> melnet::Result<()> {
        let safe_stakers = {
            loop {
                let (safe_height, safe_stakers) = self.get_trusted_stakers().await?;
                if height.epoch() > safe_height.epoch() + 1 {
                    log::error!(
                    "OUTDATED CHECKPOINT: trusted height {} in epoch {} but remote height {} in epoch {}. Continuing with best-effort to update checkpoint",
                    safe_height,
                    safe_height.epoch(),
                    height,
                    height.epoch()
                );
                    // Okay, we must recurse to one epoch ago now
                    let old_height = BlockHeight(height.0.saturating_sub(STAKE_EPOCH));
                    let (old_block, old_proof) = self.raw.get_abbr_block(old_height).await?;
                    self.validate_height(old_height, old_block.header, old_proof)
                        .await?;
                } else if height.epoch() > safe_height.epoch()
                    && safe_height.epoch() == (safe_height + BlockHeight(1)).epoch()
                {
                    // to cross the epoch, we must obtain the epoch-terminal snapshot first.
                    // this places the correct thing in the cache, which then lets this one verify too.
                    let epoch_ending_height =
                        BlockHeight((safe_height.epoch() + 1) * STAKE_EPOCH - 1);
                    let (old_block, old_proof) =
                        self.raw.get_abbr_block(epoch_ending_height).await?;
                    self.validate_height(epoch_ending_height, old_block.header, old_proof)
                        .await?;
                } else {
                    break safe_stakers;
                }
            }
        };
        // we use the stakers to validate the latest summary
        let mut good_votes = CoinValue(0);
        let mut total_votes = CoinValue(0);
        for (_, doc) in safe_stakers.iter() {
            let doc: StakeDoc =
                stdcode::deserialize(&doc).map_err(|e| MelnetError::Custom(format!("{:?}", e)))?;
            if height.epoch() >= doc.e_start && height.epoch() < doc.e_post_end {
                total_votes += doc.syms_staked;
                if let Some(sig) = proof.get(&doc.pubkey) {
                    if doc.pubkey.verify(&header.hash(), sig) {
                        good_votes += doc.syms_staked
                    }
                }
            }
        }

        if good_votes < total_votes * 2 / 3 {
            return Err(MelnetError::Custom(format!(
                "remote height {} has insufficient votes (total_votes = {}, good_votes = {})",
                height, total_votes, good_votes
            )));
        }
        // automatically update trust
        self.trust(TrustedHeight {
            height,
            header_hash: header.hash(),
        });
        Ok(())
    }

    /// Helper function to obtain the trusted staker set.
    async fn get_trusted_stakers(&self) -> melnet::Result<(BlockHeight, Tree<InMemoryCas>)> {
        let checkpoint = self.trust_store.get(self.netid).ok_or_else(|| {
            MelnetError::Custom(
                "Expected to find a trusted block when fetching trusted stakers".into(),
            )
        })?;

        let temp_forest = Database::new(InMemoryCas::default());
        let stakers = self.raw.get_stakers_raw(checkpoint.height).await?;
        // first obtain trusted SMT branch
        let (abbr_block, _) = self.raw.get_abbr_block(checkpoint.height).await?;
        if abbr_block.header.hash() != checkpoint.header_hash {
            return Err(MelnetError::Custom(format!(
                "remote block contradicted trusted block hash: trusted {}, yet got {}:{}:{}",
                checkpoint.header_hash,
                checkpoint.height,
                abbr_block.header.hash(),
                abbr_block.header.height
            )));
        }
        let trusted_stake_hash = abbr_block.header.stakes_hash;
        let mut mapping = temp_forest.get_tree(Default::default()).unwrap();
        for (k, v) in stakers {
            mapping.insert(k.0, &v);
        }
        if mapping.root_hash() != trusted_stake_hash.0 {
            return Err(MelnetError::Custom(
                "remote staker set contradicted valid header".into(),
            ));
        }
        Ok((checkpoint.height, mapping))
    }
}

/// A "snapshot" of the state at a given state. It essentially encapsulates a NodeClient and a trusted header.
#[derive(Clone)]
pub struct ValClientSnapshot {
    height: BlockHeight,
    header: Header,
    raw: NodeClient,
    cache: Arc<AsyncCache>,
}

impl ValClientSnapshot {
    /// Gets a reference to the raw, unvalidating raw client.
    pub fn get_raw(&self) -> &NodeClient {
        &self.raw
    }

    /// Gets an older snapshot.
    pub async fn get_older(&self, old_height: BlockHeight) -> melnet::Result<Self> {
        if old_height > self.height {
            return Err(MelnetError::Custom("cannot travel into the future".into()));
        }
        if old_height == self.height {
            return Ok(self.clone());
        }
        // Get an SMT branch
        let old_elem: Header = self
            .cache
            .get_or_try_fill(("header", old_height), async {
                let val = self
                    .get_smt_value(
                        Substate::History,
                        tmelcrypt::hash_single(&stdcode::serialize(&old_height).unwrap()),
                    )
                    .await?;
                let old_elem: Header = stdcode::deserialize(&val).map_err(|e| {
                    MelnetError::Custom(format!("could not deserialize old header: {}", e))
                })?;
                Ok::<_, melnet::MelnetError>(old_elem)
            })
            .await?;
        // this can never possibly be bad unless everything is horribly untrustworthy
        assert_eq!(old_elem.height, old_height);
        Ok(Self {
            height: old_height,
            header: old_elem,
            raw: self.raw.clone(),
            cache: self.cache.clone(),
        })
    }

    /// Gets the header.
    pub fn current_header(&self) -> Header {
        self.header
    }

    /// Helper function to obtain the proposer reward amount.
    pub async fn get_proposer_reward(&self) -> melnet::Result<CoinValue> {
        let reward_coin = self.get_coin(CoinID::proposer_reward(self.height)).await?;
        let reward_amount = reward_coin.map(|v| v.coin_data.value).unwrap_or_default();
        Ok(reward_amount)
    }

    /// Gets the whole block at this height.
    pub async fn current_block(&self) -> melnet::Result<Block> {
        self.cache
            .get_or_try_fill(("block", self.height), async {
                let header = self.current_header();
                let (block, _) = get_full_block(self.raw.clone(), self.height).await?;
                if block.header != header {
                    return Err(MelnetError::Custom("block header does not match".into()));
                }
                Ok(block)
            })
            .await
    }

    /// Gets the whole block at this height, with a function that gets cached transactions.
    pub async fn current_block_with_known(
        &self,
        get_known_tx: impl Fn(TxHash) -> Option<Transaction>,
    ) -> melnet::Result<Block> {
        let header = self.current_header();
        let (block, _) = self.raw.get_full_block(header.height, get_known_tx).await?;
        if block.header != header {
            return Err(MelnetError::Custom("block header does not match".into()));
        }
        Ok(block)
    }

    /// Gets a historical header.
    pub async fn get_history(&self, height: BlockHeight) -> melnet::Result<Option<Header>> {
        self.get_smt_value_serde(Substate::History, height).await
    }

    /// Gets a coin.
    pub async fn get_coin(&self, coinid: CoinID) -> melnet::Result<Option<CoinDataHeight>> {
        self.get_smt_value_serde(Substate::Coins, coinid).await
    }

    /// Gets a coin count.
    pub async fn get_coin_count(&self, covhash: Address) -> melnet::Result<Option<u64>> {
        let val = self
            .get_smt_value(Substate::Coins, covhash.0.hash_keyed(b"coin_count"))
            .await?;
        if val.is_empty() {
            Ok(None)
        } else {
            Ok(Some(stdcode::deserialize(&val).map_err(|e| {
                MelnetError::Custom(format!("bad coin count {:?} {:?}", val, e.to_string()))
            })?))
        }
    }

    /// Gets all the coins associated with the given address. Return None if the server simply does not index this information.
    pub async fn get_coins(
        &self,
        covhash: Address,
    ) -> melnet::Result<Option<BTreeMap<CoinID, CoinDataHeight>>> {
        self.cache
            .get_or_try_fill(("coins", self.height, covhash), async {
                let coins = self.raw.get_some_coins(self.height, covhash).await?;
                if let Some(coins) = coins {
                    let coins: HashSet<CoinID> = coins.into_iter().collect();
                    let count = self.get_coin_count(covhash).await?;
                    if let Some(count) = count {
                        if count != coins.len() as u64 {
                            return Err(MelnetError::Custom(format!(
                                "got incomplete list of {} coins rather than {}",
                                coins.len(),
                                count
                            )));
                        }
                    }
                    // fill in the coins
                    let mut coin_futs = BTreeMap::new();
                    for coin in coins {
                        let this = self.clone();
                        let fut_data = smolscale::spawn(async move {
                            let r = this
                                .get_coin(coin)
                                .await?
                                .expect("invalid data received while getting coin list");
                            Ok::<_, melnet::MelnetError>(r)
                        });
                        coin_futs.insert(coin, fut_data);
                    }
                    let mut toret = BTreeMap::new();
                    for (i, (k, v)) in coin_futs.into_iter().enumerate() {
                        let v = v.await?;
                        log::debug!("loading coin {}", i);
                        toret.insert(k, v);
                    }
                    Ok(Some(toret))
                } else {
                    Ok(None)
                }
            })
            .await
    }

    /// A helper function to gets the CoinDataHeight for a coin *spent* at this height. This requires special handling because if the coin was created and spent at the same height, then the coin would never appear in a confirmed coin mapping.
    pub async fn get_coin_spent_here(
        &self,
        coinid: CoinID,
    ) -> melnet::Result<Option<CoinDataHeight>> {
        // First we try the transactions mapping in this block.
        if let Some(tx) = self.get_transaction(coinid.txhash).await? {
            // Great. Now we can reconstruct the CDH from the transaction.
            return Ok(tx
                .outputs
                .get(coinid.index as usize)
                .map(|v| CoinDataHeight {
                    coin_data: v.clone(),
                    height: self.height,
                }));
        }
        // Okay, so that didn't really work. That means that if the CDH does exist, it's in the previous block's coin mapping.
        self.get_older(self.height.0.saturating_sub(1).into())
            .await?
            .get_coin(coinid)
            .await
    }

    /// Gets a pool info.
    pub async fn get_pool(&self, denom: PoolKey) -> melnet::Result<Option<PoolState>> {
        self.get_smt_value_serde(Substate::Pools, denom).await
    }

    /// Gets a stake info.
    pub async fn get_stake(&self, staking_txhash: HashVal) -> melnet::Result<Option<StakeDoc>> {
        self.get_smt_value_serde(Substate::Stakes, staking_txhash)
            .await
    }

    /// Gets a transaction.
    pub async fn get_transaction(&self, txhash: TxHash) -> melnet::Result<Option<Transaction>> {
        self.get_smt_value_serde(Substate::Transactions, txhash)
            .await
    }

    /// Helper for serde.
    async fn get_smt_value_serde<S: Serialize, D: DeserializeOwned + Serialize>(
        &self,
        substate: Substate,
        key: S,
    ) -> melnet::Result<Option<D>> {
        let val = self
            .get_smt_value(
                substate,
                tmelcrypt::hash_single(&stdcode::serialize(&key).unwrap()),
            )
            .await?;
        if val.is_empty() {
            return Ok(None);
        }
        let val = stdcode::deserialize(&val)
            .map_err(|_| MelnetError::Custom("fatal deserialization error".into()))?;
        Ok(Some(val))
    }

    /// Gets a local SMT branch, validated.
    pub async fn get_smt_value(&self, substate: Substate, key: HashVal) -> melnet::Result<Vec<u8>> {
        let verify_against = match substate {
            Substate::Coins => self.header.coins_hash,
            Substate::History => self.header.history_hash,
            Substate::Pools => self.header.pools_hash,
            Substate::Stakes => self.header.stakes_hash,
            Substate::Transactions => self.header.transactions_hash,
        };
        self.cache.get_or_try_fill((verify_against, key), async {
        let (val, branch) = self.raw.get_smt_branch(self.height, substate, key).await?;
            if !branch.verify(verify_against.0, key.0, &val) {
                return Err(MelnetError::Custom(format!(
                    "unable to verify merkle proof for height {:?}, substate {:?}, key {:?}, value {:?}, branch {:?}",
                    self.height, substate, key, val, branch
                )));
            }
            Ok(val)
        }).await
    }
}

/// A client to a particular node server.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct NodeClient {
    remote: SocketAddr,
    netname: String,
}

impl NodeClient {
    /// Creates as new NodeClient
    pub fn new(netid: NetID, remote: SocketAddr) -> Self {
        let netname = match netid {
            NetID::Mainnet => "mainnet-node".to_string(),
            NetID::Testnet => "testnet-node".to_string(),
            _ => format!("{:?}", netid),
        };
        Self { remote, netname }
    }

    /// Helper function to do a request.
    async fn request(&self, req: NodeRequest) -> melnet::Result<Vec<u8>> {
        // eprintln!("==> {:?}", req);
        // let start = Instant::now();
        let res: Vec<u8> = melnet::request(self.remote, &self.netname, "node", req).await?;
        Ok(res)
    }

    /// Sends a tx.
    pub async fn send_tx(&self, tx: Transaction) -> melnet::Result<()> {
        self.request(NodeRequest::SendTx(tx)).await?;
        Ok(())
    }

    /// Gets a summary of the state.
    pub async fn get_summary(&self) -> melnet::Result<StateSummary> {
        get_summary(self.clone()).await
    }

    /// Gets an "abbreviated block".
    pub async fn get_abbr_block(
        &self,
        height: BlockHeight,
    ) -> melnet::Result<(AbbrBlock, ConsensusProof)> {
        get_abbr_block(self.clone(), height).await
    }

    /// Gets a full block, given a function that tells known from unknown transactions.
    pub async fn get_full_block(
        &self,
        height: BlockHeight,
        get_known_tx: impl Fn(TxHash) -> Option<Transaction>,
    ) -> melnet::Result<(Block, ConsensusProof)> {
        let (abbr, cproof) = self.get_abbr_block(height).await?;
        let mut known = vec![];
        let mut unknown = vec![];
        for txhash in abbr.txhashes.iter() {
            if let Some(tx) = get_known_tx(*txhash) {
                known.push(tx);
            } else {
                unknown.push(*txhash);
            }
        }
        // send off a request
        let mut response: Block = if unknown.is_empty() {
            Block {
                header: abbr.header,
                transactions: HashSet::new(),
                proposer_action: abbr.proposer_action,
            }
        } else {
            let req = NodeRequest::GetPartialBlock(height, unknown);
            stdcode::deserialize(&self.request(req).await?)
                .map_err(|e| melnet::MelnetError::Custom(e.to_string()))?
        };
        for known in known {
            response.transactions.insert(known);
        }
        let new_abbr = response.abbreviate();
        if new_abbr.header != abbr.header || new_abbr.txhashes != abbr.txhashes {
            return Err(melnet::MelnetError::Custom(
                "mismatched abbreviation".into(),
            ));
        }
        Ok((response, cproof))
    }

    /// Gets an SMT branch.
    pub async fn get_smt_branch(
        &self,
        height: BlockHeight,
        elem: Substate,
        key: HashVal,
    ) -> melnet::Result<(Vec<u8>, FullProof)> {
        get_smt_branch(self.clone(), height, elem, key).await
    }

    /// Gets the stakers, **as the raw SMT mapping**
    pub async fn get_stakers_raw(
        &self,
        height: BlockHeight,
    ) -> melnet::Result<BTreeMap<HashVal, Vec<u8>>> {
        get_stakers_raw(self.clone(), height).await
    }

    /// Gets some coins.
    pub async fn get_some_coins(
        &self,
        height: BlockHeight,
        owner: Address,
    ) -> melnet::Result<Option<Vec<CoinID>>> {
        stdcode::deserialize(
            &self
                .request(NodeRequest::GetSomeCoins(height, owner))
                .await?,
        )
        .map_err(|e| melnet::MelnetError::Custom(e.to_string()))
    }
}

// #[cached::proc_macro::cached(result = true, size = 1000)]
async fn get_stakers_raw(
    this: NodeClient,
    height: BlockHeight,
) -> melnet::Result<BTreeMap<HashVal, Vec<u8>>> {
    stdcode::deserialize(&this.request(NodeRequest::GetStakersRaw(height)).await?)
        .map_err(|e| melnet::MelnetError::Custom(e.to_string()))
}

// #[cached::proc_macro::cached(result = true, size = 1000)]
async fn get_abbr_block(
    this: NodeClient,
    height: BlockHeight,
) -> melnet::Result<(AbbrBlock, ConsensusProof)> {
    stdcode::deserialize(&this.request(NodeRequest::GetAbbrBlock(height)).await?)
        .map_err(|e| melnet::MelnetError::Custom(e.to_string()))
}

// #[cached::proc_macro::cached(result = true, time = 5, size = 1)]
async fn get_summary(this: NodeClient) -> melnet::Result<StateSummary> {
    stdcode::deserialize(&this.request(NodeRequest::GetSummary).await?)
        .map_err(|e| melnet::MelnetError::Custom(e.to_string()))
}

// #[cached::proc_macro::cached(result = true, size = 10000)]
async fn get_smt_branch(
    this: NodeClient,
    height: BlockHeight,
    elem: Substate,
    keyhash: HashVal,
) -> melnet::Result<(Vec<u8>, FullProof)> {
    let tuple: (Vec<u8>, CompressedProof) = stdcode::deserialize(
        &this
            .request(NodeRequest::GetSmtBranch(height, elem, keyhash))
            .await?,
    )
    .map_err(|e| melnet::MelnetError::Custom(e.to_string()))?;
    let decompressed = tuple
        .1
        .decompress()
        .ok_or_else(|| melnet::MelnetError::Custom("could not decompress proof".into()))?;
    Ok((tuple.0, decompressed))
}

// #[cached::proc_macro::cached(result = true, size = 100)]
async fn get_full_block(
    this: NodeClient,
    height: BlockHeight,
) -> melnet::Result<(Block, ConsensusProof)> {
    this.get_full_block(height, |_| None).await
}
