use anyhow::{Context, Result};
use bdk::bitcoin::util::bip32::ExtendedPrivKey;
use bdk::bitcoin::{Amount, PublicKey};
use bdk::blockchain::{ElectrumBlockchain, NoopProgress};
use bdk::KeychainKind;
use cfd_protocol::{PartyParams, WalletExt};
use std::path::Path;

const SLED_TREE_NAME: &str = "wallet";

pub struct Wallet<B = ElectrumBlockchain, D = bdk::sled::Tree> {
    wallet: bdk::Wallet<B, D>,
}

impl Wallet {
    pub async fn new(
        electrum_rpc_url: &str,
        wallet_dir: &Path,
        ext_priv_key: ExtendedPrivKey,
    ) -> Result<Self> {
        let client = bdk::electrum_client::Client::new(electrum_rpc_url)
            .context("Failed to initialize Electrum RPC client")?;

        // TODO: Replace with sqlite once https://github.com/bitcoindevkit/bdk/pull/376 is merged.
        let db = bdk::sled::open(wallet_dir)?.open_tree(SLED_TREE_NAME)?;

        let wallet = bdk::Wallet::new(
            bdk::template::Bip84(ext_priv_key, KeychainKind::External),
            Some(bdk::template::Bip84(ext_priv_key, KeychainKind::Internal)),
            ext_priv_key.network,
            db,
            ElectrumBlockchain::from(client),
        )?;

        wallet
            .sync(NoopProgress, None)
            .context("Failed to sync the wallet")?; // TODO: Use LogProgress once we have logging.

        Ok(Self { wallet })
    }

    pub fn build_party_params(
        &self,
        amount: Amount,
        identity_pk: PublicKey,
    ) -> Result<PartyParams> {
        self.wallet.build_party_params(amount, identity_pk)
    }
}