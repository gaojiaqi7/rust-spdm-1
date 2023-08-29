// Copyright (c) 2020 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

use super::app_message_handler::dispatch_secured_app_message_cb;
use crate::common::{session::SpdmSessionState, SpdmDeviceIo, SpdmTransportEncap};
use crate::common::{SpdmConnectionState, ST1};
use crate::config;
use crate::error::{SpdmResult, SPDM_STATUS_UNSUPPORTED_CAP};
use crate::message::*;
use crate::protocol::{SpdmRequestCapabilityFlags, SpdmResponseCapabilityFlags};
use codec::{Codec, Reader, Writer};
extern crate alloc;
use core::ops::DerefMut;

use alloc::sync::Arc;
use spin::Mutex;

pub struct ResponderContext {
    pub common: crate::common::SpdmContext,
}

impl ResponderContext {
    pub fn new(
        device_io: Arc<Mutex<dyn SpdmDeviceIo + Send + Sync>>,
        transport_encap: Arc<Mutex<dyn SpdmTransportEncap + Send + Sync>>,
        config_info: crate::common::SpdmConfigInfo,
        provision_info: crate::common::SpdmProvisionInfo,
    ) -> Self {
        ResponderContext {
            common: crate::common::SpdmContext::new(
                device_io,
                transport_encap,
                config_info,
                provision_info,
            ),
        }
    }

    pub async fn send_message(
        &mut self,
        session_id: Option<u32>,
        send_buffer: &[u8],
        is_app_message: bool,
    ) -> SpdmResult {
        let mut err_buffer = [0u8; config::MAX_SPDM_MSG_SIZE];
        let mut writer = Writer::init(&mut err_buffer);

        let send_buffer = if self.common.negotiate_info.req_data_transfer_size_sel != 0
            && (send_buffer.len() > self.common.negotiate_info.req_data_transfer_size_sel as usize)
        {
            self.write_spdm_error(SpdmErrorCode::SpdmErrorResponseTooLarge, 0, &mut writer);
            writer.used_slice()
        } else if is_app_message && session_id.is_none() {
            self.write_spdm_error(SpdmErrorCode::SpdmErrorSessionRequired, 0, &mut writer);
            writer.used_slice()
        } else {
            send_buffer
        };

        let mut transport_buffer = [0u8; config::SENDER_BUFFER_SIZE];
        let used = if let Some(session_id) = session_id {
            self.common
                .encode_secured_message(
                    session_id,
                    send_buffer,
                    &mut transport_buffer,
                    false,
                    is_app_message,
                )
                .await?
        } else {
            self.common
                .encap(send_buffer, &mut transport_buffer)
                .await?
        };

        {
            let mut device_io = self.common.device_io.lock();
            let device_io: &mut (dyn SpdmDeviceIo + Send + Sync) = device_io.deref_mut();
            device_io.send(Arc::new(&transport_buffer[..used])).await?;
        }

        let opcode = send_buffer[1];
        if opcode == SpdmRequestResponseCode::SpdmResponseVersion.get_u8() {
            self.common
                .runtime_info
                .set_connection_state(SpdmConnectionState::SpdmConnectionAfterVersion);
        } else if opcode == SpdmRequestResponseCode::SpdmResponseCapabilities.get_u8() {
            self.common
                .runtime_info
                .set_connection_state(SpdmConnectionState::SpdmConnectionAfterCapabilities);
        } else if opcode == SpdmRequestResponseCode::SpdmResponseAlgorithms.get_u8() {
            self.common
                .runtime_info
                .set_connection_state(SpdmConnectionState::SpdmConnectionNegotiated);
        } else if opcode == SpdmRequestResponseCode::SpdmResponseDigests.get_u8() {
            if self.common.runtime_info.get_connection_state().get_u8()
                < SpdmConnectionState::SpdmConnectionAfterDigest.get_u8()
            {
                self.common
                    .runtime_info
                    .set_connection_state(SpdmConnectionState::SpdmConnectionAfterDigest);
            }
        } else if opcode == SpdmRequestResponseCode::SpdmResponseCertificate.get_u8() {
            if self.common.runtime_info.get_connection_state().get_u8()
                < SpdmConnectionState::SpdmConnectionAfterCertificate.get_u8()
            {
                self.common
                    .runtime_info
                    .set_connection_state(SpdmConnectionState::SpdmConnectionAfterCertificate);
            }
        } else if opcode == SpdmRequestResponseCode::SpdmResponseChallengeAuth.get_u8() {
            self.common
                .runtime_info
                .set_connection_state(SpdmConnectionState::SpdmConnectionAuthenticated);
        } else if opcode == SpdmRequestResponseCode::SpdmResponseFinishRsp.get_u8()
            && session_id.is_none()
        {
            let session = self
                .common
                .get_session_via_id(self.common.runtime_info.get_last_session_id().unwrap())
                .unwrap();
            session.set_session_state(
                crate::common::session::SpdmSessionState::SpdmSessionEstablished,
            );
            self.common.runtime_info.set_last_session_id(None);
        } else if opcode == SpdmRequestResponseCode::SpdmResponseEndSessionAck.get_u8() {
            let session = self.common.get_session_via_id(session_id.unwrap()).unwrap();
            let _ = session.teardown(session_id.unwrap());
        } else if (opcode == SpdmRequestResponseCode::SpdmResponseFinishRsp.get_u8()
            || opcode == SpdmRequestResponseCode::SpdmResponsePskFinishRsp.get_u8())
            && session_id.is_some()
        {
            #[allow(clippy::unnecessary_unwrap)]
            let session = self.common.get_session_via_id(session_id.unwrap()).unwrap();
            session.set_session_state(
                crate::common::session::SpdmSessionState::SpdmSessionEstablished,
            );
        }

        Ok(())
    }

    pub async fn process_message(
        &mut self,
        crypto_request: bool,
        auxiliary_app_data: &[u8],
    ) -> Result<bool, (usize, [u8; config::RECEIVER_BUFFER_SIZE])> {
        let mut receive_buffer = [0u8; config::RECEIVER_BUFFER_SIZE];
        match self
            .receive_message(&mut receive_buffer[..], crypto_request)
            .await
        {
            Ok((used, secured_message)) => {
                if secured_message {
                    let mut read = Reader::init(&receive_buffer[0..used]);
                    let session_id = u32::read(&mut read).ok_or((used, receive_buffer))?;

                    let spdm_session = self
                        .common
                        .get_session_via_id(session_id)
                        .ok_or((used, receive_buffer))?;

                    let mut app_buffer = [0u8; config::RECEIVER_BUFFER_SIZE];

                    let decode_size = spdm_session.decode_spdm_secured_message(
                        &receive_buffer[..used],
                        &mut app_buffer,
                        true,
                    );
                    if decode_size.is_err() {
                        return Err((used, receive_buffer));
                    }
                    let decode_size = decode_size.unwrap();

                    let mut spdm_buffer = [0u8; config::MAX_SPDM_MSG_SIZE];
                    let decap_result = {
                        let mut transport_encap = self.common.transport_encap.lock();
                        let transport_encap: &mut (dyn SpdmTransportEncap + Send + Sync) =
                            transport_encap.deref_mut();
                        transport_encap
                            .decap_app(
                                Arc::new(&app_buffer[0..decode_size]),
                                Arc::new(Mutex::new(&mut spdm_buffer)),
                            )
                            .await
                    };
                    match decap_result {
                        Err(_) => Err((used, receive_buffer)),
                        Ok((decode_size, is_app_message)) => {
                            if !is_app_message {
                                Ok(self
                                    .dispatch_secured_message(
                                        session_id,
                                        &spdm_buffer[0..decode_size],
                                    )
                                    .await
                                    .is_ok())
                            } else {
                                Ok(self
                                    .dispatch_secured_app_message(
                                        session_id,
                                        &spdm_buffer[..decode_size],
                                        auxiliary_app_data,
                                    )
                                    .await
                                    .is_ok())
                            }
                        }
                    }
                } else {
                    Ok(self
                        .dispatch_message(&receive_buffer[0..used])
                        .await
                        .is_ok())
                }
            }
            Err(used) => Err((used, receive_buffer)),
        }
    }

    // Debug note: receive_buffer is used as return value, when receive got a command
    // whose value is not normal, will return Err to caller to handle the raw packet,
    // So can't swap transport_buffer and receive_buffer, even though it should be by
    // their name suggestion. (03.01.2022)
    async fn receive_message(
        &mut self,
        receive_buffer: &mut [u8],
        crypto_request: bool,
    ) -> Result<(usize, bool), usize> {
        info!("receive_message!\n");

        let timeout: usize = if crypto_request {
            2 << self.common.negotiate_info.req_ct_exponent_sel
        } else {
            ST1
        };

        let mut transport_buffer = [0u8; config::RECEIVER_BUFFER_SIZE];

        let used = {
            let mut device_io = self.common.device_io.lock();
            let device_io: &mut (dyn SpdmDeviceIo + Send + Sync) = device_io.deref_mut();
            device_io
                .receive(Arc::new(Mutex::new(receive_buffer)), timeout)
                .await?
        };

        let (used, secured_message) = {
            let mut transport_encap = self.common.transport_encap.lock();
            let transport_encap: &mut (dyn SpdmTransportEncap + Send + Sync) =
                transport_encap.deref_mut();
            transport_encap
                .decap(
                    Arc::new(&receive_buffer[..used]),
                    Arc::new(Mutex::new(&mut transport_buffer)),
                )
                .await
                .map_err(|_| used)?
        };

        receive_buffer[..used].copy_from_slice(&transport_buffer[..used]);
        Ok((used, secured_message))
    }

    async fn dispatch_secured_message(&mut self, session_id: u32, bytes: &[u8]) -> SpdmResult {
        let mut reader = Reader::init(bytes);

        let session = self.common.get_immutable_session_via_id(session_id);
        if session.is_none() {
            return Err(SPDM_STATUS_UNSUPPORTED_CAP);
        }
        let session = session.unwrap();

        match session.get_session_state() {
            SpdmSessionState::SpdmSessionHandshaking => {
                let in_clear_text = self
                    .common
                    .negotiate_info
                    .req_capabilities_sel
                    .contains(SpdmRequestCapabilityFlags::HANDSHAKE_IN_THE_CLEAR_CAP)
                    && self
                        .common
                        .negotiate_info
                        .rsp_capabilities_sel
                        .contains(SpdmResponseCapabilityFlags::HANDSHAKE_IN_THE_CLEAR_CAP);
                if in_clear_text {
                    return Err(SPDM_STATUS_UNSUPPORTED_CAP);
                }

                match SpdmMessageHeader::read(&mut reader) {
                    Some(message_header) => match message_header.request_response_code {
                        #[cfg(feature = "mut-auth")]
                        SpdmRequestResponseCode::SpdmRequestGetEncapsulatedRequest => {
                            self.handle_get_encapsulated_request(session_id, bytes)
                                .await
                        }
                        #[cfg(feature = "mut-auth")]
                        SpdmRequestResponseCode::SpdmRequestDeliverEncapsulatedResponse => {
                            self.handle_deliver_encapsulated_reponse(session_id, bytes)
                                .await
                        }
                        SpdmRequestResponseCode::SpdmRequestFinish => {
                            self.handle_spdm_finish(session_id, bytes).await
                        }

                        SpdmRequestResponseCode::SpdmRequestPskFinish => {
                            self.handle_spdm_psk_finish(session_id, bytes).await
                        }

                        SpdmRequestResponseCode::SpdmRequestVendorDefinedRequest => {
                            self.handle_spdm_vendor_defined_request(Some(session_id), bytes)
                                .await
                        }

                        SpdmRequestResponseCode::SpdmRequestGetVersion
                        | SpdmRequestResponseCode::SpdmRequestGetCapabilities
                        | SpdmRequestResponseCode::SpdmRequestNegotiateAlgorithms
                        | SpdmRequestResponseCode::SpdmRequestGetDigests
                        | SpdmRequestResponseCode::SpdmRequestGetCertificate
                        | SpdmRequestResponseCode::SpdmRequestChallenge
                        | SpdmRequestResponseCode::SpdmRequestGetMeasurements
                        | SpdmRequestResponseCode::SpdmRequestKeyExchange
                        | SpdmRequestResponseCode::SpdmRequestPskExchange
                        | SpdmRequestResponseCode::SpdmRequestHeartbeat
                        | SpdmRequestResponseCode::SpdmRequestKeyUpdate
                        | SpdmRequestResponseCode::SpdmRequestEndSession => {
                            self.handle_error_request(
                                SpdmErrorCode::SpdmErrorUnexpectedRequest,
                                Some(session_id),
                                bytes,
                            )
                            .await
                        }

                        SpdmRequestResponseCode::SpdmRequestResponseIfReady => {
                            self.handle_error_request(
                                SpdmErrorCode::SpdmErrorUnsupportedRequest,
                                Some(session_id),
                                bytes,
                            )
                            .await
                        }

                        _ => Err(SPDM_STATUS_UNSUPPORTED_CAP),
                    },
                    None => Err(SPDM_STATUS_UNSUPPORTED_CAP),
                }
            }
            SpdmSessionState::SpdmSessionEstablished => {
                match SpdmMessageHeader::read(&mut reader) {
                    Some(message_header) => match message_header.request_response_code {
                        SpdmRequestResponseCode::SpdmRequestGetDigests => {
                            self.handle_spdm_digest(bytes, Some(session_id)).await
                        }
                        SpdmRequestResponseCode::SpdmRequestGetCertificate => {
                            self.handle_spdm_certificate(bytes, Some(session_id)).await
                        }
                        SpdmRequestResponseCode::SpdmRequestGetMeasurements => {
                            self.handle_spdm_measurement(Some(session_id), bytes).await
                        }

                        SpdmRequestResponseCode::SpdmRequestHeartbeat => {
                            self.handle_spdm_heartbeat(session_id, bytes).await
                        }

                        SpdmRequestResponseCode::SpdmRequestKeyUpdate => {
                            self.handle_spdm_key_update(session_id, bytes).await
                        }

                        SpdmRequestResponseCode::SpdmRequestEndSession => {
                            self.handle_spdm_end_session(session_id, bytes).await
                        }
                        SpdmRequestResponseCode::SpdmRequestVendorDefinedRequest => {
                            self.handle_spdm_vendor_defined_request(Some(session_id), bytes)
                                .await
                        }

                        SpdmRequestResponseCode::SpdmRequestGetVersion
                        | SpdmRequestResponseCode::SpdmRequestGetCapabilities
                        | SpdmRequestResponseCode::SpdmRequestNegotiateAlgorithms
                        | SpdmRequestResponseCode::SpdmRequestChallenge
                        | SpdmRequestResponseCode::SpdmRequestKeyExchange
                        | SpdmRequestResponseCode::SpdmRequestPskExchange
                        | SpdmRequestResponseCode::SpdmRequestFinish
                        | SpdmRequestResponseCode::SpdmRequestPskFinish => {
                            self.handle_error_request(
                                SpdmErrorCode::SpdmErrorUnexpectedRequest,
                                Some(session_id),
                                bytes,
                            )
                            .await
                        }

                        SpdmRequestResponseCode::SpdmRequestResponseIfReady => {
                            self.handle_error_request(
                                SpdmErrorCode::SpdmErrorUnsupportedRequest,
                                Some(session_id),
                                bytes,
                            )
                            .await
                        }

                        _ => Err(SPDM_STATUS_UNSUPPORTED_CAP),
                    },
                    None => Err(SPDM_STATUS_UNSUPPORTED_CAP),
                }
            }
            SpdmSessionState::SpdmSessionNotStarted => Err(SPDM_STATUS_UNSUPPORTED_CAP),
            SpdmSessionState::Unknown(_) => Err(SPDM_STATUS_UNSUPPORTED_CAP),
        }
    }

    async fn dispatch_secured_app_message(
        &mut self,
        session_id: u32,
        bytes: &[u8],
        auxiliary_app_data: &[u8],
    ) -> SpdmResult {
        debug!("dispatching secured app message\n");

        let (rsp_app_buffer, size) =
            dispatch_secured_app_message_cb(self, session_id, bytes, auxiliary_app_data).unwrap();
        self.send_message(Some(session_id), &rsp_app_buffer[..size], true)
            .await
    }

    pub async fn dispatch_message(&mut self, bytes: &[u8]) -> SpdmResult {
        let mut reader = Reader::init(bytes);
        match SpdmMessageHeader::read(&mut reader) {
            Some(message_header) => match message_header.request_response_code {
                SpdmRequestResponseCode::SpdmRequestGetVersion => {
                    self.handle_spdm_version(bytes).await
                }
                SpdmRequestResponseCode::SpdmRequestGetCapabilities => {
                    self.handle_spdm_capability(bytes).await
                }
                SpdmRequestResponseCode::SpdmRequestNegotiateAlgorithms => {
                    self.handle_spdm_algorithm(bytes).await
                }
                SpdmRequestResponseCode::SpdmRequestGetDigests => {
                    self.handle_spdm_digest(bytes, None).await
                }
                SpdmRequestResponseCode::SpdmRequestGetCertificate => {
                    self.handle_spdm_certificate(bytes, None).await
                }
                SpdmRequestResponseCode::SpdmRequestChallenge => {
                    self.handle_spdm_challenge(bytes).await
                }
                SpdmRequestResponseCode::SpdmRequestGetMeasurements => {
                    self.handle_spdm_measurement(None, bytes).await
                }

                SpdmRequestResponseCode::SpdmRequestKeyExchange => {
                    self.handle_spdm_key_exchange(bytes).await
                }

                SpdmRequestResponseCode::SpdmRequestPskExchange => {
                    self.handle_spdm_psk_exchange(bytes).await
                }

                SpdmRequestResponseCode::SpdmRequestVendorDefinedRequest => {
                    self.handle_spdm_vendor_defined_request(None, bytes).await
                }

                SpdmRequestResponseCode::SpdmRequestFinish => {
                    let in_clear_text = self
                        .common
                        .negotiate_info
                        .req_capabilities_sel
                        .contains(SpdmRequestCapabilityFlags::HANDSHAKE_IN_THE_CLEAR_CAP)
                        && self
                            .common
                            .negotiate_info
                            .rsp_capabilities_sel
                            .contains(SpdmResponseCapabilityFlags::HANDSHAKE_IN_THE_CLEAR_CAP);
                    if in_clear_text {
                        if let Some(session_id) = self.common.runtime_info.get_last_session_id() {
                            if let Some(session) =
                                self.common.get_immutable_session_via_id(session_id)
                            {
                                if session.get_session_state()
                                    == SpdmSessionState::SpdmSessionHandshaking
                                {
                                    return self.handle_spdm_finish(session_id, bytes).await;
                                }
                            }
                        }
                    }

                    self.handle_error_request(
                        SpdmErrorCode::SpdmErrorUnexpectedRequest,
                        None,
                        bytes,
                    )
                    .await
                }

                SpdmRequestResponseCode::SpdmRequestPskFinish
                | SpdmRequestResponseCode::SpdmRequestHeartbeat
                | SpdmRequestResponseCode::SpdmRequestKeyUpdate
                | SpdmRequestResponseCode::SpdmRequestEndSession => {
                    self.handle_error_request(
                        SpdmErrorCode::SpdmErrorUnexpectedRequest,
                        None,
                        bytes,
                    )
                    .await
                }

                SpdmRequestResponseCode::SpdmRequestResponseIfReady => {
                    self.handle_error_request(
                        SpdmErrorCode::SpdmErrorUnsupportedRequest,
                        None,
                        bytes,
                    )
                    .await
                }

                _ => Err(SPDM_STATUS_UNSUPPORTED_CAP),
            },
            None => Err(SPDM_STATUS_UNSUPPORTED_CAP),
        }
    }
}
