use {
    super::{errors::HlsError, flv_data_receiver::FlvDataReceiver},
    rtmp::channels::define::{ChannelEventProducer, ClientEvent, ClientEventConsumer},
};

pub struct RtmpEventProcessor {
    client_event_consumer: ClientEventConsumer,
    event_producer: ChannelEventProducer,
}

impl RtmpEventProcessor {
    pub fn new(consumer: ClientEventConsumer, event_producer: ChannelEventProducer) -> Self {
        Self {
            client_event_consumer: consumer,
            event_producer,
        }
    }

    pub async fn run(&mut self) -> Result<(), HlsError> {
        loop {
            let val = self.client_event_consumer.recv().await?;
            match val {
                ClientEvent::Publish {
                    app_name,
                    stream_name,
                } => {
                    let mut rtmp_subscriber =
                        FlvDataReceiver::new(app_name, stream_name, self.event_producer.clone(), 5);

                    tokio::spawn(async move {
                        if let Err(err) = rtmp_subscriber.run().await {
                            println!("hls handler run error {err}");
                        }
                    });
                }
                _ => {
                    log::trace!("other infos...");
                }
            }
        }
    }
}
