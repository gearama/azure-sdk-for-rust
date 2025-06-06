// Copyright (c) Microsoft Corporation. All Rights reserved
// Licensed under the MIT license.

use crate::{
    common::{connection_manager::ConnectionManager, retry_azure_operation, ManagementInstance},
    error::{ErrorKind, EventHubsError},
    models::{AmqpMessage, EventData, EventHubPartitionProperties, EventHubProperties},
    RetryOptions,
};
use async_lock::Mutex;
use azure_core::{
    error::{ErrorKind as AzureErrorKind, Result},
    http::Url,
    Uuid,
};
use azure_core_amqp::{
    error::{AmqpErrorCondition, AmqpErrorKind},
    AmqpError, AmqpManagement, AmqpManagementApis, AmqpSendOptions, AmqpSender, AmqpSenderApis,
    AmqpSession, AmqpSessionApis, AmqpSessionOptions,
};
use batch::{EventDataBatch, EventDataBatchOptions};
use std::{
    error::Error,
    sync::{Arc, OnceLock},
    {collections::HashMap, fmt::Debug},
};
use tracing::{debug, trace, warn};

/// Types used to collect messages into a "batch" before submitting them to an Event Hub.
pub(crate) mod batch;

const DEFAULT_EVENTHUBS_APPLICATION: &str = "DefaultApplicationName";

struct SenderInstance {
    #[allow(dead_code)]
    session: AmqpSession,
    sender: Arc<Mutex<AmqpSender>>,
}

#[derive(Default, Debug, Clone)]
/// Represents the options that can be set when submitting a batch of event data.
pub struct SendBatchOptions {}

/// A client that can be used to send events to an Event Hubs instance.
///
/// The [`ProducerClient`] is used to send events to an Event Hub. It can be used to send events to a specific partition
/// or to allow the Event Hubs instance to automatically select the partition.
///
/// The [`ProducerClient`] can be created with the fully qualified namespace of the Event
/// Hubs instance, the name of the Event Hub, and a `TokenCredential` implementation.
///
/// # Examples
///
/// ```no_run
/// use azure_messaging_eventhubs::ProducerClient;
/// use azure_identity::{DefaultAzureCredential, TokenCredentialOptions};
/// use std::error::Error;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
///    let fully_qualified_namespace = std::env::var("EVENT_HUB_NAMESPACE")?;
///    let eventhub_name = std::env::var("EVENT_HUB_NAME")?;
///    let my_credentials = DefaultAzureCredential::new()?;
///   let producer = ProducerClient::builder()
///    .with_application_id("your_application_id".to_string())
///    .open(&fully_qualified_namespace, &eventhub_name, my_credentials.clone()).await?;
///   Ok(())
/// }
/// ```
pub struct ProducerClient {
    sender_instances: Mutex<HashMap<Url, SenderInstance>>,
    mgmt_client: Mutex<OnceLock<ManagementInstance>>,
    connection_manager: Arc<ConnectionManager>,
    eventhub: String,
    endpoint: Url,
    application_id: Option<String>,

    /// The options used to configure retry operations.
    retry_options: RetryOptions,
}

/// Options used when sending an event to an Event Hub.
///
/// The `SendEventOptions` can be used to specify the partition to which the message should be sent.
/// If the partition is not specified, the Event Hub will automatically select a partition.
///
#[derive(Default, Debug)]
pub struct SendEventOptions {
    /// The id of the partition to which the event should be sent.
    pub partition_id: Option<String>,
}

/// Options used when sending an AMQP message to an Event Hub.
/// The `SendMessageOptions` can be used to specify the partition to which the message should be sent.
/// If the partition is not specified, the Event Hub will automatically select a partition.
#[derive(Default, Debug)]
pub struct SendMessageOptions {
    /// The id of the partition to which the message should be sent.
    pub partition_id: Option<String>,
}

impl From<SendEventOptions> for SendMessageOptions {
    fn from(options: SendEventOptions) -> Self {
        Self {
            partition_id: options.partition_id,
        }
    }
}

impl ProducerClient {
    pub(crate) fn new(
        endpoint: Url,
        eventhub: String,
        credential: Arc<dyn azure_core::credentials::TokenCredential>,
        application_id: Option<String>,
        retry_options: RetryOptions,
        custom_endpoint: Option<Url>,
    ) -> Self {
        Self {
            sender_instances: Mutex::new(HashMap::new()),
            mgmt_client: Mutex::new(OnceLock::new()),
            connection_manager: ConnectionManager::new(
                endpoint.clone(),
                application_id.clone(),
                custom_endpoint.clone(),
                credential,
                retry_options.clone(),
            ),
            eventhub,
            endpoint,
            retry_options,
            application_id,
        }
    }

    /// Returns a builder which can be used to create a new instance of [`ProducerClient`].
    ///
    /// # Arguments
    ///
    /// * `fully_qualified_namespace` - The fully qualified namespace of the Event Hubs instance.
    /// * `eventhub` - The name of the Event Hub.
    /// * `credential` - The token credential used for authorization.
    /// * `options` - The options for configuring the [`ProducerClient`].
    ///
    /// # Returns
    ///
    /// A new instance of [`ProducerClient`].
    pub fn builder() -> builders::ProducerClientBuilder {
        builders::ProducerClientBuilder::new()
    }

    /// Closes the connection to the Event Hub.
    ///
    /// This method should be called when the client is no longer needed, it will terminate all outstanding operations on the connection.
    ///
    /// Note that dropping the ProducerClient will also close the connection.
    pub async fn close(self) -> Result<()> {
        self.connection_manager.close_connection().await
    }

    fn should_retry_send_operation(e: &azure_core::Error) -> bool {
        match e.kind() {
            AzureErrorKind::Amqp => {
                warn!("Amqp operation failed: {}", e.source().unwrap());
                if let Some(e) = e.source() {
                    debug!("Error: {}", e);

                    if let Some(amqp_error) = e.downcast_ref::<Box<AmqpError>>() {
                        Self::should_retry_amqp_error(amqp_error)
                    } else if let Some(amqp_error) = e.downcast_ref::<AmqpError>() {
                        Self::should_retry_amqp_error(amqp_error)
                    } else {
                        debug!("Non AMQP error: {}", e);
                        false
                    }
                } else {
                    debug!("No source error found");
                    false
                }
            }
            _ => {
                debug!("Non AMQP error: {}", e);
                false
            }
        }
    }

    fn should_retry_amqp_error(amqp_error: &AmqpError) -> bool {
        match amqp_error.kind() {
            AmqpErrorKind::ManagementStatusCode(code, _) => {
                debug!("Management operation error: {}", code);
                match code {
                    // Retry on 408 (Request Timeout) and 429 (Too Many Requests)
                    azure_core::http::StatusCode::RequestTimeout
                    | azure_core::http::StatusCode::TooManyRequests
                    | azure_core::http::StatusCode::InternalServerError
                    | azure_core::http::StatusCode::BadGateway
                    | azure_core::http::StatusCode::ServiceUnavailable
                    | azure_core::http::StatusCode::GatewayTimeout => true,
                    _ => false,
                }
            }
            AmqpErrorKind::AmqpDescribedError(described_error) => {
                debug!("AMQP described error: {:?}", described_error);
                matches!(
                    described_error.condition(),
                    AmqpErrorCondition::ResourceLimitExceeded
                        | AmqpErrorCondition::ConnectionFramingError
                        | AmqpErrorCondition::LinkStolen
                )
            }
            _ => {
                debug!("Other AMQP error: {}", amqp_error);
                false
            }
        }
    }

    /// Sends an event to the Event Hub.
    ///
    /// # Arguments
    /// * `event` - The event data to send.
    /// * `options` - The options to use when sending the event.
    ///
    /// # Returns
    /// A `Result` indicating success or failure.
    ///
    /// Note:
    /// - If the event being sent does not have a message ID, a new message ID will be generated.
    /// - If the event options contain a partition ID, the event will be sent to the specified partition.
    ///
    pub async fn send_event(
        &self,
        event: impl Into<EventData>,
        options: Option<SendEventOptions>,
    ) -> Result<()> {
        let event = event.into();
        let mut message = AmqpMessage::from(event);

        if message.properties().is_none() || message.properties().unwrap().message_id.is_none() {
            message.set_message_id(Uuid::new_v4());
        }

        self.send_message(message, options.map(SendMessageOptions::from))
            .await
    }

    /// Sends an AMQP message to the Event Hub.
    ///
    /// # Arguments
    /// * `message` - The event to send.
    /// * `options` - The options to use when sending the event.
    ///
    /// # Returns
    /// A `Result` indicating success or failure.
    ///
    /// Note:
    /// - The message is sent to the service unmodified.
    ///
    pub async fn send_message(
        &self,
        message: impl Into<AmqpMessage> + Debug + Send,
        #[allow(unused_variables)] options: Option<SendMessageOptions>,
    ) -> Result<()> {
        let options = options.unwrap_or_default();
        let mut target = self.endpoint.clone();
        if let Some(partition_id) = options.partition_id {
            let target_url = format!("{}/Partitions/{}", self.base_url(), partition_id);
            target = Url::parse(&target_url).map_err(azure_core::Error::from)?;
        }
        let sender = self.ensure_sender(&target).await?;

        let message = message.into();
        let outcome = retry_azure_operation(
            || {
                let sender = sender.clone();
                let message = message.clone();
                async move {
                    let sender_guard = sender.lock().await;
                    sender_guard
                        .send(
                            message.clone(),
                            Some(AmqpSendOptions {
                                message_format: None,
                                ..Default::default()
                            }),
                        )
                        .await
                }
            },
            &self.retry_options,
            Some(Self::should_retry_send_operation),
        )
        .await?;

        // We treat all outcomes other than "rejected" as successful.
        match outcome {
            azure_core_amqp::AmqpSendOutcome::Rejected(error) => Err(azure_core::Error::new(
                azure_core::error::ErrorKind::Other,
                EventHubsError {
                    kind: ErrorKind::SendRejected(error),
                },
            )),
            azure_core_amqp::AmqpSendOutcome::Accepted => Ok(()),
            azure_core_amqp::AmqpSendOutcome::Released => Ok(()),
            azure_core_amqp::AmqpSendOutcome::Modified(_) => Ok(()),
        }
    }

    const BATCH_MESSAGE_FORMAT: u32 = 0x80013700;

    /// Creates a new batch of events to send to the Event Hub.
    /// # Arguments
    ///
    /// * `batch_options` - The options to use when creating the batch.
    ///
    /// # Returns
    ///
    /// A `Result` containing the new `EventDataBatch`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use azure_messaging_eventhubs::ProducerClient;
    /// use azure_identity::{DefaultAzureCredential, TokenCredentialOptions};
    /// use std::error::Error;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    ///   let fully_qualified_namespace = std::env::var("EVENT_HUB_NAMESPACE")?;
    ///   let eventhub_name = std::env::var("EVENT_HUB_NAME")?;
    ///   let my_credentials = DefaultAzureCredential::new()?;
    ///
    ///   let producer = ProducerClient::builder()
    ///    .with_application_id("your_application_id".to_string())
    ///    .open(&fully_qualified_namespace, &eventhub_name, my_credentials.clone()).await?;
    ///   let mut batch = producer.create_batch(None).await?;
    ///   Ok(())
    /// }
    /// ```
    ///
    pub async fn create_batch(
        &self,
        batch_options: Option<EventDataBatchOptions>,
    ) -> Result<EventDataBatch> {
        let mut batch = EventDataBatch::new(self, batch_options);

        batch.attach().await?;
        Ok(batch)
    }

    /// Submits a batch of events to the Event Hub.
    ///
    /// # Arguments
    ///
    /// * `batch` - The batch of events to submit.
    /// * `options` - The options to use when submitting the batch.
    ///
    /// # Returns
    ///
    /// A `Result` indicating success or failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use azure_messaging_eventhubs::ProducerClient;
    /// use azure_identity::{DefaultAzureCredential, TokenCredentialOptions};
    /// use std::error::Error;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    ///   let fully_qualified_namespace = std::env::var("EVENT_HUB_NAMESPACE")?;
    ///   let eventhub_name = std::env::var("EVENT_HUB_NAME")?;
    ///   let my_credentials = DefaultAzureCredential::new()?;
    ///
    ///   let producer = ProducerClient::builder()
    ///    .with_application_id("your_application_id".to_string())
    ///    .open(&fully_qualified_namespace, &eventhub_name, my_credentials.clone()).await?;
    ///
    ///   let mut batch = producer.create_batch(None).await?;
    ///   batch.try_add_event_data("Hello, World!", None)?;
    ///   producer.send_batch(&batch, None).await?;
    ///   Ok(())
    /// }
    /// ```
    ///
    pub async fn send_batch(
        &self,
        batch: &EventDataBatch<'_>,
        #[allow(unused_variables)] options: Option<SendBatchOptions>,
    ) -> Result<()> {
        let sender = self.ensure_sender(&batch.get_batch_path()?).await?;

        let outcome = retry_azure_operation(
            || {
                let sender = sender.clone();
                async move {
                    let messages = batch.get_messages();
                    let sender = sender.lock().await;

                    sender
                        .send(
                            messages,
                            Some(AmqpSendOptions {
                                message_format: Some(Self::BATCH_MESSAGE_FORMAT),
                                ..Default::default()
                            }),
                        )
                        .await
                }
            },
            &self.retry_options,
            Some(Self::should_retry_send_operation),
        )
        .await?;

        // We treat all outcomes other than "rejected" as successful.
        match outcome {
            azure_core_amqp::AmqpSendOutcome::Rejected(error) => Err(azure_core::Error::new(
                azure_core::error::ErrorKind::Other,
                EventHubsError {
                    kind: ErrorKind::SendRejected(error),
                },
            )),
            azure_core_amqp::AmqpSendOutcome::Accepted => Ok(()),
            azure_core_amqp::AmqpSendOutcome::Released => Ok(()),
            azure_core_amqp::AmqpSendOutcome::Modified(_) => Ok(()),
        }
    }

    /// Gets the properties of the Event Hub.
    /// # Returns
    /// A `Result` containing the properties of the Event Hub.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use azure_messaging_eventhubs::ProducerClient;
    /// use azure_identity::{DefaultAzureCredential, TokenCredentialOptions};
    /// use std::error::Error;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    ///   let fully_qualified_namespace = std::env::var("EVENT_HUB_NAMESPACE")?;
    ///   let eventhub_name = std::env::var("EVENT_HUB_NAME")?;
    ///   let my_credentials = DefaultAzureCredential::new()?;
    ///   let producer = ProducerClient::builder()
    ///     .open(&fully_qualified_namespace, &eventhub_name, my_credentials.clone()).await?;
    ///
    ///   let properties = producer.get_eventhub_properties().await?;
    ///   println!("Event Hub: {:?}", properties);
    ///   Ok(())
    /// }
    /// ```
    pub async fn get_eventhub_properties(&self) -> Result<EventHubProperties> {
        self.ensure_management_client().await?;

        self.mgmt_client
            .lock()
            .await
            .get()
            .ok_or_else(|| EventHubsError::from(ErrorKind::MissingManagementClient))?
            .get_eventhub_properties(&self.eventhub)
            .await
    }

    /// Gets the properties of a partition of the Event Hub.
    /// # Arguments
    /// * `partition_id` - The id of the partition.
    /// # Returns
    /// A `Result` containing the properties of the partition.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use azure_messaging_eventhubs::ProducerClient;
    /// use azure_identity::{DefaultAzureCredential, TokenCredentialOptions};
    /// use std::error::Error;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    ///  let fully_qualified_namespace = std::env::var("EVENT_HUB_NAMESPACE")?;
    ///     let eventhub_name = std::env::var("EVENT_HUB_NAME")?;
    ///     let eventhub_name = std::env::var("EVENT_HUB_NAME")?;
    ///     let my_credentials = DefaultAzureCredential::new()?;
    ///     let producer = ProducerClient::builder()
    ///        .open(&fully_qualified_namespace, &eventhub_name, my_credentials.clone()).await?;
    ///     let partition_properties = producer.get_partition_properties("0").await?;
    ///     println!("Event Hub: {:?}", partition_properties);
    ///     Ok(())
    /// }
    /// ```
    pub async fn get_partition_properties(
        &self,
        partition_id: &str,
    ) -> Result<EventHubPartitionProperties> {
        self.ensure_management_client().await?;

        self.mgmt_client
            .lock()
            .await
            .get()
            .ok_or_else(|| EventHubsError::from(ErrorKind::MissingManagementClient))?
            .get_eventhub_partition_properties(&self.eventhub, partition_id)
            .await
    }

    pub(crate) fn base_url(&self) -> &Url {
        &self.endpoint
    }

    async fn ensure_connection(&self) -> Result<()> {
        self.connection_manager.ensure_connection().await?;
        Ok(())
    }
    async fn ensure_management_client(&self) -> Result<()> {
        let mgmt_client = self.mgmt_client.lock().await;

        if mgmt_client.get().is_some() {
            return Ok(());
        }

        // Clients must call ensure_connection before calling ensure_management_client.

        trace!("Create management session.");
        let connection = self.connection_manager.get_connection()?;

        let session = AmqpSession::new();
        session.begin(connection.as_ref(), None).await?;
        trace!("Session created.");

        let management_path = self.endpoint.to_string() + "/$management";
        let management_path = Url::parse(&management_path)?;
        let access_token = self
            .connection_manager
            .authorize_path(&connection, &management_path)
            .await?;

        trace!("Create management client.");
        let management =
            AmqpManagement::new(session, "eventhubs_management".to_string(), access_token)?;
        management.attach().await?;
        mgmt_client
            .set(ManagementInstance::new(
                management,
                self.retry_options.clone(),
            ))
            .map_err(|_| EventHubsError::from(ErrorKind::MissingManagementClient))?;
        trace!("Management client created.");
        Ok(())
    }

    async fn ensure_sender(&self, path: &Url) -> Result<Arc<Mutex<AmqpSender>>> {
        let mut sender_instances = self.sender_instances.lock().await;
        if !sender_instances.contains_key(path) {
            self.connection_manager.ensure_connection().await?;

            let connection = self.connection_manager.get_connection()?;

            self.connection_manager
                .authorize_path(&connection, path)
                .await?;
            let session = AmqpSession::new();
            session
                .begin(
                    connection.as_ref(),
                    Some(AmqpSessionOptions {
                        incoming_window: Some(u32::MAX),
                        outgoing_window: Some(u32::MAX),
                        ..Default::default()
                    }),
                )
                .await?;
            let sender = AmqpSender::new();
            sender
                .attach(
                    &session,
                    format!(
                        "{}-rust-sender",
                        self.application_id
                            .as_ref()
                            .unwrap_or(&DEFAULT_EVENTHUBS_APPLICATION.to_string())
                    ),
                    path.to_string(),
                    None,
                )
                .await?;
            sender_instances.insert(
                path.clone(),
                SenderInstance {
                    session,
                    sender: Arc::new(Mutex::new(sender)),
                },
            );
        }
        Ok(sender_instances
            .get(path)
            .ok_or_else(|| EventHubsError::from(ErrorKind::MissingMessageSender))?
            .sender
            .clone())
    }
}

pub mod builders {
    use super::ProducerClient;
    use crate::RetryOptions;
    use azure_core::{http::Url, Error};
    use std::sync::Arc;

    /// A builder for creating a [`ProducerClient`].
    ///
    /// This builder is used to create a new [`ProducerClient`] with the specified parameters.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use azure_messaging_eventhubs::ProducerClient;
    /// use azure_identity::{DefaultAzureCredential, TokenCredentialOptions};
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///   let my_credential = DefaultAzureCredential::new().unwrap();
    ///   let producer = ProducerClient::builder()
    ///      .open("my_namespace", "my_eventhub", my_credential).await.unwrap();
    /// }
    /// ```
    #[derive(Default)]
    pub struct ProducerClientBuilder {
        /// The application id that will be used to identify the client.
        application_id: Option<String>,

        /// The options used to configure retry operations.
        retry_options: Option<RetryOptions>,

        /// The custom endpoint for the Event Hub.
        custom_endpoint: Option<String>,
    }

    impl ProducerClientBuilder {
        ///
        /// # Arguments
        ///
        /// * `fully_qualified_namespace` - The fully qualified namespace of the Event Hubs instance.
        /// * `eventhub` - The name of the Event Hub.
        /// * `credential` - The token credential used for authorization.
        ///
        /// # Returns
        ///
        /// A new instance of [`ProducerClientBuilder`].
        pub(super) fn new() -> Self {
            Self {
                ..Default::default()
            }
        }

        /// Sets the application id that will be used to identify the client.
        pub fn with_application_id(mut self, application_id: String) -> Self {
            self.application_id = Some(application_id);
            self
        }

        /// Sets the options used to configure retry operations.
        ///
        /// # Arguments
        ///
        /// * `retry_options` - The options used to configure retry operations.
        ///
        /// # Returns
        ///
        /// The updated [`ProducerClientBuilder`].
        pub fn with_retry_options(mut self, retry_options: RetryOptions) -> Self {
            self.retry_options = Some(retry_options);
            self
        }

        /// Sets a custom endpoint for the Event Hub.
        ///
        /// # Arguments
        /// * `endpoint` - The custom endpoint for the Event Hub.
        ///
        /// # Returns
        /// The updated [`ProducerClientBuilder`].
        ///
        /// Note: The custom endpoint option allows a customer to specify an AMQP proxy
        /// which will be used to forward requests to the actual Event Hub instance.
        ///
        pub fn with_custom_endpoint(mut self, endpoint: String) -> Self {
            self.custom_endpoint = Some(endpoint);
            self
        }

        /// Opens the connection to the Event Hub.
        ///
        /// # Arguments
        /// * `fully_qualified_namespace` - The fully qualified namespace of the Event Hubs instance.
        /// * `eventhub` - The name of the Event Hub.
        /// * `credential` - The token credential to be used for authorization.
        ///
        /// # Returns
        /// A new instance of [`ProducerClient`].
        ///
        pub async fn open(
            self,
            fully_qualified_namespace: &str,
            eventhub: &str,
            credential: Arc<dyn azure_core::credentials::TokenCredential>,
        ) -> azure_core::Result<ProducerClient> {
            let url = format!("amqps://{}/{}", fully_qualified_namespace, eventhub);
            let url = Url::parse(&url)?;

            let custom_endpoint = match self.custom_endpoint {
                Some(endpoint) => Some(Url::parse(&endpoint).map_err(Error::from)?),
                None => None,
            };

            let client = ProducerClient::new(
                url.clone(),
                eventhub.to_string(),
                credential,
                self.application_id,
                self.retry_options.unwrap_or_default(),
                custom_endpoint,
            );

            client.ensure_connection().await?;
            Ok(client)
        }
    }
}
#[cfg(test)]
mod tests {}
