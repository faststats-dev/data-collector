use crate::config::Config;
use rdkafka::{
    ClientConfig,
    consumer::{Consumer, StreamConsumer},
};
pub fn create_consumer(config: &Config) -> Result<StreamConsumer, String> {
    let mut client_config = ClientConfig::new();
    client_config
        .set("group.id", &config.group_id)
        .set("bootstrap.servers", &config.brokers)
        .set("enable.auto.commit", "false")
        .set("enable.auto.offset.store", "false")
        .set("auto.offset.reset", "latest")
        .set("max.poll.interval.ms", "86400000")
        .set(
            "fetch.message.max.bytes",
            config.max_message_bytes.to_string(),
        )
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
    let consumer: StreamConsumer = client_config
        .create()
        .map_err(|error| format!("Failed to create Kafka consumer: {error}"))?;
    consumer
        .subscribe(&[&config.topic])
        .map_err(|error| format!("Failed to subscribe to replay topic: {error}"))?;
    Ok(consumer)
}
