use crate::config::Config;
use rdkafka::{
    ClientConfig,
    client::ClientContext,
    consumer::{Consumer, StreamConsumer},
    consumer::{ConsumerContext, Rebalance},
};
#[derive(Default)]
pub struct EpochContext(pub std::sync::Mutex<u64>);
impl ClientContext for EpochContext {}
impl ConsumerContext for EpochContext {
    fn pre_rebalance(&self, _: &rdkafka::consumer::BaseConsumer<Self>, _: &Rebalance<'_>) {
        *self.0.lock().unwrap() += 1;
    }
}
fn client_config(config: &Config) -> ClientConfig {
    let mut client_config = ClientConfig::new();
    client_config
        .set("bootstrap.servers", &config.brokers)
        .set("security.protocol", &config.security_protocol);
    if let Some(mechanism) = &config.sasl_mechanism {
        client_config.set("sasl.mechanisms", mechanism);
    }
    if let Some(username) = &config.sasl_username {
        client_config.set("sasl.username", username);
    }
    if let Some(password) = &config.sasl_password {
        client_config.set("sasl.password", password);
    }
    if let Some(path) = &config.ssl_ca_location {
        client_config.set("ssl.ca.location", path);
    }
    client_config
}
pub fn create_consumer(config: &Config) -> Result<StreamConsumer<EpochContext>, String> {
    let consumer: StreamConsumer<EpochContext> = client_config(config)
        .set("group.id", &config.group_id)
        .set("enable.auto.commit", "false")
        .set("enable.auto.offset.store", "false")
        .set("queued.max.messages.kbytes", "1024")
        .set("queued.min.messages", "10")
        .set("auto.offset.reset", "latest")
        .set("max.poll.interval.ms", "300000")
        .set(
            "fetch.message.max.bytes",
            config.max_message_bytes.to_string(),
        )
        .create_with_context(EpochContext::default())
        .map_err(|error| format!("Failed to create Kafka consumer: {error}"))?;
    consumer
        .subscribe(&[&config.topic])
        .map_err(|error| format!("Failed to subscribe to replay topic: {error}"))?;
    Ok(consumer)
}
