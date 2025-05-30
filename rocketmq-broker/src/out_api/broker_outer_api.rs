/*
 * Licensed to the Apache Software Foundation (ASF) under one or more
 * contributor license agreements.  See the NOTICE file distributed with
 * this work for additional information regarding copyright ownership.
 * The ASF licenses this file to You under the Apache License, Version 2.0
 * (the "License"); you may not use this file except in compliance with
 * the License.  You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
use std::collections::HashSet;
use std::sync::Arc;

use cheetah_string::CheetahString;
use dns_lookup::lookup_host;
use rocketmq_client_rust::consumer::pull_result::PullResult;
use rocketmq_client_rust::consumer::pull_status::PullStatus;
use rocketmq_client_rust::producer::send_result::SendResult;
use rocketmq_client_rust::producer::send_status::SendStatus;
use rocketmq_client_rust::PullResultExt;
use rocketmq_common::common::broker::broker_config::BrokerIdentity;
use rocketmq_common::common::config::TopicConfig;
use rocketmq_common::common::filter::expression_type::ExpressionType;
use rocketmq_common::common::message::message_client_id_setter::MessageClientIDSetter;
use rocketmq_common::common::message::message_ext::MessageExt;
use rocketmq_common::common::message::message_queue::MessageQueue;
use rocketmq_common::common::message::MessageConst;
use rocketmq_common::common::message::MessageTrait;
use rocketmq_common::common::mix_all;
use rocketmq_common::common::sys_flag::pull_sys_flag::PullSysFlag;
use rocketmq_common::common::topic::TopicValidator;
use rocketmq_common::utils::crc32_utils;
use rocketmq_common::utils::serde_json_utils::SerdeJsonUtils;
use rocketmq_common::MessageAccessor::MessageAccessor;
use rocketmq_common::MessageDecoder;
use rocketmq_common::TimeUtils::get_current_millis;
use rocketmq_error::RocketmqError;
use rocketmq_remoting::clients::rocketmq_default_impl::RocketmqDefaultClient;
use rocketmq_remoting::clients::RemotingClient;
use rocketmq_remoting::code::request_code::RequestCode;
use rocketmq_remoting::code::response_code::ResponseCode;
use rocketmq_remoting::protocol::body::broker_body::register_broker_body::RegisterBrokerBody;
use rocketmq_remoting::protocol::body::kv_table::KVTable;
use rocketmq_remoting::protocol::body::response::lock_batch_response_body::LockBatchResponseBody;
use rocketmq_remoting::protocol::body::topic_info_wrapper::topic_config_wrapper::TopicConfigAndMappingSerializeWrapper;
use rocketmq_remoting::protocol::header::client_request_header::GetRouteInfoRequestHeader;
use rocketmq_remoting::protocol::header::lock_batch_mq_request_header::LockBatchMqRequestHeader;
use rocketmq_remoting::protocol::header::message_operation_header::send_message_request_header::SendMessageRequestHeader;
use rocketmq_remoting::protocol::header::message_operation_header::send_message_request_header_v2::SendMessageRequestHeaderV2;
use rocketmq_remoting::protocol::header::message_operation_header::send_message_response_header::SendMessageResponseHeader;
use rocketmq_remoting::protocol::header::namesrv::broker_request::UnRegisterBrokerRequestHeader;
use rocketmq_remoting::protocol::header::namesrv::register_broker_header::RegisterBrokerRequestHeader;
use rocketmq_remoting::protocol::header::namesrv::register_broker_header::RegisterBrokerResponseHeader;
use rocketmq_remoting::protocol::header::namesrv::topic_operation_header::RegisterTopicRequestHeader;
use rocketmq_remoting::protocol::header::namesrv::topic_operation_header::TopicRequestHeader;
use rocketmq_remoting::protocol::header::pull_message_request_header::PullMessageRequestHeader;
use rocketmq_remoting::protocol::header::pull_message_response_header::PullMessageResponseHeader;
use rocketmq_remoting::protocol::header::unlock_batch_mq_request_header::UnlockBatchMqRequestHeader;
use rocketmq_remoting::protocol::heartbeat::subscription_data::SubscriptionData;
use rocketmq_remoting::protocol::namesrv::RegisterBrokerResult;
use rocketmq_remoting::protocol::remoting_command::RemotingCommand;
use rocketmq_remoting::protocol::route::route_data_view::QueueData;
use rocketmq_remoting::protocol::route::topic_route_data::TopicRouteData;
use rocketmq_remoting::protocol::RemotingDeserializable;
use rocketmq_remoting::protocol::RemotingSerializable;
use rocketmq_remoting::remoting::RemotingService;
use rocketmq_remoting::request_processor::default_request_processor::DefaultRemotingRequestProcessor;
use rocketmq_remoting::rpc::client_metadata::ClientMetadata;
use rocketmq_remoting::rpc::rpc_client_impl::RpcClientImpl;
use rocketmq_remoting::rpc::rpc_request_header::RpcRequestHeader;
use rocketmq_remoting::runtime::config::client_config::TokioClientConfig;
use rocketmq_remoting::runtime::RPCHook;
use rocketmq_rust::ArcMut;
use rocketmq_store::base::message_store::MessageStore;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::broker_runtime::BrokerRuntimeInner;

pub struct BrokerOuterAPI {
    remoting_client: ArcMut<RocketmqDefaultClient<DefaultRemotingRequestProcessor>>,
    name_server_address: Option<String>,
    rpc_client: RpcClientImpl,
    client_metadata: ClientMetadata,
}

impl BrokerOuterAPI {
    pub fn new(tokio_client_config: Arc<TokioClientConfig>) -> Self {
        let client = ArcMut::new(RocketmqDefaultClient::new(
            tokio_client_config,
            DefaultRemotingRequestProcessor,
        ));
        let client_metadata = ClientMetadata::new();
        Self {
            remoting_client: client.clone(),
            name_server_address: None,
            rpc_client: RpcClientImpl::new(client_metadata.clone(), client),
            client_metadata,
        }
    }

    pub fn new_with_hook(
        tokio_client_config: Arc<TokioClientConfig>,
        rpc_hook: Option<Arc<Box<dyn RPCHook>>>,
    ) -> Self {
        let mut client = ArcMut::new(RocketmqDefaultClient::new(
            tokio_client_config,
            DefaultRemotingRequestProcessor,
        ));
        let client_metadata = ClientMetadata::new();
        if let Some(rpc_hook) = rpc_hook {
            client.register_rpc_hook(rpc_hook);
        }
        Self {
            remoting_client: client.clone(),
            name_server_address: None,
            rpc_client: RpcClientImpl::new(client_metadata.clone(), client),
            client_metadata,
        }
    }

    fn create_request(broker_name: CheetahString, topic_config: TopicConfig) -> RemotingCommand {
        let request_header =
            RegisterTopicRequestHeader::new(topic_config.topic_name.as_ref().cloned().unwrap());
        let queue_data = QueueData::new(
            broker_name,
            topic_config.read_queue_nums,
            topic_config.write_queue_nums,
            topic_config.perm,
            topic_config.topic_sys_flag,
        );
        let topic_route_data = TopicRouteData {
            queue_datas: vec![queue_data],
            ..Default::default()
        };
        let topic_route_body = topic_route_data
            .encode()
            .expect("encode topic route data failed");

        RemotingCommand::create_request_command(RequestCode::RegisterTopicInNamesrv, request_header)
            .set_body(topic_route_body)
    }
}

impl BrokerOuterAPI {
    pub async fn start(&self) {
        let wrapper = ArcMut::downgrade(&self.remoting_client);
        self.remoting_client.start(wrapper).await;
    }

    pub async fn update_name_server_address_list(&self, addrs: CheetahString) {
        let addr_vec = addrs
            .split(";")
            .map(CheetahString::from_slice)
            .collect::<Vec<CheetahString>>();
        self.remoting_client
            .update_name_server_address_list(addr_vec)
            .await
    }

    pub async fn update_name_server_address_list_by_dns_lookup(&self, domain: CheetahString) {
        let address_list = dns_lookup_address_by_domain(domain.as_str());
        self.remoting_client
            .update_name_server_address_list(address_list)
            .await;
    }

    pub async fn register_broker_all<MS: MessageStore>(
        &self,
        cluster_name: CheetahString,
        broker_addr: CheetahString,
        broker_name: CheetahString,
        broker_id: u64,
        ha_server_addr: CheetahString,
        topic_config_wrapper: TopicConfigAndMappingSerializeWrapper,
        filter_server_list: Vec<CheetahString>,
        oneway: bool,
        timeout_mills: u64,
        enable_acting_master: bool,
        compressed: bool,
        heartbeat_timeout_millis: Option<i64>,
        _broker_identity: BrokerIdentity,
        broker_runtime_inner: ArcMut<BrokerRuntimeInner<MS>>,
    ) -> Vec<RegisterBrokerResult> {
        let name_server_address_list = self.remoting_client.get_available_name_srv_list();
        let mut register_broker_result_list = Vec::new();
        if !name_server_address_list.is_empty() {
            let mut request_header = RegisterBrokerRequestHeader {
                broker_addr,
                broker_id,
                broker_name,
                cluster_name,
                ha_server_addr,
                enable_acting_master: Some(enable_acting_master),
                compressed: false,
                heartbeat_timeout_millis,
                body_crc32: 0,
            };

            //build request body
            let request_body = RegisterBrokerBody {
                topic_config_serialize_wrapper: topic_config_wrapper,
                filter_server_list,
            };
            let body = request_body.encode(compressed);
            let body_crc32 = crc32_utils::crc32(body.as_ref());
            request_header.body_crc32 = body_crc32;

            let mut handle_vec = Vec::with_capacity(name_server_address_list.len());
            for namesrv_addr in name_server_address_list.iter() {
                let cloned_body = body.clone();
                let cloned_header = request_header.clone();
                let addr = namesrv_addr.clone();
                let broker_runtime_inner_ = broker_runtime_inner.clone();
                let join_handle = tokio::spawn(async move {
                    broker_runtime_inner_
                        .broker_outer_api()
                        .register_broker(&addr, oneway, timeout_mills, cloned_header, cloned_body)
                        .await
                });
                /*let handle =
                self.register_broker(addr, oneway, timeout_mills, cloned_header, cloned_body);*/
                handle_vec.push(join_handle);
            }
            while let Some(handle) = handle_vec.pop() {
                let result = tokio::join!(handle);
                match result.0 {
                    Ok(value) => {
                        if let Some(v) = value {
                            register_broker_result_list.push(v);
                        } else {
                            error!("Register broker to name remoting_server error");
                        }
                    }
                    Err(e) => {
                        error!("Register broker to name remoting_server error, error={}", e);
                    }
                }
            }
        }

        register_broker_result_list
    }

    async fn register_broker(
        &self,
        namesrv_addr: &CheetahString,
        oneway: bool,
        timeout_mills: u64,
        request_header: RegisterBrokerRequestHeader,
        body: Vec<u8>,
    ) -> Option<RegisterBrokerResult> {
        debug!(
            "Register broker to name remoting_server, namesrv_addr={},request_code={:?}, \
             request_header={:?}, body={:?}",
            namesrv_addr,
            RequestCode::RegisterBroker,
            request_header,
            body
        );
        let request =
            RemotingCommand::create_request_command(RequestCode::RegisterBroker, request_header)
                .set_body(body.clone());
        if oneway {
            self.remoting_client
                .invoke_oneway(namesrv_addr, request, timeout_mills)
                .await;
            return None;
        }
        match self
            .remoting_client
            .invoke_async(Some(namesrv_addr), request, timeout_mills)
            .await
        {
            Ok(response) => match From::from(response.code()) {
                ResponseCode::Success => {
                    info!(
                        "Register broker to name remoting_server success, namesrv_addr={} \
                         response body={:?}",
                        namesrv_addr,
                        response.body()
                    );
                    let register_broker_result =
                        response.decode_command_custom_header::<RegisterBrokerResponseHeader>();
                    let mut result = RegisterBrokerResult::default();
                    if let Ok(header) = register_broker_result {
                        result.ha_server_addr = header
                            .ha_server_addr
                            .clone()
                            .unwrap_or(CheetahString::empty());
                        result.master_addr =
                            header.master_addr.clone().unwrap_or(CheetahString::empty());
                    }
                    if let Some(body) = response.body() {
                        result.kv_table = SerdeJsonUtils::decode::<KVTable>(body.as_ref()).unwrap();
                    }
                    Some(result)
                }
                _ => None,
            },
            Err(err) => {
                error!(
                    "Register broker to name remoting_server error, namesrv_addr={}, error={}",
                    namesrv_addr, err
                );
                None
            }
        }
    }

    /// Register the topic route info of single topic to all name remoting_server nodes.
    /// This method is used to replace incremental broker registration feature.
    pub async fn register_single_topic_all(
        &self,
        broker_name: CheetahString,
        topic_config: TopicConfig,
        timeout_mills: u64,
    ) {
        let request = Self::create_request(broker_name, topic_config);
        let name_server_address_list = self.remoting_client.get_available_name_srv_list();
        let mut handle_vec = Vec::with_capacity(name_server_address_list.len());
        for namesrv_addr in name_server_address_list.iter() {
            let cloned_request = request.clone();
            let addr = namesrv_addr.clone();
            let client = self.remoting_client.clone();
            let join_handle = tokio::spawn(async move {
                client
                    .invoke_async(Some(&addr), cloned_request, timeout_mills)
                    .await
            });
            handle_vec.push(join_handle);
        }
        while let Some(handle) = handle_vec.pop() {
            let _result = tokio::join!(handle);
        }
    }

    pub fn shutdown(&mut self) {
        self.remoting_client.shutdown();
    }

    pub fn refresh_metadata(&self) {}

    pub fn rpc_client(&self) -> &RpcClientImpl {
        &self.rpc_client
    }

    pub async fn lock_batch_mq_async(
        &self,
        addr: &CheetahString,
        request_body: bytes::Bytes,
        timeout_millis: u64,
    ) -> rocketmq_error::RocketMQResult<HashSet<MessageQueue>> {
        let mut request = RemotingCommand::create_request_command(
            RequestCode::LockBatchMq,
            LockBatchMqRequestHeader::default(),
        );
        request.set_body_mut_ref(request_body);
        let result = self
            .remoting_client
            .invoke_async(Some(addr), request, timeout_millis)
            .await;
        match result {
            Ok(response) => {
                if ResponseCode::from(response.code()) == ResponseCode::Success {
                    let lock_batch_response_body =
                        LockBatchResponseBody::decode(response.get_body().unwrap()).unwrap();
                    Ok(lock_batch_response_body.lock_ok_mq_set)
                } else {
                    Err(RocketmqError::MQBrokerError(
                        response.code(),
                        response
                            .remark()
                            .cloned()
                            .unwrap_or(CheetahString::empty())
                            .to_json()
                            .expect("to json failed"),
                        "".to_string(),
                    ))
                }
            }
            Err(e) => Err(e),
        }
    }

    pub async fn unlock_batch_mq_async(
        &self,
        addr: &CheetahString,
        request_body: bytes::Bytes,
        timeout_millis: u64,
    ) -> rocketmq_error::RocketMQResult<()> {
        let mut request = RemotingCommand::create_request_command(
            RequestCode::UnlockBatchMq,
            UnlockBatchMqRequestHeader::default(),
        );
        request.set_body_mut_ref(request_body);
        let result = self
            .remoting_client
            .invoke_async(Some(addr), request, timeout_millis)
            .await;
        match result {
            Ok(response) => {
                if ResponseCode::from(response.code()) == ResponseCode::Success {
                    Ok(())
                } else {
                    Err(RocketmqError::MQBrokerError(
                        response.code(),
                        response
                            .remark()
                            .cloned()
                            .unwrap_or(CheetahString::empty())
                            .to_string(),
                        "".to_string(),
                    ))
                }
            }
            Err(e) => Err(e),
        }
    }

    pub async fn get_topic_route_info_from_name_server(
        &self,
        topic: &CheetahString,
        timeout_millis: u64,
        allow_topic_not_exist: bool,
    ) -> rocketmq_error::RocketMQResult<TopicRouteData> {
        let header = GetRouteInfoRequestHeader {
            topic: topic.clone(),
            ..Default::default()
        };
        let request =
            RemotingCommand::create_request_command(RequestCode::GetRouteinfoByTopic, header);
        let response = self
            .remoting_client
            .invoke_async(None, request, timeout_millis)
            .await?;
        match ResponseCode::from(response.code()) {
            ResponseCode::TopicNotExist => {
                if allow_topic_not_exist {
                    warn!(
                        "get Topic [{}] RouteInfoFromNameServer is not exist value",
                        topic
                    );
                }
            }
            ResponseCode::Success => {
                if let Some(body) = response.body() {
                    let topic_route_data = TopicRouteData::decode(body).unwrap();
                    return Ok(topic_route_data);
                }
            }
            _ => {}
        }
        Err(RocketmqError::MQBrokerError(
            response.code(),
            response
                .remark()
                .cloned()
                .unwrap_or(CheetahString::empty())
                .to_string(),
            "".to_string(),
        ))
    }

    pub async fn send_message_to_specific_broker(
        &self,
        broker_addr: &CheetahString,
        broker_name: &CheetahString,
        msg: MessageExt,
        group: CheetahString,
        timeout_millis: u64,
    ) -> rocketmq_error::RocketMQResult<SendResult> {
        let uniq_msg_id = MessageClientIDSetter::get_uniq_id(&msg);
        let queue_id = msg.queue_id;
        let topic = msg.get_topic().clone();
        let request = build_send_message_request(msg, group);
        let response = self
            .remoting_client
            .invoke_async(Some(broker_addr), request, timeout_millis)
            .await?;

        process_send_response(
            broker_name,
            uniq_msg_id.unwrap_or_default(),
            queue_id,
            topic,
            &response,
        )
    }

    pub async fn pull_message_from_specific_broker_async(
        &self,
        broker_name: &CheetahString,
        broker_addr: &CheetahString,
        consumer_group: &CheetahString,
        topic: &CheetahString,
        queue_id: i32,
        offset: i64,
        max_nums: i32,
        timeout_millis: u64,
    ) -> rocketmq_error::RocketMQResult<(Option<PullResult>, String, bool)> {
        let request_header = PullMessageRequestHeader {
            consumer_group: consumer_group.clone(),
            topic: topic.clone(),
            queue_id,
            queue_offset: offset,
            max_msg_nums: max_nums,
            sys_flag: PullSysFlag::build_sys_flag(false, false, true, false) as i32,
            commit_offset: 0,
            suspend_timeout_millis: 0,
            subscription: Some(CheetahString::from_static_str(SubscriptionData::SUB_ALL)),
            sub_version: get_current_millis() as i64,
            expression_type: Some(CheetahString::from_static_str(ExpressionType::TAG)),
            max_msg_bytes: Some(i32::MAX),
            topic_request: Some(TopicRequestHeader {
                lo: None,
                rpc: Some(RpcRequestHeader {
                    broker_name: Some(broker_name.clone()),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        };
        let request_command =
            RemotingCommand::create_request_command(RequestCode::PullMessage, request_header);
        match self
            .remoting_client
            .invoke_async(Some(broker_addr), request_command, timeout_millis)
            .await
        {
            Ok(response) => {
                let code = response.code();
                let mut pull_result_ext = match process_pull_response(response, broker_addr) {
                    Ok(value) => value,
                    Err(_) => return Ok((None, format!("Response Code:{}", code), true)),
                };
                let name = pull_result_ext.pull_result.pull_status().to_string();
                process_pull_result(&mut pull_result_ext, broker_name, queue_id);
                Ok((Some(pull_result_ext.pull_result), name, false))
            }
            Err(e) => Ok((None, e.to_string(), true)),
        }
    }

    pub async fn unregister_broker_all(
        &self,
        cluster_name: &CheetahString,
        broker_name: &CheetahString,
        broker_addr: &CheetahString,
        broker_id: u64,
    ) {
        let name_server_address_list = self.remoting_client.get_name_server_address_list();
        for namesrv_addr in name_server_address_list.iter() {
            match self
                .unregister_broker(
                    namesrv_addr,
                    cluster_name,
                    broker_addr,
                    broker_name,
                    broker_id,
                )
                .await
            {
                Ok(_) => {
                    info!(
                        "Unregister broker from name remoting_server success, namesrv_addr={}",
                        namesrv_addr
                    );
                }
                Err(e) => {
                    error!(
                        "Unregister broker from name remoting_server error, namesrv_addr={}, \
                         error={}",
                        namesrv_addr, e
                    );
                }
            }
        }
    }
    pub async fn unregister_broker(
        &self,
        namesrv_addr: &CheetahString,
        cluster_name: &CheetahString,
        broker_addr: &CheetahString,
        broker_name: &CheetahString,
        broker_id: u64,
    ) -> rocketmq_error::RocketMQResult<()> {
        let request_header = UnRegisterBrokerRequestHeader {
            broker_name: broker_name.clone(),
            broker_addr: broker_addr.clone(),
            cluster_name: cluster_name.clone(),
            broker_id,
        };
        let request =
            RemotingCommand::create_request_command(RequestCode::UnregisterBroker, request_header);
        let response = self
            .remoting_client
            .invoke_async(Some(namesrv_addr), request, 3000)
            .await?;
        if ResponseCode::from(response.code()) == ResponseCode::Success {
            Ok(())
        } else {
            Err(RocketmqError::MQBrokerError(
                response.code(),
                response.remark().map_or("".to_string(), |s| s.to_string()),
                broker_addr.to_string(),
            ))
        }
    }
}

fn process_pull_result(
    pull_result: &mut PullResultExt,
    broker_name: &CheetahString,
    queue_id: i32,
) {
    if *pull_result.pull_result.pull_status() == PullStatus::Found {
        let mut bytes = pull_result.message_binary.take().unwrap_or_default();
        let mut message_list = MessageDecoder::decodes_batch(&mut bytes, true, true);
        for message in message_list.iter_mut() {
            let tra_flag = message.get_property(&CheetahString::from_static_str(
                MessageConst::PROPERTY_TRANSACTION_PREPARED,
            ));
            if tra_flag.is_some() && tra_flag.unwrap() == "true" {
                if let Some(id) = message.get_property(&CheetahString::from_static_str(
                    MessageConst::PROPERTY_UNIQ_CLIENT_MESSAGE_ID_KEYIDX,
                )) {
                    message.set_transaction_id(id);
                }
            }
            MessageAccessor::put_property(
                message,
                CheetahString::from_static_str(MessageConst::PROPERTY_MIN_OFFSET),
                pull_result.pull_result.min_offset().to_string().into(),
            );
            MessageAccessor::put_property(
                message,
                CheetahString::from_static_str(MessageConst::PROPERTY_MAX_OFFSET),
                pull_result.pull_result.max_offset().to_string().into(),
            );
            message.set_broker_name(broker_name.clone());
            message.set_queue_id(queue_id);
            if let Some(offset_delta) = pull_result.offset_delta {
                message.set_queue_offset(message.queue_offset + offset_delta);
            }
        }
    }
}

fn process_pull_response(
    mut response: RemotingCommand,
    addr: &CheetahString,
) -> rocketmq_error::RocketMQResult<PullResultExt> {
    let pull_status = match ResponseCode::from(response.code()) {
        ResponseCode::Success => PullStatus::Found,
        ResponseCode::PullNotFound => PullStatus::NoNewMsg,
        ResponseCode::PullRetryImmediately => PullStatus::NoMatchedMsg,
        ResponseCode::PullOffsetMoved => PullStatus::OffsetIllegal,
        _ => {
            return Err(RocketmqError::MQBrokerError(
                response.code(),
                response.remark().map_or("".to_string(), |s| s.to_string()),
                addr.to_string(),
            ))
        }
    };
    let response_header = response.decode_command_custom_header::<PullMessageResponseHeader>()?;
    let pull_result = PullResultExt {
        pull_result: PullResult::new(
            pull_status,
            response_header.next_begin_offset as u64,
            response_header.min_offset as u64,
            response_header.max_offset as u64,
            Some(vec![]),
        ),
        suggest_which_broker_id: response_header.suggest_which_broker_id,
        message_binary: response.take_body(),
        offset_delta: response_header.offset_delta,
    };
    Ok(pull_result)
}

fn dns_lookup_address_by_domain(domain: &str) -> Vec<CheetahString> {
    let mut address_list = Vec::new();
    // Ensure logging is initialized

    match domain.find(':') {
        Some(index) => {
            let (domain_str, port_str) = domain.split_at(index);
            match lookup_host(domain_str) {
                Ok(addresses) => {
                    for address in addresses {
                        address_list.push(format!("{}{}", address, port_str).into());
                    }
                    info!(
                        "DNS lookup address by domain success, domain={}, result={:?}",
                        domain, address_list
                    );
                }
                Err(e) => {
                    error!(
                        "DNS lookup address by domain error, domain={}, error={}",
                        domain, e
                    );
                }
            }
        }
        None => {
            error!("Invalid domain format, missing port: {}", domain);
        }
    }

    address_list
}

fn build_send_message_request(msg: MessageExt, group: CheetahString) -> RemotingCommand {
    let header = build_send_message_request_header_v2(msg, group);
    RemotingCommand::create_request_command(RequestCode::SendMessage, header)
}

fn build_send_message_request_header_v2(
    msg: MessageExt,
    group: CheetahString,
) -> SendMessageRequestHeaderV2 {
    let header = SendMessageRequestHeader {
        producer_group: group,
        topic: msg.get_topic().clone(),
        default_topic: CheetahString::from_static_str(TopicValidator::AUTO_CREATE_TOPIC_KEY_TOPIC),
        default_topic_queue_nums: 8,
        queue_id: msg.queue_id,
        sys_flag: msg.sys_flag,
        born_timestamp: msg.born_timestamp,
        flag: msg.get_flag(),
        properties: Some(MessageDecoder::message_properties_to_string(
            msg.get_properties(),
        )),
        reconsume_times: Some(msg.reconsume_times),
        batch: Some(false),
        ..Default::default()
    };
    SendMessageRequestHeaderV2::create_send_message_request_header_v2_with_move(header)
}

pub fn process_send_response(
    broker_name: &CheetahString,
    uniq_msg_id: CheetahString,
    queue_id: i32,
    topic: CheetahString,
    response: &RemotingCommand,
) -> rocketmq_error::RocketMQResult<SendResult> {
    let mut send_status: Option<SendStatus> = None;

    // Match the response code to the corresponding SendStatus
    match ResponseCode::from(response.code()) {
        ResponseCode::FlushDiskTimeout => send_status = Some(SendStatus::FlushDiskTimeout),
        ResponseCode::FlushSlaveTimeout => send_status = Some(SendStatus::FlushSlaveTimeout),
        ResponseCode::SlaveNotAvailable => send_status = Some(SendStatus::SlaveNotAvailable),
        ResponseCode::Success => send_status = Some(SendStatus::SendOk),
        _ => (),
    };

    // If send_status is not None, process the response
    if let Some(status) = send_status {
        let response_header =
            response.decode_command_custom_header::<SendMessageResponseHeader>()?;

        let message_queue = MessageQueue::from_parts(topic, broker_name, queue_id);

        let mut send_result = SendResult::new(
            status,
            Some(uniq_msg_id),
            Some(response_header.msg_id().to_string()),
            Some(message_queue),
            response_header.queue_id() as u64,
        );

        send_result.set_transaction_id(
            response_header
                .transaction_id()
                .map_or("".to_string(), |s| s.to_string()),
        );
        if let Some(region_id) =
            response
                .get_ext_fields()
                .unwrap()
                .get(&CheetahString::from_static_str(
                    MessageConst::PROPERTY_MSG_REGION,
                ))
        {
            send_result.set_region_id(region_id.to_string());
        } else {
            send_result.set_region_id(mix_all::DEFAULT_TRACE_REGION_ID.to_string());
        }

        if let Some(trace_on) =
            response
                .get_ext_fields()
                .unwrap()
                .get(&CheetahString::from_static_str(
                    MessageConst::PROPERTY_MSG_REGION,
                ))
        {
            send_result.set_trace_on(trace_on == "true");
        } else {
            send_result.set_trace_on(false);
        }
        return Ok(send_result);
    }

    // If send_status is None, we throw an error
    Err(RocketmqError::MQBrokerError(
        response.code(),
        "".to_string(),
        response.remark().map_or("".to_string(), |s| s.to_string()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_lookup_address_by_domain_returns_correct_addresses() {
        let domain = "localhost:8080";
        let addresses = dns_lookup_address_by_domain(domain);
        assert!(addresses.contains(&"127.0.0.1:8080".into()));
    }

    #[test]
    fn dns_lookup_address_by_domain_handles_invalid_domain() {
        let domain = "invalid_domain";
        let addresses = dns_lookup_address_by_domain(domain);
        assert!(addresses.is_empty());
    }

    #[test]
    fn dns_lookup_address_by_domain_handles_domain_without_port() {
        let domain = "localhost";
        let addresses = dns_lookup_address_by_domain(domain);
        assert!(addresses.is_empty());
    }
}
