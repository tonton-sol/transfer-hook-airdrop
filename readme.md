# Transfer-Hook Airdrop Tool

This tool is designed to automate the process of airdropping tokens with the transfer-hook extension.

## Features

- Airdrop tokens to multiple recipients.
- Read recipient addresses from a CSV file.
- Utilize command-line arguments for dynamic operation.

## Requirements

- Rust programming language
- Cargo (Rust's package manager)
- Access to a Solana RPC endpoint
- A Solana keypair file for transaction signing

## Installation

Clone the repository and build the project:

```bash
git clone https://github.com/tonton-sol/transfer-hook-airdrop
cd transfer-hook-airdrop
cargo install --path .
```

## Configuration

Before using the tool, you must have a Solana keypair file and access to an RPC endpoint. The keypair file can be specified on the command line or in a configuration file.

## Usage

Run the tool with the following command:

```bash
thook [OPTIONS] COMMAND
```

### Options

- `--rpc NETWORK_URL`: Specify the network address of your Solana RPC provider.
- `--config PATH`: Path to custom Solana configuration file.
- `--keypair KEYPAIR_FILEPATH`: Filepath to the keypair used for signing transactions.
- `--priority_fee MICROLAMPORTS`: Set the priority fee per transaction in microlamports. (Not implemented yet!)

### Commands

#### Airdrop

Airdrop tokens to the addresses listed in a specified CSV file.

```bash
thook airdrop --token_address <TOKEN_ADDRESS> --recipients_csv_path <RECIPIENTS_CSV_PATH> --amount <AMOUNT>
```

- `TOKEN_ADDRESS`: The address of the token to airdrop.
- `RECIPIENTS_CSV_PATH`: Path to the CSV file containing the addresses of the airdrop recipients.
- `AMOUNT`: The amount of the token to airdrop per address.

## Example

thook --rpc https://api.mainnet-beta.solana.com --keypair /path/to/keypair.json airdrop --token_address So11111111111111111111111111111111111111112 --recipients_csv_path /path/to/recipients.csv --amount 100
```

This command will airdrop 100 units of the specified token to each address listed in the CSV file, using the specified keypair for transaction signing and the mainnet beta network for transaction processing.

## Notes

- The transfer-hook ExtraAccountMetas account derived from the transfer hook program and mint must be configured correctly. It must contain all of the extra accounts used in the transfer-hook program.
