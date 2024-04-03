mod bindings;

use crate::bindings::olympus_heart::OlympusHeart;
use console::style;
use dotenv::dotenv;
use ethers::{
    abi::Address,
    core::k256::ecdsa::SigningKey,
    middleware::SignerMiddleware,
    providers::{Http, Middleware, Provider, ProviderExt, StreamExt, Ws},
    signers::{LocalWallet,Wallet},
    types::{transaction::eip2718::TypedTransaction, U256},
};
use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use reqwest::Client;
use std::fmt::Write;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::SystemTime;

static CLOCK_EMOJI: &str = "⏰";
static OHMEGA_EMOJI: &str = "Ω";
static SAD_EMOJI: &str = "😢";

fn greet() {
    println!(
        r#"
     ███████    █████       █████ █████ ██████   ██████ ███████████  █████  █████  █████████
   ███░░░░░███ ░░███       ░░███ ░░███ ░░██████ ██████ ░░███░░░░░███░░███  ░░███  ███░░░░░███
  ███     ░░███ ░███        ░░███ ███   ░███░█████░███  ░███    ░███ ░███   ░███ ░███    ░░░
 ░███      ░███ ░███         ░░█████    ░███░░███ ░███  ░██████████  ░███   ░███ ░░█████████
 ░███      ░███ ░███          ░░███     ░███ ░░░  ░███  ░███░░░░░░   ░███   ░███  ░░░░░░░░███
 ░░███     ███  ░███      █    ░███     ░███      ░███  ░███         ░███   ░███  ███    ░███
  ░░░███████░   ███████████    █████    █████     █████ █████        ░░████████  ░░█████████
    ░░░░░░░    ░░░░░░░░░░░    ░░░░░    ░░░░░     ░░░░░ ░░░░░          ░░░░░░░░    ░░░░░░░░░
                                      Heart Beat Bot

             "#
    );
}

enum AuctionState {
    Profitable(TypedTransaction),
    Ended,
}

struct Bot<K> {
    provider: Provider<Ws>,
    web_client: Client,
    contract: OlympusHeart<Provider<Ws>>,
    signer: SignerMiddleware<K, Wallet<SigningKey>>,
}

impl<K: Middleware> Bot<K> {
    pub fn new(
        provider: Provider<Ws>,
        web_client: Client,
        contract: OlympusHeart<Provider<Ws>>,
        signer: SignerMiddleware<K, Wallet<SigningKey>>,
    ) -> Self {
        Bot {
            provider,
            web_client,
            contract,
            signer,
        }
    }

    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            self.wait_for_next_auction().await;
            if let AuctionState::Profitable(tx) = self.wait_for_profitable().await {
                self.bid(tx).await;
            }
        }
    }

    async fn wait_for_profitable(&self) -> AuctionState {
        println!(
            "{} {}Waiting for rewards to be high enough...",
            style("[2/3]").bold().dim(),
            OHMEGA_EMOJI
        );

        let eth_price = U256::from(
            ((match get_token_price(&self.web_client, "ethereum").await {
                Ok(price) => price,
                Err(e) => {
                    panic!("Failed to get ETH price: {}", e);
                }
            }) * (1e6 as f64)) as u64,
        );
        let ohm_price = U256::from(
            ((match get_token_price(&self.web_client, "olympus").await {
                Ok(price) => price,
                Err(e) => panic!("Failed to get OHM price: {}", e),
            }) * (1e6 as f64)) as u64,
        );

        let mut beat_tx = self.contract.beat().tx;

        beat_tx.set_from(self.provider.get_accounts().await.unwrap()[0]);
        let mut stream = self.provider.watch_blocks().await.unwrap().stream();
        let mut block_counter = 0;
        while let Some(_block) = stream.next().await {
            block_counter += 1;
            let reward_in_ohm = self.calculate_reward_for_next_block().await;
            if reward_in_ohm <= U256::zero() {
                println!(
                    "{} {}Someone bought the auction...",
                    style("[3/3]").bold().dim(),
                    SAD_EMOJI
                );
                return AuctionState::Ended;
            }

            let gas_estimate = self.provider.estimate_gas(&beat_tx, None).await.unwrap();
            let gas_price = self.provider.get_gas_price().await.unwrap();
            let gas_cost_dollar = (gas_estimate * gas_price * eth_price) / U256::from(1e6 as u64);
            let reward_in_dollar = (reward_in_ohm * ohm_price) / U256::from(1e6 as u64);
            println!("{} blocks into the auction", block_counter);
            println!("Current rewards in dollar {}", reward_in_dollar);
            println!("Current gas costs in dollar {}", gas_cost_dollar);
            println!("-----------------------------------------------------");

            if reward_in_dollar > gas_cost_dollar {
                beat_tx.set_gas_price(gas_price);
                return AuctionState::Profitable(beat_tx);
            }
        }
        return AuctionState::Ended;
    }

    async fn wait_for_next_auction(&self) {
        println!(
            "{} {}Waiting for next auction...",
            style("[1/3]").bold().dim(),
            CLOCK_EMOJI
        );
        let last_beat: u64 = self
            .contract
            .last_beat()
            .call()
            .await
            .expect("Failed to get last beat");
        let frequency: u64 = self
            .contract
            .frequency()
            .call()
            .await
            .expect("Failed to get frequency");

        let time_till_next_auction = calculate_time_till_next_auction(&last_beat, &frequency);
        if time_till_next_auction <= 0 {
            return;
        }
        let pb = ProgressBar::new(time_till_next_auction);
        pb.set_style(
            ProgressStyle::with_template("[{elapsed_precise}] [{wide_bar:.cyan/blue}] ({eta})")
                .unwrap()
                .with_key("eta", |state: &ProgressState, w: &mut dyn Write| {
                    let eta = state.eta();
                    let hours = eta.as_secs() / 3600;
                    let minutes = (eta.as_secs() % 3600) / 60;
                    let seconds = eta.as_secs() % 60;
                    write!(w, "{:02}:{:02}:{:02}", hours, minutes, seconds).unwrap()
                })
                .progress_chars("#>-"),
        );

        while pb.position() < time_till_next_auction {
            thread::sleep(Duration::from_millis(1000));
            pb.set_position(pb.position() + 1);
        }

        pb.finish_and_clear();
    }

    async fn calculate_reward_for_next_block(&self) -> U256 {
        let last_beat: u64 = self
            .contract
            .last_beat()
            .call()
            .await
            .expect("Failed to get last beat");
        let frequency: u64 = self
            .contract
            .frequency()
            .call()
            .await
            .expect("Failed to get frequency");
        let auction_duraction = self
            .contract
            .auction_duration()
            .call()
            .await
            .expect("Failed to get auction duration");
        let max_reward = self
            .contract
            .max_reward()
            .call()
            .await
            .expect("Failed to get max reward");

        let latest_block_number = self.provider.get_block_number().await.unwrap();

        let latest_block = self
            .provider
            .get_block(latest_block_number)
            .await
            .unwrap()
            .unwrap();

        let next_block_timestamp = latest_block.timestamp + 12;

        let reward = calculate_reward_for_next_block(
            frequency,
            last_beat,
            next_block_timestamp,
            max_reward,
            auction_duraction,
        );

        reward
    }

    async fn bid(&self, tx: TypedTransaction) {
        println!(
            "{} {}Auction is profitable, bidding...",
            style("[3/3]").bold().dim(),
            OHMEGA_EMOJI
        );

        let pending_tx = self
            .signer
            .send_transaction(tx, None)
            .await
            .expect("Failed to send transaction");

        let receipt = pending_tx.await.expect("Failed to get receipt");
        println!("Transaction mined: {:?}", receipt);
    }
}

fn get_sys_time_in_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_else(|_| panic!("SystemTime before UNIX EPOCH!"))
        .as_secs()
}

fn calculate_time_till_next_auction(last_beat: &u64, frequency: &u64) -> u64 {
    let now: u64 = get_sys_time_in_secs();
    let time_since_last_beat: u64 = now - last_beat;
    let sleep_time: u64 = frequency - time_since_last_beat;
    sleep_time
}

pub async fn get_token_price(web_client: &Client, token: &str) -> Result<f64, reqwest::Error> {
    let url = format!("https://coins.llama.fi/prices/current/coingecko:{}", token);
    let payload = web_client
        .get(&url)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    let price = payload["coins"][format!("coingecko:{}", token)]["price"]
        .as_f64()
        .unwrap();
    Ok(price)
}

fn calculate_reward_for_next_block(
    frequency: u64,
    last_beat: u64,
    target_timestamp: U256,
    max_reward: U256,
    auction_duration: u64,
) -> U256 {
    let next_beat = U256::from(last_beat + frequency);
    if target_timestamp <= next_beat {
        return U256::zero();
    }

    let duration: U256 = if auction_duration > frequency {
        frequency.into()
    } else {
        auction_duration.into()
    };

    if target_timestamp - next_beat >= duration {
        max_reward
    } else {
        (target_timestamp - next_beat) * max_reward / duration
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    greet();
    dotenv().ok();
    let provider_url = std::env::var("PROVIDER_URL").expect("PROVIDER_URL is not set in .env file");

    let provider = Provider::<Ws>::connect(&provider_url).await.expect("Failed to connect to provider");
    let web_client = Client::new();
    let heart_contract_address: Address = "0xD5a0Ae3Bf7309416e70cB14399bDd508fE82C658".parse().expect("Failed to parse contract address");
    let heart_contract = OlympusHeart::new(heart_contract_address, Arc::new(provider.clone()));

    let private_key = std::env::var("PRIVATE_KEY").expect("PRIVATE_KEY is not set in .env file");
    let wallet: LocalWallet = private_key.parse().expect("Failed to parse private key");
    let provider_url_signer =
        std::env::var("PROVIDER_URL_SIGNER").expect("PROVIDER_URL_SIGNER is not set in .env file");
    let provider_signer = Provider::<Http>::try_connect(provider_url_signer.as_str())
        .await
        .expect("Failed to connect to signer provider");

    let signer = SignerMiddleware::new(provider_signer, wallet);
    let bot = Bot::new(provider, web_client, heart_contract, signer);

    bot.run().await?;

    Ok(())
}
