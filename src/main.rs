// pub mod meta;

use {
    // crate::meta::parse_transfer_hook_account_arg,
    clap::{crate_description, crate_name, crate_version, Arg, Command},
    solana_clap_v3_utils::{
        input_parsers::{
            parse_url_or_moniker, pubkey_of_signer, signer::SignerSourceParserBuilder, Amount,
        },
        input_validators::{
            is_amount_or_all, is_valid_pubkey, is_valid_signer, normalize_to_url_if_moniker,
        },
        keypair::DefaultSigner,
    },

    solana_client::nonblocking::rpc_client::RpcClient,
    solana_remote_wallet::remote_wallet::RemoteWalletManager,
    solana_sdk::{
        commitment_config::CommitmentConfig,
        instruction::Instruction,
        pubkey::Pubkey,
        signature::{Signature, Signer},
        system_instruction, system_program,
        transaction::Transaction,
    },
    spl_tlv_account_resolution::{account::ExtraAccountMeta, state::ExtraAccountMetaList},
    spl_transfer_hook_interface::{
        get_extra_account_metas_address,
        instruction::{initialize_extra_account_meta_list, update_extra_account_meta_list},
    },
    std::{process::exit, rc::Rc},
};

// Helper function to calculate the required lamports for rent
async fn calculate_rent_lamports(
    rpc_client: &RpcClient,
    account_address: &Pubkey,
    account_size: usize,
) -> Result<u64, Box<dyn std::error::Error>> {
    let required_lamports = rpc_client
        .get_minimum_balance_for_rent_exemption(account_size)
        .await
        .map_err(|err| format!("error: unable to fetch rent-exemption: {err}"))?;
    let account_info = rpc_client.get_account(account_address).await;
    let current_lamports = account_info.map(|a| a.lamports).unwrap_or(0);
    Ok(required_lamports.saturating_sub(current_lamports))
}

async fn build_transaction_with_rent_transfer(
    rpc_client: &RpcClient,
    payer: &dyn Signer,
    extra_account_metas_address: &Pubkey,
    extra_account_metas: &Vec<ExtraAccountMeta>,
    instruction: Instruction,
) -> Result<Transaction, Box<dyn std::error::Error>> {
    let account_size = ExtraAccountMetaList::size_of(extra_account_metas.len())?;
    let transfer_lamports =
        calculate_rent_lamports(rpc_client, extra_account_metas_address, account_size).await?;

    let mut instructions = vec![];
    if transfer_lamports > 0 {
        instructions.push(system_instruction::transfer(
            &payer.pubkey(),
            extra_account_metas_address,
            transfer_lamports,
        ));
    }

    instructions.push(instruction);

    let transaction = Transaction::new_with_payer(&instructions, Some(&payer.pubkey()));

    Ok(transaction)
}

async fn sign_and_send_transaction(
    transaction: &mut Transaction,
    rpc_client: &RpcClient,
    payer: &dyn Signer,
    mint_authority: &dyn Signer,
) -> Result<Signature, Box<dyn std::error::Error>> {
    let mut signers = vec![payer];
    if payer.pubkey() != mint_authority.pubkey() {
        signers.push(mint_authority);
    }

    let blockhash = rpc_client
        .get_latest_blockhash()
        .await
        .map_err(|err| format!("error: unable to get latest blockhash: {err}"))?;

    transaction
        .try_sign(&signers, blockhash)
        .map_err(|err| format!("error: failed to sign transaction: {err}"))?;

    rpc_client
        .send_and_confirm_transaction_with_spinner(transaction)
        .await
        .map_err(|err| format!("error: send transaction: {err}").into())
}

struct Config {
    commitment_config: CommitmentConfig,
    default_signer: Box<dyn Signer>,
    json_rpc_url: String,
    verbose: bool,
}

async fn process_create_extra_account_metas(
    rpc_client: &RpcClient,
    program_id: &Pubkey,
    token: &Pubkey,
    extra_account_metas: Vec<ExtraAccountMeta>,
    mint_authority: &dyn Signer,
    payer: &dyn Signer,
) -> Result<Signature, Box<dyn std::error::Error>> {
    let extra_account_metas_address = get_extra_account_metas_address(token, program_id);

    // Check if the extra meta account has already been initialized
    let extra_account_metas_account = rpc_client.get_account(&extra_account_metas_address).await;
    if let Ok(account) = &extra_account_metas_account {
        if account.owner != system_program::id() {
            return Err(format!("error: extra account metas for mint {token} and program {program_id} already exists").into());
        }
    }

    let instruction = initialize_extra_account_meta_list(
        program_id,
        &extra_account_metas_address,
        token,
        &mint_authority.pubkey(),
        &extra_account_metas,
    );

    let mut transaction = build_transaction_with_rent_transfer(
        rpc_client,
        payer,
        &extra_account_metas_address,
        &extra_account_metas,
        instruction,
    )
    .await?;

    sign_and_send_transaction(&mut transaction, rpc_client, payer, mint_authority).await
}

async fn process_update_extra_account_metas(
    rpc_client: &RpcClient,
    program_id: &Pubkey,
    token: &Pubkey,
    extra_account_metas: Vec<ExtraAccountMeta>,
    mint_authority: &dyn Signer,
    payer: &dyn Signer,
) -> Result<Signature, Box<dyn std::error::Error>> {
    let extra_account_metas_address = get_extra_account_metas_address(token, program_id);

    // Check if the extra meta account has been initialized first
    let extra_account_metas_account = rpc_client.get_account(&extra_account_metas_address).await;
    if extra_account_metas_account.is_err() {
        return Err(format!(
            "error: extra account metas for mint {token} and program {program_id} does not exist"
        )
        .into());
    }

    let instruction = update_extra_account_meta_list(
        program_id,
        &extra_account_metas_address,
        token,
        &mint_authority.pubkey(),
        &extra_account_metas,
    );

    let mut transaction = build_transaction_with_rent_transfer(
        rpc_client,
        payer,
        &extra_account_metas_address,
        &extra_account_metas,
        instruction,
    )
    .await?;

    sign_and_send_transaction(&mut transaction, rpc_client, payer, mint_authority).await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app_matches = Command::new(crate_name!())
        .about(crate_description!())
        .version(crate_version!())
        .subcommand_required(true)
        .arg_required_else_help(true)
        .arg({
            let arg = Arg::new("config_file")
                .short('C')
                .long("config")
                .value_name("PATH")
                .takes_value(true)
                .global(true)
                .help("Configuration file to use");
            if let Some(ref config_file) = *solana_cli_config::CONFIG_FILE {
                arg.default_value(config_file)
            } else {
                arg
            }
        })
        .arg(
            Arg::new("fee_payer")
                .long("fee-payer")
                .value_name("KEYPAIR")
                .value_parser(SignerSourceParserBuilder::default().build())
                .takes_value(true)
                .global(true)
                .help(
                    "Filepath or URL to a keypair to pay transaction fee [default: client keypair]",
                ),
        )
        .arg(
            Arg::new("verbose")
                .long("verbose")
                .short('v')
                .takes_value(false)
                .global(true)
                .help("Show additional information"),
        )
        .arg(
            Arg::new("json_rpc_url")
                .short('u')
                .long("url")
                .value_name("URL")
                .takes_value(true)
                .global(true)
                .value_parser(parse_url_or_moniker)
                .help("JSON RPC URL for the cluster [default: value from configuration file]"),
        )
        .subcommand(
            Command::new("airdrop")
                .about("Airdrop a given number of tokens to the provided list of addresses")
                .arg(
                    Arg::with_name("token")
                        .validator(|p| is_valid_pubkey(p))
                        .value_name("TOKEN_MINT_ADDRESS")
                        .takes_value(true)
                        .index(1)
                        .required(true)
                        .help("Token to airdrop"),
                )
                .arg(
                    Arg::with_name("amount")
                        .validator(|a| is_amount_or_all(a))
                        .value_name("TOKEN_AMOUNT")
                        .takes_value(true)
                        .index(2)
                        .required(true)
                        .help("Amount to send, in tokens; accepts keyword ALL"),
                )
                .arg(
                    Arg::with_name("recipient_accounts")
                        .validator(|p| is_valid_pubkey(p))
                        .value_name("RECIPIENT_ACCOUNTS")
                        .takes_value(true)
                        .multiple(true)
                        .min_values(0)
                        .index(3)
                        .help("Accounts to airdrop to"),
                )
                .arg(
                    Arg::new("recipients_csv_file")
                        .short('f')
                        .long("file")
                        .value_name("RECIPIENTS_CSV_FILE")
                        .takes_value(true)
                        .global(true)
                        .value_parser(parse_url_or_moniker)
                        .help("CSV file containing a list of recipient accounts"),
                ),
        )
        .get_matches();

    let (command, matches) = app_matches.subcommand().unwrap();
    let mut wallet_manager: Option<Rc<RemoteWalletManager>> = None;

    let cli_config = if let Some(config_file) = matches.value_of("config_file") {
        solana_cli_config::Config::load(config_file).unwrap_or_default()
    } else {
        solana_cli_config::Config::default()
    };

    let config = {
        let default_signer = DefaultSigner::new(
            "fee_payer",
            matches
                .value_of("fee_payer")
                .map(|s| s.to_string())
                .unwrap_or_else(|| cli_config.keypair_path.clone()),
        );

        let json_rpc_url = normalize_to_url_if_moniker(
            matches
                .value_of("json_rpc_url")
                .unwrap_or(&cli_config.json_rpc_url),
        );

        Config {
            commitment_config: CommitmentConfig::confirmed(),
            default_signer: default_signer
                .signer_from_path(matches, &mut wallet_manager)
                .unwrap_or_else(|err| {
                    eprintln!("error: {err}");
                    exit(1);
                }),
            json_rpc_url,
            verbose: matches.is_present("verbose"),
        }
    };
    solana_logger::setup_with_default("solana=info");

    if config.verbose {
        println!("JSON RPC URL: {}", config.json_rpc_url);
    }
    let rpc_client =
        RpcClient::new_with_commitment(config.json_rpc_url.clone(), config.commitment_config);

    match (command, matches) {
        ("airdrop", arg_matches) => {
            let token = pubkey_of_signer(arg_matches, "token", &mut wallet_manager)
                .unwrap()
                .unwrap();
            let amount = match arg_matches.value_of("amount").unwrap() {
                "ALL" => None,
                amount => Some(amount.parse::<f64>().unwrap()),
            };
            println!("{:?}", token);
            println!("{:?}", amount.unwrap());
        }
        ("create-extra-metas", arg_matches) => {
            let program_id = pubkey_of_signer(arg_matches, "program_id", &mut wallet_manager)
                .unwrap()
                .unwrap();
            let token = pubkey_of_signer(arg_matches, "token", &mut wallet_manager)
                .unwrap()
                .unwrap();
            let transfer_hook_accounts = arg_matches
                .get_many::<Vec<ExtraAccountMeta>>("transfer_hook_accounts")
                .unwrap_or_default()
                .flatten()
                .cloned()
                .collect();
            let mint_authority = DefaultSigner::new(
                "mint_authority",
                matches
                    .value_of("mint_authority")
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| cli_config.keypair_path.clone()),
            )
            .signer_from_path(matches, &mut wallet_manager)
            .unwrap_or_else(|err| {
                eprintln!("error: {err}");
                exit(1);
            });
            let signature = process_create_extra_account_metas(
                &rpc_client,
                &program_id,
                &token,
                transfer_hook_accounts,
                mint_authority.as_ref(),
                config.default_signer.as_ref(),
            )
            .await
            .unwrap_or_else(|err| {
                eprintln!("error: send transaction: {err}");
                exit(1);
            });
            println!("Signature: {signature}");
        }
        ("update-extra-metas", arg_matches) => {
            let program_id = pubkey_of_signer(arg_matches, "program_id", &mut wallet_manager)
                .unwrap()
                .unwrap();
            let token = pubkey_of_signer(arg_matches, "token", &mut wallet_manager)
                .unwrap()
                .unwrap();
            let transfer_hook_accounts = arg_matches
                .get_many::<Vec<ExtraAccountMeta>>("transfer_hook_accounts")
                .unwrap_or_default()
                .flatten()
                .cloned()
                .collect();
            let mint_authority = DefaultSigner::new(
                "mint_authority",
                matches
                    .value_of("mint_authority")
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| cli_config.keypair_path.clone()),
            )
            .signer_from_path(matches, &mut wallet_manager)
            .unwrap_or_else(|err| {
                eprintln!("error: {err}");
                exit(1);
            });
            let signature = process_update_extra_account_metas(
                &rpc_client,
                &program_id,
                &token,
                transfer_hook_accounts,
                mint_authority.as_ref(),
                config.default_signer.as_ref(),
            )
            .await
            .unwrap_or_else(|err| {
                eprintln!("error: send transaction: {err}");
                exit(1);
            });
            println!("Signature: {signature}");
        }
        _ => unreachable!(),
    };

    Ok(())
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_sdk::{bpf_loader_upgradeable, instruction::AccountMeta, signer::keypair::Keypair},
        solana_test_validator::{TestValidator, TestValidatorGenesis, UpgradeableProgramInfo},
        spl_token_client::{
            client::{
                ProgramClient, ProgramRpcClient, ProgramRpcClientSendTransaction, SendTransaction,
                SimulateTransaction,
            },
            token::Token,
        },
        std::{path::PathBuf, sync::Arc},
    };

    async fn new_validator_for_test(program_id: Pubkey) -> (TestValidator, Keypair) {
        solana_logger::setup();
        let mut test_validator_genesis = TestValidatorGenesis::default();
        test_validator_genesis.add_upgradeable_programs_with_path(&[UpgradeableProgramInfo {
            program_id,
            loader: bpf_loader_upgradeable::id(),
            program_path: PathBuf::from("../../../target/deploy/spl_transfer_hook_example.so"),
            upgrade_authority: Pubkey::new_unique(),
        }]);
        test_validator_genesis.start_async().await
    }

    async fn setup_mint<T: SendTransaction + SimulateTransaction>(
        program_id: &Pubkey,
        mint_authority: &Pubkey,
        decimals: u8,
        payer: Arc<dyn Signer>,
        client: Arc<dyn ProgramClient<T>>,
    ) -> Token<T> {
        let mint_account = Keypair::new();
        let token = Token::new(
            client,
            program_id,
            &mint_account.pubkey(),
            Some(decimals),
            payer,
        );
        token
            .create_mint(mint_authority, None, vec![], &[&mint_account])
            .await
            .unwrap();
        token
    }

    #[tokio::test]
    async fn test_create() {
        let program_id = Pubkey::new_unique();

        let (test_validator, payer) = new_validator_for_test(program_id).await;
        let payer: Arc<dyn Signer> = Arc::new(payer);
        let rpc_client = Arc::new(test_validator.get_async_rpc_client());
        let client = Arc::new(ProgramRpcClient::new(
            rpc_client.clone(),
            ProgramRpcClientSendTransaction,
        ));

        let mint_authority = Keypair::new();
        let decimals = 2;

        let token = setup_mint(
            &spl_token_2022::id(),
            &mint_authority.pubkey(),
            decimals,
            payer.clone(),
            client.clone(),
        )
        .await;

        let required_address = Pubkey::new_unique();
        let accounts = vec![AccountMeta::new_readonly(required_address, false)];
        process_create_extra_account_metas(
            &rpc_client,
            &program_id,
            token.get_address(),
            accounts.iter().map(|a| a.into()).collect(),
            &mint_authority,
            payer.as_ref(),
        )
        .await
        .unwrap();

        let extra_account_metas_address =
            get_extra_account_metas_address(token.get_address(), &program_id);
        let account = rpc_client
            .get_account(&extra_account_metas_address)
            .await
            .unwrap();
        assert_eq!(account.owner, program_id);
    }
}
