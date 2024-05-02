use {
    clap::{Parser, Subcommand},
    csv::Reader,
    futures_util::TryFutureExt,
    solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig},
    solana_sdk::{
        commitment_config::{CommitmentConfig, CommitmentLevel},
        instruction::Instruction,
        message::Message,
        pubkey::Pubkey,
        signature::read_keypair_file,
        signer::Signer,
        transaction::Transaction,
    },
    spl_associated_token_account::{
        get_associated_token_address_with_program_id,
        instruction::create_associated_token_account_idempotent,
    },
    spl_token_2022::offchain,
    spl_token_client::client::{ProgramClient, ProgramRpcClient, ProgramRpcClientSendTransaction},
    std::{error::Error, str::FromStr, sync::Arc},
};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(
        long,
        value_name = "NETWORK_URL",
        help = "Network address of your RPC provider",
        global = true
    )]
    rpc: Option<String>,

    #[clap(
        global = true,
        short = 'C',
        long = "config",
        id = "PATH",
        help = "Filepath to config file."
    )]
    pub config_file: Option<String>,

    #[arg(
        long,
        value_name = "KEYPAIR_FILEPATH",
        help = "Filepath to keypair to use",
        global = true
    )]
    keypair: Option<String>,

    #[arg(
        long,
        value_name = "MICROLAMPORTS",
        help = "Number of microlamports to pay as priority fee per transaction",
        default_value = "0",
        global = true
    )]
    priority_fee: u64,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    #[command(about = "Airdrop tokens to the provided list of addresses.")]
    Airdrop(AirdropArgs),
}

#[derive(Parser, Debug)]
struct AirdropArgs {
    #[arg(
        value_name = "TOKEN_ADDRESS",
        help = "The address of the token to airdrop"
    )]
    pub token_address: String,

    #[arg(
        value_name = "RECIPIENTS_CSV_PATH",
        help = "The address CSV of the airdrop recipients"
    )]
    pub recipients_csv_path: String,

    #[arg(
        value_name = "AMOUNT",
        help = "The amount of the token to airdrop per address"
    )]
    pub amount: u64,
}
fn extract_column_from_csv(
    file_path: &str,
    column_index: usize,
) -> Result<Vec<Pubkey>, Box<dyn std::error::Error>> {
    let mut rdr = Reader::from_path(file_path)?;
    let mut column_values: Vec<Pubkey> = Vec::new();

    for result in rdr.records() {
        let record = result?;
        if let Some(value) = record.get(column_index) {
            column_values.push(Pubkey::from_str(&value.to_string()).unwrap());
        }
    }

    Ok(column_values)
}

async fn load_config(args: &Args) -> Result<solana_cli_config::Config, Box<dyn Error>> {
    if let Some(config_file) = &args.config_file {
        Ok(solana_cli_config::Config::load(config_file)?)
    } else if let Some(config_file) = &*solana_cli_config::CONFIG_FILE {
        Ok(solana_cli_config::Config::load(config_file).unwrap_or_default())
    } else {
        Ok(solana_cli_config::Config::default())
    }
}

async fn create_airdrop_tx(
    args: AirdropArgs,
    rpc_client: Arc<RpcClient>,
    source_keypair: Arc<dyn Signer>,
) -> Result<Transaction, Box<dyn Error>> {
    let recipients_pubkeys = extract_column_from_csv(&args.recipients_csv_path, 0).unwrap();
    let source_pubkey = &source_keypair.pubkey();
    let token_pubkey = Pubkey::from_str(&args.token_address).unwrap();
    let token_amount = args.amount;

    let amount = spl_token_2022::ui_amount_to_amount(token_amount as f64, 9);

    println!("Source: {:?}", source_keypair.pubkey());
    println!("Token: {:?}", token_pubkey);
    println!("Recipients: {:?}", recipients_pubkeys);
    println!("Amount: {}", token_amount);

    let program_client: Arc<dyn ProgramClient<ProgramRpcClientSendTransaction>> = Arc::new(
        ProgramRpcClient::new(rpc_client, ProgramRpcClientSendTransaction),
    );

    let sender = get_associated_token_address_with_program_id(
        &source_pubkey,
        &token_pubkey,
        &spl_token_2022::id(),
    );
    println!("Sender ATA: {}", sender);

    let mut instructions: Vec<Instruction> = Vec::new();

    for recipient in recipients_pubkeys.iter() {
        let destination = get_associated_token_address_with_program_id(
            &recipient,
            &token_pubkey,
            &spl_token_2022::id(),
        );
        println!("Destination ATA: {}", destination);

        instructions.push(create_associated_token_account_idempotent(
            &source_pubkey,
            recipient,
            &token_pubkey,
            &spl_token_2022::id(),
        ));

        let fetch_account_data_fn = |address| {
            program_client
                .get_account(address)
                .map_ok(|opt| opt.map(|acc| acc.data))
        };

        let instruction = offchain::create_transfer_checked_instruction_with_extra_metas(
            &spl_token_2022::id(),
            &sender,
            &token_pubkey,
            &destination,
            &source_keypair.pubkey(),
            &[],
            amount,
            9,
            fetch_account_data_fn,
        )
        .await
        .unwrap();

        instructions.push(instruction)
    }

    let blockhash = program_client.get_latest_blockhash().await.unwrap();

    let message =
        Message::new_with_blockhash(&instructions, Some(&source_keypair.pubkey()), &blockhash);
    let mut transaction = Transaction::new_unsigned(message);

    // let signers = [&source_keypair];
    let signers: Vec<&dyn Signer> = vec![source_keypair.as_ref()];

    transaction.sign(&signers, blockhash);

    Ok(transaction)
}

async fn execute_airdrop(
    transaction: Transaction,
    rpc_client: Arc<RpcClient>,
) -> Result<(), Box<dyn Error>> {
    let config = RpcSendTransactionConfig {
        skip_preflight: true,
        preflight_commitment: Some(CommitmentLevel::Processed),
        ..Default::default()
    };

    rpc_client
        .send_and_confirm_transaction_with_spinner_and_config(
            &transaction,
            CommitmentConfig::processed(),
            config,
        )
        .await
        .unwrap();
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let cli_config = load_config(&args).await?;
    let source_keypair =
        Arc::new(read_keypair_file(args.keypair.unwrap_or(cli_config.keypair_path)).unwrap());
    let cluster = args.rpc.unwrap_or(cli_config.json_rpc_url);
    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        cluster,
        CommitmentConfig::confirmed(),
    ));

    match args.command {
        Commands::Airdrop(args) => {
            let tx = create_airdrop_tx(args, rpc_client.clone(), source_keypair)
                .await
                .unwrap();
            execute_airdrop(tx, rpc_client.clone()).await?;
        }
    }

    Ok(())
}
