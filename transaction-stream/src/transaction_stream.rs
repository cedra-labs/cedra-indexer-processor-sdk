use crate::{
    config::TransactionStreamConfig,
    utils::{additional_headers::AdditionalHeaders, time::timestamp_to_iso},
};
use anyhow::{anyhow, Result};
use cedra_moving_average::MovingAverage;
use cedra_protos::{
    indexer::v1::{raw_data_client::RawDataClient, GetTransactionsRequest, TransactionsResponse},
    transaction::v1::Transaction,
    util::timestamp::Timestamp,
};
use cedra_transaction_filter::BooleanTransactionFilter;
use futures_util::StreamExt;
use prost::Message;
use sample::{sample, SampleRate};
use std::time::Duration;
use tokio::time::timeout;
use tonic::{Response, Streaming};
use tracing::{error, info, warn};

/// GRPC request metadata key for the token ID.
const GRPC_API_GATEWAY_API_KEY_HEADER: &str = "authorization";
/// GRPC request metadata key for the request name. This is used to identify the
/// data destination.
const GRPC_REQUEST_NAME_HEADER: &str = "x-cedra-request-name";
/// GRPC connection id
const GRPC_CONNECTION_ID: &str = "x-cedra-connection-id";
/// 256MB
pub const MAX_RESPONSE_SIZE: usize = 1024 * 1024 * 256;

/// TransactionsPBResponse is a struct that holds the transactions fetched from the stream.
/// It also includes some contextual information about the transactions.
#[derive(Clone)]
pub struct TransactionsPBResponse {
    pub transactions: Vec<Transaction>,
    pub chain_id: u64,
    // We put start/end versions here as filtering means there are potential "gaps" here now
    pub start_version: u64,
    pub end_version: u64,
    pub start_txn_timestamp: Option<Timestamp>,
    pub end_txn_timestamp: Option<Timestamp>,
    pub size_in_bytes: u64,
}

/// Helper function to build a GRPC request for fetching transactions.
pub fn grpc_request_builder(
    starting_version: Option<u64>,
    transactions_count: Option<u64>,
    grpc_auth_token: String,
    request_name_header: String,
    additional_headers: AdditionalHeaders,
    transaction_filter: Option<BooleanTransactionFilter>,
) -> tonic::Request<GetTransactionsRequest> {
    let mut request = tonic::Request::new(GetTransactionsRequest {
        starting_version,
        transactions_count,
        transaction_filter: transaction_filter.map(Into::into),
        ..GetTransactionsRequest::default()
    });
    request.metadata_mut().insert(
        GRPC_API_GATEWAY_API_KEY_HEADER,
        format!("Bearer {}", grpc_auth_token.clone())
            .parse()
            .unwrap(),
    );
    request.metadata_mut().insert(
        GRPC_REQUEST_NAME_HEADER,
        request_name_header.parse().unwrap(),
    );
    additional_headers.drain_into_metadata_map(request.metadata_mut());
    request
}

/// Given a `TransactionStreamConfig`, this function will return a stream of transactions.
/// It also handles timeouts and retries.
pub async fn get_stream(
    transaction_stream_config: TransactionStreamConfig,
) -> Result<Response<Streaming<TransactionsResponse>>> {
    info!(
        stream_address = transaction_stream_config
            .indexer_grpc_data_service_address
            .to_string(),
        start_version = transaction_stream_config.starting_version,
        end_version = transaction_stream_config.request_ending_version,
        "[Transaction Stream] Setting up rpc channel"
    );

    let channel = tonic::transport::Channel::from_shared(
        transaction_stream_config
            .indexer_grpc_data_service_address
            .to_string(),
    )
    .expect(
        "[Transaction Stream] Failed to build GRPC channel, perhaps because the data service URL is invalid",
    )
    .http2_keep_alive_interval(transaction_stream_config.indexer_grpc_http2_ping_interval())
    .keep_alive_timeout(transaction_stream_config.indexer_grpc_http2_ping_timeout());

    // If the scheme is https, add a TLS config.
    let channel = if transaction_stream_config
        .indexer_grpc_data_service_address
        .scheme()
        == "https"
    {
        let config = tonic::transport::channel::ClientTlsConfig::new();
        channel
            .tls_config(config)
            .expect("[Transaction Stream] Failed to create TLS config")
    } else {
        channel
    };

    info!(
        stream_address = transaction_stream_config
            .indexer_grpc_data_service_address
            .to_string(),
        start_version = transaction_stream_config.starting_version,
        end_version = transaction_stream_config.request_ending_version,
        "[Transaction Stream] Setting up GRPC client"
    );

    // TODO: move this to a config file
    // Retry this connection a few times before giving up
    let mut connect_retries = 0;
    let res = loop {
        let res = timeout(
            transaction_stream_config.indexer_grpc_reconnection_timeout(),
            RawDataClient::connect(channel.clone()),
        )
        .await;
        match res {
            Ok(connect_res) => match connect_res {
                Ok(client) => break Ok(client),
                Err(e) => {
                    error!(
                        stream_address = transaction_stream_config.indexer_grpc_data_service_address.to_string(),
                        start_version = transaction_stream_config.starting_version,
                        end_version = transaction_stream_config.request_ending_version,
                        error = ?e,
                        "[Transaction Stream] Error connecting to GRPC client"
                    );
                    connect_retries += 1;
                    if connect_retries
                        >= transaction_stream_config.indexer_grpc_reconnection_max_retries
                    {
                        break Err(anyhow!("Error connecting to GRPC client").context(e));
                    }
                },
            },
            Err(e) => {
                error!(
                    stream_address = transaction_stream_config.indexer_grpc_data_service_address.to_string(),
                    start_version = transaction_stream_config.starting_version,
                    end_version = transaction_stream_config.request_ending_version,
                    retries = connect_retries,
                    error = ?e,
                    "[Transaction Stream] Timed out connecting to GRPC client"
                );
                connect_retries += 1;
                if connect_retries
                    >= transaction_stream_config.indexer_grpc_reconnection_max_retries
                {
                    break Err(anyhow!("Timed out connecting to GRPC client"));
                }
            },
        }
    };

    let raw_data_client = res?;

    let mut rpc_client = raw_data_client
        .accept_compressed(tonic::codec::CompressionEncoding::Zstd)
        .send_compressed(tonic::codec::CompressionEncoding::Zstd)
        .max_decoding_message_size(MAX_RESPONSE_SIZE)
        .max_encoding_message_size(MAX_RESPONSE_SIZE);

    let count = transaction_stream_config.request_ending_version.map(|v| {
        (v as i64
            - transaction_stream_config
                .starting_version
                .unwrap_or_else(|| {
                    panic!("starting_version is required when using request_ending_version")
                }) as i64
            + 1) as u64
    });

    info!(
        stream_address = transaction_stream_config.indexer_grpc_data_service_address.to_string(),
        start_version = transaction_stream_config.starting_version,
        end_version = transaction_stream_config.request_ending_version,
        num_of_transactions = ?count,
        "[Transaction Stream] Setting up GRPC stream",
    );

    // TODO: move this to a config file
    // Retry this connection a few times before giving up
    let mut connect_retries = 0;
    loop {
        let timeout_res = timeout(
            transaction_stream_config.indexer_grpc_reconnection_timeout(),
            async {
                let request = grpc_request_builder(
                    transaction_stream_config.starting_version,
                    count,
                    transaction_stream_config.auth_token.clone(),
                    transaction_stream_config.request_name_header.clone(),
                    transaction_stream_config.additional_headers.clone(),
                    transaction_stream_config.transaction_filter.clone(),
                );
                rpc_client.get_transactions(request).await
            },
        )
        .await;
        match timeout_res {
            Ok(response_res) => match response_res {
                Ok(response) => break Ok(response),
                Err(e) => {
                    error!(
                        stream_address = transaction_stream_config.indexer_grpc_data_service_address.to_string(),
                        start_version = transaction_stream_config.starting_version,
                        end_version = transaction_stream_config.request_ending_version,
                        error = ?e,
                        "[Transaction Stream] Error making grpc request. Retrying..."
                    );
                    connect_retries += 1;
                    if connect_retries
                        >= transaction_stream_config.indexer_grpc_reconnection_max_retries
                    {
                        break Err(anyhow!("Error making grpc request").context(e));
                    }
                },
            },
            Err(e) => {
                error!(
                    stream_address = transaction_stream_config.indexer_grpc_data_service_address.to_string(),
                    start_version = transaction_stream_config.starting_version,
                    end_version = transaction_stream_config.request_ending_version,
                    retries = connect_retries,
                    error = ?e,
                    "[Transaction Stream] Timeout making grpc request. Retrying...",
                );
                connect_retries += 1;
                if connect_retries
                    >= transaction_stream_config.indexer_grpc_reconnection_max_retries
                {
                    break Err(anyhow!("Timeout making grpc request").context(e));
                }
            },
        }
    }
}

/// Helper function to get the chain id from the stream.
pub async fn get_chain_id(transaction_stream_config: TransactionStreamConfig) -> Result<u64> {
    info!(
        stream_address = transaction_stream_config
            .indexer_grpc_data_service_address
            .to_string(),
        "[Transaction Stream] Connecting to GRPC stream to get chain id",
    );

    let transaction_stream_config_for_chain_id = TransactionStreamConfig {
        starting_version: Some(1),
        request_ending_version: Some(2),
        ..transaction_stream_config.clone()
    };
    let response = get_stream(transaction_stream_config_for_chain_id).await?;
    let connection_id = match response.metadata().get(GRPC_CONNECTION_ID) {
        Some(connection_id) => connection_id.to_str().unwrap().to_string(),
        None => "".to_string(),
    };
    let mut resp_stream = response.into_inner();
    info!(
        stream_address = transaction_stream_config
            .indexer_grpc_data_service_address
            .to_string(),
        connection_id = connection_id,
        "[Transaction Stream] Successfully connected to GRPC stream to get chain id",
    );

    match tokio::time::timeout(
        transaction_stream_config.indexer_grpc_response_item_timeout(),
        resp_stream.next(),
    )
    .await
    {
        // Received datastream response
        Ok(response) => match response {
            Some(Ok(r)) => match r.chain_id {
                Some(chain_id) => Ok(chain_id),
                None => {
                    error!(
                        stream_address = transaction_stream_config
                            .indexer_grpc_data_service_address
                            .to_string(),
                        connection_id = connection_id,
                        "[Transaction Stream] Chain Id doesn't exist."
                    );
                    Err(anyhow!("Chain Id doesn't exist"))
                },
            },
            Some(Err(rpc_error)) => {
                error!(
                    stream_address = transaction_stream_config.indexer_grpc_data_service_address.to_string(),
                    connection_id = connection_id,
                    error = ?rpc_error,
                    "[Transaction Stream] Error receiving datastream response for chain id"
                );
                Err(anyhow!("Error receiving datastream response for chain id").context(rpc_error))
            },
            None => {
                error!(
                    stream_address = transaction_stream_config
                        .indexer_grpc_data_service_address
                        .to_string(),
                    connection_id = connection_id,
                    "[Transaction Stream] Stream ended before getting response fo for chain id"
                );
                Err(anyhow!("Stream ended before getting response for chain id"))
            },
        },
        // Timeout receiving datastream response
        Err(e) => {
            warn!(
                stream_address = transaction_stream_config.indexer_grpc_data_service_address.to_string(),
                connection_id = connection_id,
                start_version = transaction_stream_config.starting_version,
                end_version = transaction_stream_config.request_ending_version,
                error = ?e,
                "[Transaction Stream] Timeout receiving datastream response for chain id."
            );
            Err(anyhow!(
                "Timeout receiving datastream response for chain id"
            ))
        },
    }
}

/// TransactionStream is a struct that holds the state of the stream and provides methods to fetch transactions
/// from the stream.
/// - init_stream: Initializes the stream and returns the stream and connection id
/// - get_next_transaction_batch: Fetches the next batch of transactions from the stream
/// - is_end_of_stream: Checks if we've reached the end of the stream. This is determined by the ending version set in `TransactionStreamConfig`
/// - reconnect_to_grpc: Reconnects to the GRPC stream
/// - get_chain_id: Fetches the chain id from the stream
pub struct TransactionStream {
    transaction_stream_config: TransactionStreamConfig,
    stream: Streaming<TransactionsResponse>,
    connection_id: String,
    reconnection_retries: u64,
    last_fetched_version: Option<i64>,
    fetch_ma: MovingAverage,
}

impl TransactionStream {
    pub async fn new(transaction_stream_config: TransactionStreamConfig) -> Result<Self> {
        let (stream, connection_id) = Self::init_stream(transaction_stream_config.clone()).await?;
        Ok(Self {
            transaction_stream_config: transaction_stream_config.clone(),
            stream,
            connection_id,
            reconnection_retries: 0,
            last_fetched_version: transaction_stream_config
                .starting_version
                .map(|v| v as i64 - 1),
            fetch_ma: MovingAverage::new(3000),
        })
    }

    async fn init_stream(
        transaction_stream_config: TransactionStreamConfig,
    ) -> Result<(Streaming<TransactionsResponse>, String)> {
        info!(
            stream_address = transaction_stream_config
                .indexer_grpc_data_service_address
                .to_string(),
            start_version = transaction_stream_config.starting_version,
            end_version = transaction_stream_config.request_ending_version,
            "[Transaction Stream] Connecting to GRPC stream",
        );
        let resp_stream = get_stream(transaction_stream_config.clone()).await?;
        let connection_id = match resp_stream.metadata().get(GRPC_CONNECTION_ID) {
            Some(connection_id) => connection_id.to_str().unwrap().to_string(),
            None => "".to_string(),
        };
        info!(
            stream_address = transaction_stream_config
                .indexer_grpc_data_service_address
                .to_string(),
            connection_id = connection_id,
            start_version = transaction_stream_config.starting_version,
            end_version = transaction_stream_config.request_ending_version,
            "[Transaction Stream] Successfully connected to GRPC stream",
        );
        Ok((resp_stream.into_inner(), connection_id))
    }

    /// Gets a batch of transactions from the stream. Batch size is set in the grpc server.
    /// The number of batches depends on our config
    /// There could be several special scenarios:
    /// 1. If we lose the connection, we will try reconnecting X times within Y seconds before crashing.
    /// 2. If we specified an end version and we hit that, we will stop fetching, but we will make sure that
    ///    all existing transactions are processed
    ///
    /// Returns
    /// - true if should continue fetching
    /// - false if we reached the end of the stream or there is an error and the loop should stop
    pub async fn get_next_transaction_batch(&mut self) -> Result<TransactionsPBResponse> {
        let grpc_channel_recv_latency = std::time::Instant::now();

        let txn_pb_res = match tokio::time::timeout(
            self.transaction_stream_config
                .indexer_grpc_response_item_timeout(),
            self.stream.next(),
        )
        .await
        {
            // Received datastream response
            Ok(response) => {
                match response {
                    Some(Ok(r)) => {
                        self.reconnection_retries = 0;

                        // The processed range may not exist if using the v1 transaction stream.
                        // In the case that it doesn't exist, use the previous behavior of using the transaction version of the first and last transactions.
                        let start_version = match r.processed_range {
                            Some(range) => range.first_version,
                            None => r.transactions.as_slice().first().unwrap().version,
                        };
                        let end_version = match r.processed_range {
                            Some(range) => range.last_version,
                            None => r.transactions.as_slice().last().unwrap().version,
                        };

                        // The processed range does not contain a timestamp, so we use the timestamp of the first and last transactions.
                        let start_txn_timestamp =
                            r.transactions.as_slice().first().and_then(|t| t.timestamp);
                        let end_txn_timestamp =
                            r.transactions.as_slice().last().and_then(|t| t.timestamp);

                        let size_in_bytes = r.encoded_len() as u64;
                        let chain_id: u64 = r
                            .chain_id
                            .expect("[Transaction Stream] Chain Id doesn't exist.");
                        let num_txns = r.transactions.len();
                        let duration_in_secs = grpc_channel_recv_latency.elapsed().as_secs_f64();
                        self.fetch_ma.tick_now(num_txns as u64);

                        sample!(
                            SampleRate::Duration(Duration::from_secs(1)),
                            info!(
                                stream_address = self
                                    .transaction_stream_config
                                    .indexer_grpc_data_service_address
                                    .to_string(),
                                connection_id = self.connection_id,
                                start_version = start_version,
                                end_version = end_version,
                                start_txn_timestamp_iso = start_txn_timestamp
                                    .as_ref()
                                    .map(timestamp_to_iso)
                                    .unwrap_or_default(),
                                end_txn_timestamp_iso = end_txn_timestamp
                                    .as_ref()
                                    .map(timestamp_to_iso)
                                    .unwrap_or_default(),
                                num_of_transactions = end_version - start_version + 1,
                                size_in_bytes = size_in_bytes,
                                duration_in_secs = duration_in_secs,
                                tps = self.fetch_ma.avg().ceil() as u64,
                                bytes_per_sec = size_in_bytes as f64 / duration_in_secs,
                                "[Transaction Stream] Received transactions from GRPC.",
                            )
                        );

                        if let Some(last_fetched_version) = self.last_fetched_version {
                            if last_fetched_version + 1 != start_version as i64 {
                                error!(
                                    last_fetched_version = self.last_fetched_version, // last fetched version
                                    expected_start_version =
                                        self.last_fetched_version.map(|v| v + 1), // expected start version
                                    actual_start_version = start_version, // actual start version
                                    "[Transaction Stream] Received batch with gap from GRPC stream"
                                );
                                return Err(anyhow!("Received batch with gap from GRPC stream"));
                            }
                        }
                        self.last_fetched_version = Some(end_version as i64);

                        let txn_pb = TransactionsPBResponse {
                            transactions: r.transactions,
                            chain_id,
                            start_version,
                            end_version,
                            start_txn_timestamp,
                            end_txn_timestamp,
                            size_in_bytes,
                        };

                        Ok(txn_pb)
                    },
                    // Error receiving datastream response
                    Some(Err(rpc_error)) => {
                        warn!(
                            stream_address = self.transaction_stream_config.indexer_grpc_data_service_address.to_string(),
                            connection_id = self.connection_id,
                            start_version = self.transaction_stream_config.starting_version,
                            end_version = self.transaction_stream_config.request_ending_version,
                            error = ?rpc_error,
                            "[Transaction Stream] Error receiving datastream response."
                        );
                        Err(anyhow!("Error receiving datastream response"))
                    },
                    // Stream is finished
                    None => {
                        warn!(
                            stream_address = self
                                .transaction_stream_config
                                .indexer_grpc_data_service_address
                                .to_string(),
                            connection_id = self.connection_id,
                            start_version = self.transaction_stream_config.starting_version,
                            end_version = self.transaction_stream_config.request_ending_version,
                            "[Transaction Stream] Stream ended."
                        );
                        Err(anyhow!("Stream ended"))
                    },
                }
            },
            // Timeout receiving datastream response
            Err(e) => {
                warn!(
                    stream_address = self.transaction_stream_config.indexer_grpc_data_service_address.to_string(),
                    connection_id = self.connection_id,
                    start_version = self.transaction_stream_config.starting_version,
                    end_version = self.transaction_stream_config.request_ending_version,
                    error = ?e,
                    "[Transaction Stream] Timeout receiving datastream response."
                );
                Err(anyhow!("Timeout receiving datastream response"))
            },
        };
        txn_pb_res
    }

    /// Helper function to signal that we've fetched all the transactions up to the ending version that was requested.
    pub fn is_end_of_stream(&self) -> bool {
        if let (Some(ending_version), Some(last_fetched_version)) = (
            self.transaction_stream_config.request_ending_version,
            self.last_fetched_version,
        ) {
            // If we've already fetched ending_version inclusive, then we're done
            last_fetched_version >= ending_version as i64
        } else {
            false
        }
    }

    pub async fn reconnect_to_grpc_with_retries(&mut self) -> Result<()> {
        let mut reconnection_retries = 0;

        loop {
            // Sleep for 100ms between reconnect tries
            // TODO: Turn this into exponential backoff
            tokio::time::sleep(Duration::from_millis(100)).await;

            reconnection_retries += 1;

            if reconnection_retries
                >= self
                    .transaction_stream_config
                    .indexer_grpc_reconnection_max_retries
            {
                error!(
                    stream_address = self
                        .transaction_stream_config
                        .indexer_grpc_data_service_address
                        .to_string(),
                    "[Transaction Stream] Reconnected more than 100 times. Will not retry.",
                );
                break Err(anyhow!("Reconnected more than 100 times. Will not retry."));
            }

            match self.reconnect_to_grpc().await {
                Ok(_) => {
                    break Ok(());
                },
                Err(e) => {
                    error!(
                        stream_address = self.transaction_stream_config
                            .indexer_grpc_data_service_address
                            .to_string(),
                        error = ?e,
                        "[Transaction Stream] Error reconnecting to GRPC stream. Retrying..."
                    );
                    continue;
                },
            }
        }
    }

    pub async fn reconnect_to_grpc(&mut self) -> Result<()> {
        // Upon reconnection, requested starting version should be the last fetched version + 1
        let request_starting_version = self.last_fetched_version.map(|v| (v + 1) as u64);
        info!(
            stream_address = self
                .transaction_stream_config
                .indexer_grpc_data_service_address
                .to_string(),
            requested_starting_version = request_starting_version,
            requested_ending_version = self.transaction_stream_config.request_ending_version,
            reconnection_retries = self.reconnection_retries,
            "[Transaction Stream] Reconnecting to GRPC stream"
        );
        let response = get_stream(TransactionStreamConfig {
            starting_version: request_starting_version,
            ..self.transaction_stream_config.clone()
        })
        .await?;
        let connection_id = match response.metadata().get(GRPC_CONNECTION_ID) {
            Some(connection_id) => connection_id.to_str().unwrap().to_string(),
            None => "".to_string(),
        };
        self.connection_id = connection_id;
        self.stream = response.into_inner();
        info!(
            stream_address = self
                .transaction_stream_config
                .indexer_grpc_data_service_address
                .to_string(),
            connection_id = self.connection_id,
            starting_version = request_starting_version,
            ending_version = self.transaction_stream_config.request_ending_version,
            reconnection_retries = self.reconnection_retries,
            "[Transaction Stream] Successfully reconnected to GRPC stream"
        );
        Ok(())
    }

    pub async fn get_chain_id(self) -> Result<u64> {
        get_chain_id(self.transaction_stream_config).await
    }
}
