use crate::{
    traits::{NamedStep, PollableAsyncRunType, PollableAsyncStep, Processable},
    types::transaction_context::{TransactionContext, TransactionMetadata},
    utils::errors::ProcessorError,
};
use anyhow::Result;
use cedra_indexer_transaction_stream::{
    TransactionStream as TransactionStreamInternal, TransactionStreamConfig,
};
use cedra_protos::transaction::v1::Transaction;
use async_trait::async_trait;
use mockall::mock;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

// TransactionStreamStep is establishes a gRPC connection with Transaction Stream
// fetches transactions, and outputs them for processing. It also handles reconnections with retries.
// This is usually the initial step in a processor.
pub struct TransactionStreamStep
where
    Self: Sized + Send + 'static,
{
    transaction_stream_config: TransactionStreamConfig,
    pub transaction_stream: Mutex<TransactionStreamInternal>,
}

impl TransactionStreamStep
where
    Self: Sized + Send + 'static,
{
    pub async fn new(
        transaction_stream_config: TransactionStreamConfig,
    ) -> Result<Self, ProcessorError> {
        let transaction_stream_res =
            TransactionStreamInternal::new(transaction_stream_config.clone()).await;
        match transaction_stream_res {
            Err(e) => Err(ProcessorError::StepInitError {
                message: format!("Error creating transaction stream: {:?}", e),
            }),
            Ok(transaction_stream) => Ok(Self {
                transaction_stream: Mutex::new(transaction_stream),
                transaction_stream_config,
            }),
        }
    }
}

#[async_trait]
impl Processable for TransactionStreamStep
where
    Self: Sized + Send + 'static,
{
    type Input = ();
    // The TransactionStreamStep will output a batch of transactions for processing
    type Output = Vec<Transaction>;
    type RunType = PollableAsyncRunType;

    async fn process(
        &mut self,
        _item: TransactionContext<()>,
    ) -> Result<Option<TransactionContext<Vec<Transaction>>>, ProcessorError> {
        Ok(None)
    }
}

#[async_trait]
impl PollableAsyncStep for TransactionStreamStep
where
    Self: Sized + Send + Sync + 'static,
{
    fn poll_interval(&self) -> std::time::Duration {
        Duration::from_secs(0)
    }

    async fn poll(
        &mut self,
    ) -> Result<Option<Vec<TransactionContext<Vec<Transaction>>>>, ProcessorError> {
        let txn_pb_response_res = self
            .transaction_stream
            .lock()
            .await
            .get_next_transaction_batch()
            .await;
        match txn_pb_response_res {
            Ok(txn_pb_response) => {
                let transactions_with_context = TransactionContext {
                    data: txn_pb_response.transactions,
                    metadata: TransactionMetadata {
                        start_version: txn_pb_response.start_version,
                        end_version: txn_pb_response.end_version,
                        start_transaction_timestamp: txn_pb_response.start_txn_timestamp,
                        end_transaction_timestamp: txn_pb_response.end_txn_timestamp,
                        total_size_in_bytes: txn_pb_response.size_in_bytes,
                    },
                };
                Ok(Some(vec![transactions_with_context]))
            },
            Err(e) => {
                warn!(
                    stream_address = self.transaction_stream_config.indexer_grpc_data_service_address.to_string(),
                    error = ?e,
                    "Error fetching transactions from TransactionStream. Attempting to reconnect."
                );

                // TransactionStream closes connections every 5 minutes. We should try to reconnect
                match self
                    .transaction_stream
                    .lock()
                    .await
                    .reconnect_to_grpc_with_retries()
                    .await
                {
                    Ok(_) => {
                        info!(
                            stream_address = self
                                .transaction_stream_config
                                .indexer_grpc_data_service_address
                                .to_string(),
                            "Successfully reconnected to TransactionStream."
                        );
                        // Return nothing for now. The next poll will fetch the next batch of transactions.
                        Ok(None)
                    },
                    Err(e) => {
                        error!(
                            stream_address = self.transaction_stream_config
                                .indexer_grpc_data_service_address
                                .to_string(),
                            error = ?e,
                            " Error reconnecting transaction stream."
                        );
                        Err(ProcessorError::PollError {
                            message: format!("Error reconnecting to TransactionStream: {:?}", e),
                        })
                    },
                }
            },
        }
    }

    async fn should_continue_polling(&mut self) -> bool {
        let is_end = self.transaction_stream.lock().await.is_end_of_stream();
        if is_end {
            info!("Reached ending version");
        }
        !is_end
    }
}

impl NamedStep for TransactionStreamStep {
    fn name(&self) -> String {
        "TransactionStreamStep".to_string()
    }
}

mock! {
    pub TransactionStreamStep {}

    #[async_trait]
    impl Processable for TransactionStreamStep
    where Self: Sized + Send + 'static,
    {
        type Input = ();
        type Output = Vec<Transaction>;
        type RunType = PollableAsyncRunType;

        async fn init(&mut self);

        async fn process(&mut self, _item: TransactionContext<()> ) -> Result<Option<TransactionContext<Vec<Transaction>>>, ProcessorError>;
    }

    #[async_trait]
    impl PollableAsyncStep for TransactionStreamStep
    where
        Self: Sized + Send + 'static,
    {
        fn poll_interval(&self) -> std::time::Duration;

        // async fn poll(&mut self) -> Option<Vec<TransactionsPBResponse>> {
        //     // Testing framework can provide mocked transactions here
        //     Some(vec![TransactionsPBResponse {
        //         transactions: vec![],
        //         chain_id: 0,
        //         start_version: 0,
        //         end_version: 100,
        //         start_txn_timestamp: None,
        //         end_txn_timestamp: None,
        //         size_in_bytes: 10,
        //     }])
        // }
        async fn poll(&mut self) -> Result<Option<Vec<TransactionContext<Vec<Transaction>>>>, ProcessorError>;

        async fn should_continue_polling(&mut self) -> bool;
    }

    impl NamedStep for TransactionStreamStep {
        fn name(&self) -> String;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        builder::ProcessorBuilder,
        test::{steps::pass_through_step::PassThroughStep, utils::receive_with_timeout},
        traits::IntoRunnableStep,
        types::transaction_context::TransactionMetadata,
    };
    use mockall::Sequence;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::needless_return)]
    async fn test_transaction_stream() {
        let mut mock_transaction_stream = MockTransactionStreamStep::new();
        // Testing framework can provide mocked transactions here
        mock_transaction_stream.expect_poll().returning(|| {
            Ok(Some(vec![TransactionContext {
                data: vec![Transaction::default()],
                metadata: TransactionMetadata {
                    start_version: 0,
                    end_version: 100,
                    start_transaction_timestamp: None,
                    end_transaction_timestamp: None,
                    total_size_in_bytes: 10,
                },
            }]))
        });
        mock_transaction_stream
            .expect_poll_interval()
            .returning(|| Duration::from_secs(0));
        mock_transaction_stream.expect_init().returning(|| {
            // Do nothing
        });
        mock_transaction_stream
            .expect_name()
            .returning(|| "MockTransactionStream".to_string());

        // Set up the mock transaction stream to poll 3 times
        let mut seq = Sequence::new();
        mock_transaction_stream
            .expect_should_continue_polling()
            .times(3)
            .in_sequence(&mut seq)
            .return_const(true);
        mock_transaction_stream
            .expect_should_continue_polling()
            .return_const(false);

        let pass_through_step = PassThroughStep::default();

        let (_, mut output_receiver) = ProcessorBuilder::new_with_inputless_first_step(
            mock_transaction_stream.into_runnable_step(),
        )
        .connect_to(pass_through_step.into_runnable_step(), 5)
        .end_and_return_output_receiver(5);

        tokio::time::sleep(Duration::from_millis(250)).await;
        for _ in 0..3 {
            let result = receive_with_timeout(&mut output_receiver, 100)
                .await
                .unwrap();

            assert_eq!(result.data.len(), 1);
        }

        // After receiving 3 outputs, the channel should be empty
        let result = receive_with_timeout(&mut output_receiver, 100).await;
        assert!(result.is_none());
    }
}
