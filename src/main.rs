use {
    clap::{Parser, Subcommand},
    csv::{Reader, Writer},
    futures_util::TryFutureExt,
    solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig},
    solana_sdk::{
        commitment_config::{CommitmentConfig, CommitmentLevel},
        compute_budget::ComputeBudgetInstruction,
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

const CU_LIMIT: u32 = 1000000;
const REMAINING_CSV_FILE: &str = "remaining_recipients.csv";
const MAX_RETRIES: usize = 5;
const MAX_TRANSFERS_PER_TX: usize = 4;

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
        long,
        value_name = "AMOUNT",
        help = "The amount of the token to airdrop to each recipient",
        global = false
    )]
    pub amount: Option<u64>,

    #[arg(
        long,
        value_name = "MICROLAMPORTS",
        help = "Number of microlamports to pay as priority fee per transaction",
        default_value = "0",
        global = true
    )]
    priority_fee: Option<u64>,
}

fn extract_column_from_csv(
    file_path: &str,
    column_index: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut rdr = Reader::from_path(file_path)?;
    let mut column_values: Vec<String> = Vec::new();

    for result in rdr.records() {
        let record = result?;
        if let Some(value) = record.get(column_index) {
            column_values.push(value.to_string());
        }
    }

    Ok(column_values)
}

fn write_remaining_csv(
    recipients: Vec<(String, u64)>,
    file_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut wtr = Writer::from_path(file_path)?;
    wtr.write_record(&["pubkey", "amount"])?;
    for (pubkey, amount) in recipients {
        wtr.write_record(&[pubkey, amount.to_string()])?;
    }
    wtr.flush()?;
    Ok(())
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

async fn process_airdrop(
    args: &AirdropArgs,
    rpc_client: Arc<RpcClient>,
    source_keypair: Arc<dyn Signer>,
) -> Result<(), Box<dyn Error>> {
    let recipient_pubkeys: Vec<Pubkey> = extract_column_from_csv(&args.recipients_csv_path, 0)?
        .iter()
        .map(|s| Pubkey::from_str(s).unwrap())
        .collect();

    let recipient_amounts = if let Some(amount) = args.amount {
        vec![amount; recipient_pubkeys.len()]
    } else {
        extract_column_from_csv(&args.recipients_csv_path, 1)?
            .iter()
            .map(|s| s.parse::<u64>().unwrap())
            .collect()
    };

    let total_tokens: u64 = recipient_amounts.iter().sum();

    let source_pubkey = &source_keypair.pubkey();
    let token_pubkey = Pubkey::from_str(&args.token_address).unwrap();

    println!("Airdropping {} tokens", total_tokens);
    println!("  Sender: {:?}", source_keypair.pubkey());
    println!("  Token: {:?}", token_pubkey);
    println!("  Recipients file: {}", &args.recipients_csv_path);
    println!("");

    let program_client: Arc<dyn ProgramClient<ProgramRpcClientSendTransaction>> = Arc::new(
        ProgramRpcClient::new(rpc_client.clone(), ProgramRpcClientSendTransaction),
    );

    let sender = get_associated_token_address_with_program_id(
        &source_pubkey,
        &token_pubkey,
        &spl_token_2022::id(),
    );

    let cu_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(CU_LIMIT);
    let cu_price_ix =
        ComputeBudgetInstruction::set_compute_unit_price(args.priority_fee.unwrap_or_default());

    let mut instructions: Vec<Instruction> = Vec::new();
    let mut transaction_count = 0;
    let mut transfer_count = 0;
    let mut remaining_recipients = vec![];

    // Initialize remaining recipients file with headers
    write_remaining_csv(vec![], REMAINING_CSV_FILE)?;

    for (_i, (recipient, &amount)) in recipient_pubkeys
        .iter()
        .zip(recipient_amounts.iter())
        .enumerate()
    {
        let mut recipient_instructions: Vec<Instruction> = Vec::new();

        let destination = get_associated_token_address_with_program_id(
            recipient,
            &token_pubkey,
            &spl_token_2022::id(),
        );

        let token_amount = spl_token_2022::ui_amount_to_amount(amount as f64, 9);

        if let Ok(Some(_ata)) = program_client.get_account(destination).await {
        } else {
            recipient_instructions.push(create_associated_token_account_idempotent(
                &source_pubkey,
                recipient,
                &token_pubkey,
                &spl_token_2022::id(),
            ));
        }

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
            token_amount,
            9,
            fetch_account_data_fn,
        )
        .await
        .unwrap();

        recipient_instructions.push(instruction);

        // if instructions.len() + recipient_instructions.len() + 1 >
        // MAX_INSTRUCTIONS_PER_TX {
        if transfer_count >= MAX_TRANSFERS_PER_TX {
            transfer_count = 0;
            transaction_count += 1;
            println!(
                "Packing transaction {}/{} 📦",
                transaction_count,
                (recipient_pubkeys.len() + MAX_TRANSFERS_PER_TX - 1) / MAX_TRANSFERS_PER_TX
            );

            let mut tx_instructions = vec![cu_price_ix.clone(), cu_limit_ix.clone()];
            tx_instructions.append(&mut instructions);

            let blockhash = program_client.get_latest_blockhash().await.unwrap();
            let message = Message::new_with_blockhash(
                &tx_instructions,
                Some(&source_keypair.pubkey()),
                &blockhash,
            );
            let mut transaction = Transaction::new_unsigned(message);

            let signers: Vec<&dyn Signer> = vec![source_keypair.as_ref()];
            transaction.sign(&signers, blockhash);

            if let Err(e) = send_transaction_with_retries(
                &mut transaction,
                rpc_client.clone(),
                &program_client,
                &source_keypair,
            )
            .await
            {
                println!(
                    "Failed to send transaction {}/{} ❌",
                    transaction_count,
                    (recipient_pubkeys.len() + MAX_TRANSFERS_PER_TX - 1) / MAX_TRANSFERS_PER_TX
                );
                println!("Writing remaining recipients to CSV 📝");
                remaining_recipients.extend(
                    recipient_pubkeys
                        .iter()
                        .skip((transaction_count - 1) * MAX_TRANSFERS_PER_TX)
                        .zip(
                            recipient_amounts
                                .iter()
                                .skip((transaction_count - 1) * MAX_TRANSFERS_PER_TX),
                        )
                        .map(|(pk, &amt)| (pk.to_string(), amt)),
                );
                write_remaining_csv(remaining_recipients, REMAINING_CSV_FILE)?;
                return Err(e);
            }
            instructions.clear();
        }

        instructions.extend(recipient_instructions);
        transfer_count += 1;
    }

    if !instructions.is_empty() {
        transaction_count += 1;
        println!(
            "Packing transaction {}/{} 📦",
            transaction_count,
            (recipient_pubkeys.len() + MAX_TRANSFERS_PER_TX - 1) / MAX_TRANSFERS_PER_TX
        );

        let mut tx_instructions = vec![cu_price_ix.clone(), cu_limit_ix.clone()];
        tx_instructions.append(&mut instructions);

        let blockhash = program_client.get_latest_blockhash().await.unwrap();
        let message = Message::new_with_blockhash(
            &tx_instructions,
            Some(&source_keypair.pubkey()),
            &blockhash,
        );
        let mut transaction = Transaction::new_unsigned(message);

        let signers: Vec<&dyn Signer> = vec![source_keypair.as_ref()];
        transaction.sign(&signers, blockhash);

        if let Err(e) = send_transaction_with_retries(
            &mut transaction,
            rpc_client.clone(),
            &program_client,
            &source_keypair,
        )
        .await
        {
            println!(
                "Failed to send transaction {}/{} ❌",
                transaction_count,
                (recipient_pubkeys.len() + MAX_TRANSFERS_PER_TX - 1) / MAX_TRANSFERS_PER_TX
            );
            println!("Writing remaining recipients to CSV 📝");
            remaining_recipients.extend(
                recipient_pubkeys
                    .iter()
                    .skip((transaction_count - 1) * MAX_TRANSFERS_PER_TX)
                    .zip(
                        recipient_amounts
                            .iter()
                            .skip((transaction_count - 1) * MAX_TRANSFERS_PER_TX),
                    )
                    .map(|(pk, &amt)| (pk.to_string(), amt)),
            );
            write_remaining_csv(remaining_recipients, REMAINING_CSV_FILE)?;
            return Err(e);
        }
    }

    // Write the final remaining recipients to the CSV file
    if remaining_recipients.is_empty() {
        println!("Airdrop successful 🎊");
        write_remaining_csv(vec![], REMAINING_CSV_FILE)?;
    }

    Ok(())
}

async fn send_transaction_with_retries(
    transaction: &mut Transaction,
    rpc_client: Arc<RpcClient>,
    program_client: &Arc<dyn ProgramClient<ProgramRpcClientSendTransaction>>,
    source_keypair: &Arc<dyn Signer>,
) -> Result<(), Box<dyn Error>> {
    for attempt in 0..MAX_RETRIES {
        println!(
            "Sending transaction attempt {}/{} 🚀",
            attempt + 1,
            MAX_RETRIES
        );
        match send_transaction(transaction.clone(), rpc_client.clone()).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                if attempt + 1 == MAX_RETRIES {
                    return Err(e);
                }
                if e.to_string().contains("Blockhash not found") {
                    println!("Refreshing blockhash and retrying...");
                    let blockhash = program_client.get_latest_blockhash().await.unwrap();
                    transaction.message.recent_blockhash = blockhash;
                    transaction.sign(&[source_keypair.as_ref()], blockhash);
                }
            }
        }
    }
    Ok(())
}

async fn send_transaction(
    transaction: Transaction,
    rpc_client: Arc<RpcClient>,
) -> Result<(), Box<dyn Error>> {
    // println!("Sending transaction 🚀");

    let config = RpcSendTransactionConfig {
        skip_preflight: false,
        preflight_commitment: Some(CommitmentLevel::Processed),
        ..Default::default()
    };

    let signature = rpc_client
        .send_and_confirm_transaction_with_spinner_and_config(
            &transaction,
            CommitmentConfig::finalized(),
            config,
        )
        .await
        .map_err(|e| Box::new(e) as Box<dyn Error>)?;

    println!("Transaction sent successfully ✅");
    println!("Signature: {}", signature);

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
            process_airdrop(&args, rpc_client.clone(), source_keypair).await?;
        }
    }

    Ok(())
}
