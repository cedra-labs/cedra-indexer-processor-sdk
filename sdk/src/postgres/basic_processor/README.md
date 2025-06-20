# Custom processor function 

Utility function that lets you create a Postgres processor. It works by running the code in `run_processor` method and applying a `process_function` on each transaction. 

## How to use
1. Install Postgres and Diesel CLI
2. Add the `cedra-indexer-processor-sdk` crate with the `postgres_full` feature in the `[dependencies]` section of your `Config.toml`:
```
cedra-indexer-processor-sdk = { git = "https://github.com/cedra-labs/cedra-indexer-processor-sdk.git", rev = "{COMMIT_HASH}", features = ["postgres_full"] }
```
3. Setup Diesel and define your DB migrations. 
4. In `main.rs`, call the `process` function with your indexing logic. You'll need to implement this part:
```
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("/path/to/src/db/migrations");
process(
    "processor_name".to_string(),
    MIGRATIONS, 
    async |transactions, conn_pool| {
        // Implement your indexing logic
    },
)
.await?;
```
The `process` function is an abstraction around a regular SDK processor. 

It runs your db migrations, validates the chain id, connects to Transaction Stream, tracks the last successful version, and processes transactions using your custom indexing logic. 

See [`postgres-basic-events-example`](https://github.com/cedra-labs/cedra-indexer-processor-sdk/tree/main/examples/postgres-basic-events-example) for an example on how to use this function to create a simple processor that writes events to Postgres. 

5. Construct a `config.yaml` file with this example:
```
# This is a template yaml for the processor
health_check_port: 8085
server_config:
  transaction_stream_config:
    indexer_grpc_data_service_address: "https://grpc.mainnet.cedralabs.com:443"
    auth_token: "AUTH_TOKEN"
    request_name_header: "PROCESSOR_NAME"
    starting_version: 0
  postgres_config:
    connection_string: postgresql://postgres:@localhost:5432/example
```
6. Run processor using this command `cargo run -p postgres-basic-events-example -- -c /path/to/config.yaml`
