// Copyright (c) Microsoft Corporation. All Rights reserved
// Licensed under the MIT license.

use crate::{
    error::{AmqpDescribedError, AmqpError, AmqpErrorKind},
    messaging::{AmqpMessage, AmqpTarget},
    sender::{
        AmqpSendOptions, AmqpSendOutcome, AmqpSenderApis, AmqpSenderOptions, SendModification,
    },
    session::AmqpSession,
    AmqpOrderedMap, AmqpSymbol, AmqpValue,
};
use async_trait::async_trait;
use azure_core::Result;
use std::borrow::BorrowMut;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tracing::{info, warn};

#[derive(Default)]
pub(crate) struct Fe2o3AmqpSender {
    sender: OnceLock<Mutex<fe2o3_amqp::Sender>>,
}

impl Fe2o3AmqpSender {
    fn could_not_set_message_sender() -> azure_core::Error {
        azure_core::Error::message(
            azure_core::error::ErrorKind::Amqp,
            "Could not set message sender",
        )
    }
    fn could_not_get_message_sender() -> azure_core::Error {
        azure_core::Error::message(
            azure_core::error::ErrorKind::Amqp,
            "Could not get message sender",
        )
    }
}

#[async_trait]
impl AmqpSenderApis for Fe2o3AmqpSender {
    async fn attach(
        &self,
        session: &AmqpSession,
        name: String,
        target: impl Into<AmqpTarget> + Send,
        options: Option<AmqpSenderOptions>,
    ) -> Result<()> {
        let mut session_builder = fe2o3_amqp::Sender::builder();

        if let Some(options) = options {
            // if let Some(link_credit) = options.link_credit {
            //     session_builder = session_builder.link_credit(link_credit);
            // }
            if let Some(sender_settle_mode) = options.sender_settle_mode {
                session_builder = session_builder.sender_settle_mode(sender_settle_mode.into());
            }
            if let Some(receiver_settle_mode) = options.receiver_settle_mode {
                session_builder = session_builder.receiver_settle_mode(receiver_settle_mode.into());
            }
            if let Some(max_message_size) = options.max_message_size {
                session_builder = session_builder.max_message_size(max_message_size);
            }

            if let Some(source) = options.source {
                session_builder = session_builder.source(source);
            }
            if let Some(offered_capabilities) = options.offered_capabilities {
                let capabilities = offered_capabilities.into_iter().map(|c| c.into()).collect();
                session_builder = session_builder.set_offered_capabilities(capabilities);
            }
            if let Some(desired_capabilities) = options.desired_capabilities {
                let capabilities = desired_capabilities.into_iter().map(|c| c.into()).collect();
                session_builder = session_builder.set_desired_capabilities(capabilities);
            }
            if let Some(properties) = options.properties {
                session_builder = session_builder.properties(properties.into());
            }
            if let Some(initial_delivery_count) = options.initial_delivery_count {
                session_builder = session_builder.initial_delivery_count(initial_delivery_count);
            }
        }
        let sender = session_builder
            .name(name)
            .target(target.into())
            .attach(session.implementation.get()?.lock().await.borrow_mut())
            .await
            .map_err(|e| azure_core::Error::from(AmqpError::from(e)))?;
        self.sender
            .set(Mutex::new(sender))
            .map_err(|_| Self::could_not_set_message_sender())?;
        Ok(())
    }

    async fn detach(mut self) -> Result<()> {
        let sender = self
            .sender
            .take()
            .ok_or_else(Self::could_not_get_message_sender)?;
        let res = sender
            .into_inner()
            .detach()
            .await
            .map_err(|e| AmqpError::from(e.1));
        match res {
            Ok(_) => Ok(()),
            Err(e) => match e.kind() {
                AmqpErrorKind::ClosedByRemote(_) => {
                    info!("Error detaching sender: {:?}", e);
                    Ok(())
                }
                _ => {
                    warn!("Error detaching sender: {:?}", e);
                    Err(e.into())
                }
            },
        }
    }

    async fn max_message_size(&self) -> azure_core::Result<Option<u64>> {
        Ok(self
            .sender
            .get()
            .ok_or_else(Self::could_not_get_message_sender)?
            .lock()
            .await
            .max_message_size())
    }

    async fn send(
        &self,
        message: impl Into<AmqpMessage> + std::fmt::Debug + Send,
        options: Option<AmqpSendOptions>,
    ) -> Result<AmqpSendOutcome> {
        let message: AmqpMessage = message.into();
        let message: fe2o3_amqp_types::messaging::Message<
            fe2o3_amqp_types::messaging::Body<fe2o3_amqp_types::primitives::Value>,
        > = message.into();
        let mut sendable = fe2o3_amqp::link::delivery::Sendable {
            message,
            message_format: 0,
            settled: Default::default(),
        };
        if let Some(options) = options {
            if let Some(message_format) = options.message_format {
                sendable.message_format = message_format;
            }
            sendable.settled = options.settled;
        }

        let outcome = self
            .sender
            .get()
            .ok_or_else(Self::could_not_get_message_sender)?
            .lock()
            .await
            .borrow_mut()
            .send(sendable)
            .await
            .map_err(AmqpError::from)?;

        Ok(match outcome {
            fe2o3_amqp_types::messaging::Outcome::Accepted(_) => AmqpSendOutcome::Accepted,
            fe2o3_amqp_types::messaging::Outcome::Rejected(rejected) => {
                AmqpSendOutcome::Rejected(rejected.error.map(AmqpDescribedError::from))
            }
            fe2o3_amqp_types::messaging::Outcome::Released(_) => AmqpSendOutcome::Released,
            fe2o3_amqp_types::messaging::Outcome::Modified(m) => {
                AmqpSendOutcome::Modified(m.into())
            }
        })
    }
}

impl From<fe2o3_amqp_types::messaging::Modified> for SendModification {
    fn from(m: fe2o3_amqp_types::messaging::Modified) -> Self {
        Self {
            delivery_failed: m.delivery_failed,
            undeliverable_here: m.undeliverable_here,
            message_annotations: m
                .message_annotations
                .map(AmqpOrderedMap::<AmqpSymbol, AmqpValue>::from),
        }
    }
}

impl Fe2o3AmqpSender {
    pub fn new() -> Self {
        Self {
            sender: OnceLock::new(),
        }
    }
}

impl From<fe2o3_amqp::link::SendError> for AmqpError {
    fn from(e: fe2o3_amqp::link::SendError) -> Self {
        match e {
            fe2o3_amqp::link::SendError::LinkStateError(link_state_error) => {
                AmqpError::from(link_state_error)
            }
            fe2o3_amqp::link::SendError::Detached(detach_error) => detach_error.into(),
            fe2o3_amqp::link::SendError::NonTerminalDeliveryState => {
                AmqpErrorKind::NonTerminalDeliveryState.into()
            }
            fe2o3_amqp::link::SendError::IllegalDeliveryState => {
                AmqpErrorKind::IllegalDeliveryState.into()
            }
            fe2o3_amqp::link::SendError::MessageEncodeError => {
                AmqpError::from(AmqpErrorKind::TransportImplementationError(Box::new(e)))
            }
        }
    }
}

impl From<fe2o3_amqp::link::SenderAttachError> for AmqpError {
    fn from(e: fe2o3_amqp::link::SenderAttachError) -> Self {
        match e {
            fe2o3_amqp::link::SenderAttachError::RemoteClosedWithError(e) => {
                AmqpErrorKind::AmqpDescribedError(e.into()).into()
            }
            fe2o3_amqp::link::SenderAttachError::IllegalSessionState
            | fe2o3_amqp::link::SenderAttachError::IllegalState => {
                AmqpErrorKind::ConnectionDropped(Box::new(e)).into()
            }
            fe2o3_amqp::link::SenderAttachError::CoordinatorIsNotImplemented
            | fe2o3_amqp::link::SenderAttachError::DuplicatedLinkName
            | fe2o3_amqp::link::SenderAttachError::NonAttachFrameReceived
            | fe2o3_amqp::link::SenderAttachError::ExpectImmediateDetach
            | fe2o3_amqp::link::SenderAttachError::IncomingTargetIsNone
            | fe2o3_amqp::link::SenderAttachError::SndSettleModeNotSupported
            | fe2o3_amqp::link::SenderAttachError::RcvSettleModeNotSupported
            | fe2o3_amqp::link::SenderAttachError::TargetAddressIsNoneWhenDynamicIsTrue
            | fe2o3_amqp::link::SenderAttachError::SourceAddressIsSomeWhenDynamicIsTrue
            | fe2o3_amqp::link::SenderAttachError::DynamicNodePropertiesIsSomeWhenDynamicIsFalse => {
                AmqpErrorKind::TransportImplementationError(Box::new(e)).into()
            }
        }
    }
}
