use a2::{
	Client, DefaultNotificationBuilder, Endpoint, NotificationBuilder,
	NotificationOptions, Priority, PushType,
};
use axum::extract::State as AxumState;
use futures_util::StreamExt;
use prometheus_client::metrics::counter::Counter as CounterMetric;
use prometheus_client::metrics::family::Family as MetricFamily;
use prometheus_client::metrics::info::Info as InfoMetric;
use prometheus_client::registry::Registry as MetricsRegistry;
use serde::{Deserialize, Deserializer};
use std::env;
use std::str::FromStr;
use std::sync::Arc;
use tokio::signal::ctrl_c;
use tokio_xmpp::{BareJid, Jid};
use url::{Url, Host};

const VERSION: &str = git_version::git_version!();

#[derive(Deserialize)]
struct EndpointConfig {
	p8: String,
	kid: String
}

#[derive(Deserialize)]
struct Config {
	dev: EndpointConfig,
	prod: EndpointConfig,
	team_id: String,
	#[serde(deserialize_with = "deserialize_jid")]
	jid: BareJid,
	password: String,
	listen: String,
}

fn deserialize_jid<'de, D>(deserializer: D) -> Result<BareJid, D::Error>
where
	D: Deserializer<'de>,
{
	let s: String = Deserialize::deserialize(deserializer)?;
	BareJid::from_str(&s)
		.map_err(|_| serde::de::Error::custom(format!("Invalid JID: {}", s)))
}

fn apns_priority(s: String) -> Priority {
	return match s.as_str() {
		"high" => Priority::High,
		_ => Priority::Normal,
	};
}

async fn metrics_handler(
	AxumState(registry): AxumState<Arc<MetricsRegistry>>,
) -> impl axum::response::IntoResponse {
	let mut buffer = String::new();
	let _ = prometheus_client::encoding::text::encode(&mut buffer, &registry);
	([(axum::http::header::CONTENT_TYPE, "text/plain")], buffer)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	let mut args = env::args();

	// Skip the program name
	args.next();

	let config: Config =
		serde_dhall::from_str(&args.next().ok_or("No config path provided")?)
			.parse()?;

	let mut registry = <MetricsRegistry>::default();
	registry.register(
		"build",
		"Info about the build that is running",
		InfoMetric::new(vec![("version", VERSION)]),
	);

	let mut buffer = String::new();
	prometheus_client::encoding::text::encode(&mut buffer, &registry)?;
	println!("{}\n", buffer);

	let apns_success: CounterMetric = CounterMetric::default();
	registry.register(
		"apns_success",
		"Number of successful APNS pushes",
		apns_success.clone(),
	);

	let apns_failure =
		MetricFamily::<Vec<(&str, String)>, CounterMetric>::default();
	registry.register(
		"apns_failure",
		"Number of failed APNS pushes",
		apns_failure.clone(),
	);

	let dev_client = Client::token(
		std::io::Cursor::new(config.dev.p8),
		config.dev.kid,
		config.team_id.clone(),
		Endpoint::Sandbox,
	)?;

	let prod_client = Client::token(
		std::io::Cursor::new(config.prod.p8),
		config.prod.kid,
		config.team_id,
		Endpoint::Production,
	)?;

	let mut client = tokio_xmpp::AsyncClient::new(config.jid, config.password);
	client.set_reconnect(true);

	let app = axum::Router::new()
		.route("/metrics", axum::routing::get(metrics_handler))
		.with_state(Arc::new(registry));

	let addr = config.listen.parse()?;
	println!("Listening on {}", addr);
	tokio::spawn(async move {
		axum::Server::bind(&addr)
			.serve(app.into_make_service())
			.await
			.unwrap()
	});

	loop {
		tokio::select! {
			events = client.next() => {
				for event in events {
					if event.is_online() {
						client.send_stanza(
							xmpp_parsers::presence::Presence::new(xmpp_parsers::presence::Type::None).into()
						).await?;
					}
					if let Some(stanza) = event.into_stanza() {
						if stanza.name() != "message" { continue; }
						if let Some(notif) = stanza.children().find(|el| el.is("notification", "urn:xmpp:push2:0")) {
							if let Some(url) = notif.children().find(|el| el.is("client", "urn:xmpp:push2:0")).and_then(|el| Url::parse(&el.text()).ok()) {
								let priority = notif.children().find(|el| el.is("priority", "urn:xmpp:push2:0")).map(|el| el.text());
								let data = notif.children().find(|el| el.is("encrypted", "urn:xmpp:sce:rfc8291:0")).and_then(|encrypted|
									encrypted.children().find(|el| el.is("payload", "urn:xmpp:sce:rfc8291:0")).map(|el| el.text())
								);
								let voip = notif.children().find(|el| el.is("voip", "urn:xmpp:push2:0")).is_some();
								let mut topic_and_voip_token = url.fragment().unwrap_or("").split("&");
								let topic = topic_and_voip_token.next();
								let voip_token = topic_and_voip_token.next();

								let apns = if url.host() == Some(Host::Domain("api.development.push.apple.com")) {
									&dev_client
								} else {
									&prod_client
								};

								let wrapped_token = if voip {
									voip_token
								} else {
									url.path_segments().and_then(|segs| segs.last())
								};

								let final_topic = if voip {
									topic.map(|base| base.to_owned() + ".voip")
								} else {
									topic.map(|base| base.to_owned())
								};

								if let Some(device_token) = wrapped_token {
									let mut payload = DefaultNotificationBuilder::new()
										.set_title("New Message")
										.set_mutable_content()
										.build(
											device_token,
											NotificationOptions {
												apns_id: stanza.attr("id"),
												apns_collapse_id: None,
												apns_expiration: None,
												apns_topic: final_topic.as_deref(),
												apns_push_type: Some(if voip { PushType::Voip } else { PushType::Alert }),
												apns_priority: priority.map(apns_priority)
											}
										);
									data.map(|dat| payload.add_custom_data("rfc8291", &dat));
									// TODO: Do not send more notifications with tokens that return Unregistered, BadDeviceToken or DeviceTokenNotForTopic.
									match apns.send(payload).await {
										Ok(_) => {
											apns_success.inc();
										}
										Err(a2::Error::ResponseError(r)) => {
											println!("Error from APNS: {:?}", &r);
											client.send_stanza(
												xmpp_parsers::message::Message::error(stanza.attr("from").and_then(|s| Jid::new(s).ok()))
												.with_payload(xmpp_parsers::stanza_error::StanzaError::new(
													xmpp_parsers::stanza_error::ErrorType::Cancel,
													xmpp_parsers::stanza_error::DefinedCondition::ServiceUnavailable,
													"en",
													format!("Got error response from APNS: {:?}", &r)
												)).into()
											).await?;
											apns_failure.get_or_create(&vec![
												("code", r.code.to_string()),
												("reason", r.error.map(|err| err.reason.to_string()).unwrap_or("None".to_string()))
											]).inc();
										}
										Err(err) => {
											println!("Could not send: {:?}", &err);
											client.send_stanza(
												xmpp_parsers::message::Message::error(stanza.attr("from").and_then(|s| Jid::new(s).ok()))
												.with_payload(xmpp_parsers::stanza_error::StanzaError::new(
													xmpp_parsers::stanza_error::ErrorType::Cancel,
													xmpp_parsers::stanza_error::DefinedCondition::ServiceUnavailable,
													"en",
													format!("Got error sending to APNS: {:?}", &err)
												)).into()
											).await?;
											apns_failure.get_or_create(&vec![
												("reason", err.to_string())
											]).inc();
										}
									}
								}
							}
						}
					}
				}
			},
			_ = ctrl_c() => {
				client.set_reconnect(false);
				client.send_end().await?;
				break;
			},
		}
	}

	return Ok(());
}
