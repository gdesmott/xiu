//This mod will be move out of the rtmp library.
pub mod define;
pub mod errors;

use {
    crate::cache::Cache,
    crate::notify::Notifier,
    crate::session::{common::SubscriberInfo, define::SubscribeType},
    define::{
        AvStatisticSender, ChannelData, ChannelDataConsumer, ChannelDataProducer, ChannelEvent,
        ChannelEventConsumer, ChannelEventProducer, ClientEvent, ClientEventConsumer,
        ClientEventProducer, PubSubInfo, StreamStatisticSizeSender, TransmitterEvent,
        TransmitterEventConsumer, TransmitterEventProducer,
    },
    errors::{ChannelError, ChannelErrorValue},
    std::collections::HashMap,
    tokio::sync::{broadcast, mpsc, mpsc::UnboundedReceiver},
    uuid::Uuid,
};

/************************************************************************************
* For a publisher, we new a broadcast::channel .
* For a player, we also new a oneshot::channel which subscribe the puslisher's broadcast channel,
* because we not only need to send av data from the publisher,but also some cache data(metadata
* and seq headers), so establishing a middle channel is needed.
************************************************************************************
*
*          stream_producer                      player_producers
*
*                                         sender(oneshot::channel) player
*                                    ----------------------------------
*                                   /     sender(oneshot::channel) player
*                                  /   --------------------------------
*           (broadcast::channel)  /   /   sender(oneshot::channel) player
* publisher --------------------->--------------------------------------
*                                 \   \   sender(oneshot::channel) player
*                                  \   --------------------------------
*                                   \     sender(oneshot::channel) player
*                                     ---------------------------------
*
*************************************************************************************/

//receive data from ChannelsManager and send to players/subscribers
pub struct Transmitter {
    //used for receiving Audio/Video data
    data_consumer: ChannelDataConsumer,
    //used for receiving event
    event_consumer: TransmitterEventConsumer,
    //used for sending audio/video data to players/subscribers
    subscriberid_to_producer: HashMap<Uuid, ChannelDataProducer>,
    //used for cache metadata and GOP
    cache: Cache,
}

impl Transmitter {
    fn new(
        app_name: String,
        stream_name: String,
        data_consumer: UnboundedReceiver<ChannelData>,
        event_consumer: UnboundedReceiver<TransmitterEvent>,
        gop_num: usize,
    ) -> Self {
        Self {
            data_consumer,
            event_consumer,
            subscriberid_to_producer: HashMap::new(),
            cache: Cache::new(app_name, stream_name, gop_num),
        }
    }

    pub async fn run(&mut self) -> Result<(), ChannelError> {
        loop {
            tokio::select! {
                data = self.event_consumer.recv() =>{
                    if let Some(val) = data {
                        match val {
                            TransmitterEvent::Subscribe {
                                producer,
                                info,
                            } => {

                                if let Some(meta_body_data) = self.cache.get_metadata() {
                                    producer.send(meta_body_data).map_err(|_| ChannelError {
                                        value: ChannelErrorValue::SendError,
                                    })?;
                                }
                                if let Some(audio_seq_data) = self.cache.get_audio_seq() {
                                    producer.send(audio_seq_data).map_err(|_| ChannelError {
                                        value: ChannelErrorValue::SendError,
                                    })?;
                                }
                                if let Some(video_seq_data) = self.cache.get_video_seq() {
                                    producer.send(video_seq_data).map_err(|_| ChannelError {
                                        value: ChannelErrorValue::SendError,
                                    })?;
                                }

                                match info.sub_type {
                                    SubscribeType::PlayerRtmp
                                    | SubscribeType::PlayerHttpFlv
                                    | SubscribeType::PlayerHls
                                    | SubscribeType::GenerateHls => {
                                        if let Some(gops_data) = self.cache.get_gops_data() {
                                            for gop in gops_data {
                                                for channel_data in gop.get_frame_data() {
                                                    producer.send(channel_data).map_err(|_| ChannelError {
                                                        value: ChannelErrorValue::SendError,
                                                    })?;
                                                }
                                            }
                                        }
                                    }
                                    SubscribeType::PublisherRtmp => {}
                                }
                                self.subscriberid_to_producer
                                    .insert(info.id, producer);
                            }
                            TransmitterEvent::UnSubscribe { info } => {
                                self.subscriberid_to_producer
                                    .remove(&info.id);
                            }
                            TransmitterEvent::UnPublish {} => {
                                return Ok(());
                            }
                            TransmitterEvent::Api { sender } => {
                                let avstatistic_data = self.cache.av_statistics.get_avstatistic_data().await;
                                if let Err(err) = sender.send(avstatistic_data){
                                    log::info!("Transmitter send avstatistic data err: {}",err);
                                }
                            }
                        }
                    }
                }

                data = self.data_consumer.recv() =>{
                    if let Some(val) = data {
                        match val {
                            ChannelData::MetaData { timestamp, data } => {
                                self.cache.save_metadata(data, timestamp);
                            }
                            ChannelData::Audio { timestamp, data } => {
                                self.cache.save_audio_data(data.clone(), timestamp).await?;

                                let data = ChannelData::Audio {
                                    timestamp,
                                    data: data.clone(),
                                };

                                for (_, v) in self.subscriberid_to_producer.iter() {
                                    if let Err(audio_err) = v.send(data.clone()).map_err(|_| ChannelError {
                                        value: ChannelErrorValue::SendAudioError,
                                    }) {
                                        log::error!("Transmiter send error: {}", audio_err);
                                    }
                                }
                            }
                            ChannelData::Video { timestamp, data } => {
                                self.cache.save_video_data(data.clone(), timestamp).await?;

                                let data = ChannelData::Video {
                                    timestamp,
                                    data: data.clone(),
                                };
                                for (_, v) in self.subscriberid_to_producer.iter() {
                                    if let Err(video_err) = v.send(data.clone()).map_err(|_| ChannelError {
                                        value: ChannelErrorValue::SendVideoError,
                                    }) {
                                        log::error!("Transmiter send error: {}", video_err);
                                    }
                                }
                            }
                        }
                    }
                }

            }
        }

        //Ok(())
    }
}

pub struct ChannelsManager {
    //app_name to stream_name to producer
    channels: HashMap<String, HashMap<String, TransmitterEventProducer>>,
    //save info to kick off client
    channels_info: HashMap<Uuid, PubSubInfo>,
    //event is consumed in Channels, produced from other rtmp sessions
    channel_event_consumer: ChannelEventConsumer,
    //event is produced from other rtmp sessions
    channel_event_producer: ChannelEventProducer,
    //client_event_producer: client_event_producer
    client_event_producer: ClientEventProducer,
    //configure how many gops will be cached.
    rtmp_gop_num: usize,
    //The rtmp static push/pull and the hls transfer is triggered actively,
    //add a control switches separately.
    rtmp_push_enabled: bool,
    //enable rtmp pull
    rtmp_pull_enabled: bool,
    //enable hls
    hls_enabled: bool,
    //http notifier on sub/pub event
    notifier: Option<Notifier>,
}

impl ChannelsManager {
    pub fn new(notifier: Option<Notifier>) -> Self {
        let (event_producer, event_consumer) = mpsc::unbounded_channel();
        let (client_producer, _) = broadcast::channel(100);

        Self {
            channels: HashMap::new(),
            channels_info: HashMap::new(),
            channel_event_consumer: event_consumer,
            channel_event_producer: event_producer,
            client_event_producer: client_producer,
            rtmp_push_enabled: false,
            rtmp_pull_enabled: false,
            rtmp_gop_num: 1,
            hls_enabled: false,
            notifier,
        }
    }
    pub async fn run(&mut self) {
        self.event_loop().await;
    }

    pub fn set_rtmp_push_enabled(&mut self, enabled: bool) {
        self.rtmp_push_enabled = enabled;
    }

    pub fn set_rtmp_pull_enabled(&mut self, enabled: bool) {
        self.rtmp_pull_enabled = enabled;
    }

    pub fn set_rtmp_gop_num(&mut self, gop_num: usize) {
        self.rtmp_gop_num = gop_num;
    }

    pub fn set_hls_enabled(&mut self, enabled: bool) {
        self.hls_enabled = enabled;
    }

    pub fn get_channel_event_producer(&mut self) -> ChannelEventProducer {
        self.channel_event_producer.clone()
    }

    pub fn get_client_event_consumer(&mut self) -> ClientEventConsumer {
        self.client_event_producer.subscribe()
    }

    pub async fn event_loop(&mut self) {
        while let Some(message) = self.channel_event_consumer.recv().await {
            let event_serialize_str = if let Ok(data) = serde_json::to_string(&message) {
                log::info!("event data: {}", data);
                data
            } else {
                String::from("empty body")
            };

            match message {
                ChannelEvent::Publish {
                    app_name,
                    stream_name,
                    responder,
                    info,
                } => {
                    let rv = self.publish(&app_name, &stream_name);
                    match rv {
                        Ok(producer) => {
                            if responder.send(producer).is_err() {
                                log::error!("event_loop responder send err");
                            }
                            if let Some(notifier) = &self.notifier {
                                notifier.on_publish_notify(event_serialize_str).await;
                            }
                            self.channels_info.insert(
                                info.id,
                                PubSubInfo::Publish {
                                    app_name,
                                    stream_name,
                                },
                            );
                        }
                        Err(err) => {
                            log::error!("event_loop Publish err: {}\n", err);
                            continue;
                        }
                    }
                }

                ChannelEvent::UnPublish {
                    app_name,
                    stream_name,
                    info: _,
                } => {
                    if let Err(err) = self.unpublish(&app_name, &stream_name) {
                        log::error!(
                            "event_loop Unpublish err: {} with app name: {} stream name :{}\n",
                            err,
                            app_name,
                            stream_name
                        );
                    }

                    if let Some(notifier) = &self.notifier {
                        notifier.on_unpublish_notify(event_serialize_str).await;
                    }
                }
                ChannelEvent::Subscribe {
                    app_name,
                    stream_name,
                    info,
                    responder,
                } => {
                    let sub_id = info.id;
                    let rv = self.subscribe(&app_name, &stream_name, info.clone()).await;
                    match rv {
                        Ok(consumer) => {
                            if responder.send(consumer).is_err() {
                                log::error!("event_loop Subscribe err");
                            }

                            if let Some(notifier) = &self.notifier {
                                notifier.on_play_notify(event_serialize_str).await;
                            }

                            self.channels_info.insert(
                                sub_id,
                                PubSubInfo::Subscribe {
                                    app_name,
                                    stream_name,
                                    sub_info: info,
                                },
                            );
                        }
                        Err(err) => {
                            log::error!("event_loop Subscribe error: {}", err);
                            continue;
                        }
                    }
                }
                ChannelEvent::UnSubscribe {
                    app_name,
                    stream_name,
                    info,
                } => {
                    if self.unsubscribe(&app_name, &stream_name, info).is_ok() {
                        if let Some(notifier) = &self.notifier {
                            notifier.on_stop_notify(event_serialize_str).await;
                        }
                    }
                }

                ChannelEvent::ApiStatistic {
                    data_sender,
                    size_sender,
                } => {
                    if let Err(err) = self.api_statistic(data_sender, size_sender) {
                        log::error!("event_loop api error: {}", err);
                    }
                }
                ChannelEvent::ApiKickClient { id } => {
                    self.api_kick_off_client(id);

                    if let Some(notifier) = &self.notifier {
                        notifier.on_unpublish_notify(event_serialize_str).await;
                    }
                }
            }
        }
    }

    fn api_statistic(
        &mut self,
        data_sender: AvStatisticSender,
        size_sender: StreamStatisticSizeSender,
    ) -> Result<(), ChannelError> {
        let mut stream_count: usize = 0;
        for v in self.channels.values() {
            for event_sender in v.values() {
                stream_count += 1;
                if let Err(err) = event_sender.send(TransmitterEvent::Api {
                    sender: data_sender.clone(),
                }) {
                    log::error!("TransmitterEvent  api send data err: {}", err);
                    return Err(ChannelError {
                        value: ChannelErrorValue::SendError,
                    });
                }
            }
        }

        if let Err(err) = size_sender.send(stream_count) {
            log::error!("TransmitterEvent api send size err: {}", err);
            return Err(ChannelError {
                value: ChannelErrorValue::SendError,
            });
        }

        Ok(())
    }

    fn api_kick_off_client(&mut self, uid: Uuid) {
        let info = if let Some(info) = self.channels_info.get(&uid) {
            info.clone()
        } else {
            return;
        };

        match info {
            PubSubInfo::Publish {
                app_name,
                stream_name,
            } => {
                if let Err(err) = self.unpublish(&app_name, &stream_name) {
                    log::error!(
                        "event_loop ApiKickClient pub err: {} with app name: {} stream name :{}\n",
                        err,
                        app_name,
                        stream_name
                    );
                }
            }
            PubSubInfo::Subscribe {
                app_name,
                stream_name,
                sub_info,
            } => {
                if let Err(err) = self.unsubscribe(&app_name, &stream_name, sub_info) {
                    log::error!(
                        "event_loop ApiKickClient pub err: {} with app name: {} stream name :{}\n",
                        err,
                        app_name,
                        stream_name
                    );
                }
            }
        }
    }

    //player subscribe a stream
    pub async fn subscribe(
        &mut self,
        app_name: &String,
        stream_name: &String,
        sub_info: SubscriberInfo,
    ) -> Result<mpsc::UnboundedReceiver<ChannelData>, ChannelError> {
        if let Some(val) = self.channels.get_mut(app_name) {
            if let Some(producer) = val.get_mut(stream_name) {
                let (channel_data_producer, channel_data_consumer) = mpsc::unbounded_channel();
                let event = TransmitterEvent::Subscribe {
                    producer: channel_data_producer,
                    info: sub_info,
                };

                producer.send(event).map_err(|_| ChannelError {
                    value: ChannelErrorValue::SendError,
                })?;

                return Ok(channel_data_consumer);
            }
        }

        if self.rtmp_pull_enabled {
            log::info!(
                "subscribe: try to pull stream, app_name: {}, stream_name: {}",
                app_name,
                stream_name
            );

            let client_event = ClientEvent::Subscribe {
                app_name: app_name.clone(),
                stream_name: stream_name.clone(),
            };

            //send subscribe info to pull clients
            self.client_event_producer
                .send(client_event)
                .map_err(|_| ChannelError {
                    value: ChannelErrorValue::SendError,
                })?;
        }

        Err(ChannelError {
            value: ChannelErrorValue::NoAppOrStreamName,
        })
    }

    pub fn unsubscribe(
        &mut self,
        app_name: &String,
        stream_name: &String,
        sub_info: SubscriberInfo,
    ) -> Result<(), ChannelError> {
        match self.channels.get_mut(app_name) {
            Some(val) => match val.get_mut(stream_name) {
                Some(producer) => {
                    let event = TransmitterEvent::UnSubscribe { info: sub_info };
                    producer.send(event).map_err(|_| ChannelError {
                        value: ChannelErrorValue::SendError,
                    })?;
                }
                None => {
                    return Err(ChannelError {
                        value: ChannelErrorValue::NoStreamName,
                    })
                }
            },
            None => {
                return Err(ChannelError {
                    value: ChannelErrorValue::NoAppName,
                })
            }
        }

        Ok(())
    }

    //publish a stream
    pub fn publish(
        &mut self,
        app_name: &String,
        stream_name: &String,
    ) -> Result<ChannelDataProducer, ChannelError> {
        match self.channels.get_mut(app_name) {
            Some(val) => {
                if val.get(stream_name).is_some() {
                    return Err(ChannelError {
                        value: ChannelErrorValue::Exists,
                    });
                }
            }
            None => {
                let stream_map = HashMap::new();
                self.channels.insert(app_name.clone(), stream_map);
            }
        }

        if let Some(stream_map) = self.channels.get_mut(app_name) {
            let (event_publisher, event_consumer) = mpsc::unbounded_channel();
            let (data_publisher, data_consumer) = mpsc::unbounded_channel();

            let mut transmitter = Transmitter::new(
                app_name.clone(),
                stream_name.clone(),
                data_consumer,
                event_consumer,
                self.rtmp_gop_num,
            );

            let app_name_clone = app_name.clone();
            let stream_name_clone = stream_name.clone();

            tokio::spawn(async move {
                if let Err(err) = transmitter.run().await {
                    log::error!(
                        "transmiter run error, app_name: {}, stream_name: {}, error: {}",
                        app_name_clone,
                        stream_name_clone,
                        err,
                    );
                } else {
                    log::info!(
                        "transmiter exists: app_name: {}, stream_name: {}",
                        app_name_clone,
                        stream_name_clone
                    );
                }
            });

            stream_map.insert(stream_name.clone(), event_publisher);

            if self.rtmp_push_enabled || self.hls_enabled {
                let client_event = ClientEvent::Publish {
                    app_name: app_name.clone(),
                    stream_name: stream_name.clone(),
                };

                //send publish info to push clients
                self.client_event_producer
                    .send(client_event)
                    .map_err(|_| ChannelError {
                        value: ChannelErrorValue::SendError,
                    })?;
            }

            Ok(data_publisher)
        } else {
            Err(ChannelError {
                value: ChannelErrorValue::NoAppName,
            })
        }
    }

    fn unpublish(&mut self, app_name: &String, stream_name: &String) -> Result<(), ChannelError> {
        match self.channels.get_mut(app_name) {
            Some(val) => match val.get_mut(stream_name) {
                Some(producer) => {
                    let event = TransmitterEvent::UnPublish {};
                    producer.send(event).map_err(|_| ChannelError {
                        value: ChannelErrorValue::SendError,
                    })?;
                    val.remove(stream_name);
                    log::info!(
                        "unpublish remove stream, app_name: {},stream_name: {}",
                        app_name,
                        stream_name
                    );
                }
                None => {
                    return Err(ChannelError {
                        value: ChannelErrorValue::NoStreamName,
                    })
                }
            },
            None => {
                return Err(ChannelError {
                    value: ChannelErrorValue::NoAppName,
                })
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use std::cell::RefCell;

    use std::sync::Arc;
    pub struct TestFunc {}

    impl TestFunc {
        fn new() -> Self {
            Self {}
        }
        pub fn aaa(&mut self) {}
    }

    //https://juejin.cn/post/6844904105698148360
    #[test]
    fn test_lock() {
        let channel = Arc::new(RefCell::new(TestFunc::new()));
        channel.borrow_mut().aaa();
    }
}
